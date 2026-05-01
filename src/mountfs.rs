use crate::config::{AppConfig, LocalConfig, RemoteConfig, SyncConfig, parse_remote};
use crate::error::Result;
use crate::remote::RemoteClient;
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use fuser::{
    Config, Errno, FileAttr, FileType, Filesystem, FopenFlags, INodeNo, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
    TimeOrNow,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);

pub fn mount(config: AppConfig, mountpoint: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&mountpoint)?;
    let fs = MobfsFuse::new(config)?;
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RW,
        MountOption::FSName("mobfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    fuser::mount2(fs, mountpoint, &config)?;
    Ok(())
}

pub fn config_from_remote(
    remote: String,
    mountpoint: &Path,
    port: u16,
    token: Option<String>,
    ssh_tunnel: bool,
) -> Result<AppConfig> {
    let target = parse_remote(&remote)?;
    Ok(AppConfig {
        remote: RemoteConfig {
            backend: target.backend,
            host: target.host,
            user: String::new(),
            path: target.path,
            port,
            identity: None,
            ssh_tunnel,
            token: Some(
                token
                    .or_else(|| std::env::var("MOBFS_TOKEN").ok())
                    .unwrap_or_else(crate::config::generate_token),
            ),
        },
        local: LocalConfig {
            root: mountpoint.to_path_buf(),
        },
        sync: SyncConfig {
            ignore: vec![
                ".mobfs".to_string(),
                "target".to_string(),
                "node_modules".to_string(),
                ".mobfs.toml".to_string(),
                ".DS_Store".to_string(),
                ".mobfs-mountfs-journal.jsonl".to_string(),
            ],
            connect_retries: crate::config::DEFAULT_CONNECT_RETRIES,
            operation_retries: crate::config::DEFAULT_OP_RETRIES,
        },
    })
}

struct MobfsFuse {
    client: Mutex<RemoteClient>,
    snapshot: Mutex<Snapshot>,
    path_to_ino: Mutex<BTreeMap<String, u64>>,
    ino_to_path: Mutex<BTreeMap<u64, String>>,
    read_cache: Mutex<BTreeMap<(String, u64, u32), Vec<u8>>>,
    journal: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum JournalOp {
    Truncate {
        path: String,
        size: u64,
    },
    SetMetadata {
        path: String,
        mode: Option<u32>,
        modified: Option<i64>,
    },
    Mkdir {
        path: String,
    },
    Symlink {
        path: String,
        target: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Remove {
        path: String,
        dir: bool,
    },
}

impl MobfsFuse {
    fn new(config: AppConfig) -> Result<Self> {
        let journal = config.local.root.join(".mobfs-mountfs-journal.jsonl");
        let mut client = RemoteClient::connect(config)?;
        replay_journal(&journal, &mut client)?;
        let snapshot = client.snapshot()?;
        let mut path_to_ino = BTreeMap::new();
        let mut ino_to_path = BTreeMap::new();
        path_to_ino.insert(String::new(), 1);
        ino_to_path.insert(1, String::new());
        for path in snapshot.entries.keys() {
            let ino = inode_for(path);
            path_to_ino.insert(path.clone(), ino);
            ino_to_path.insert(ino, path.clone());
        }
        Ok(Self {
            client: Mutex::new(client),
            snapshot: Mutex::new(snapshot),
            path_to_ino: Mutex::new(path_to_ino),
            ino_to_path: Mutex::new(ino_to_path),
            read_cache: Mutex::new(BTreeMap::new()),
            journal,
        })
    }

    fn refresh(&self) -> Result<()> {
        let snapshot = self.client.lock().unwrap().snapshot()?;
        let mut path_to_ino = BTreeMap::new();
        let mut ino_to_path = BTreeMap::new();
        path_to_ino.insert(String::new(), 1);
        ino_to_path.insert(1, String::new());
        for path in snapshot.entries.keys() {
            let ino = inode_for(path);
            path_to_ino.insert(path.clone(), ino);
            ino_to_path.insert(ino, path.clone());
        }
        *self.snapshot.lock().unwrap() = snapshot;
        *self.path_to_ino.lock().unwrap() = path_to_ino;
        *self.ino_to_path.lock().unwrap() = ino_to_path;
        Ok(())
    }

    fn invalidate_path_cache(&self, path: &str) {
        self.read_cache
            .lock()
            .unwrap()
            .retain(|(cached, _, _), _| cached != path && !cached.starts_with(&format!("{path}/")));
    }

    fn refresh_or_eio(&self) -> std::result::Result<(), Errno> {
        self.refresh().map_err(|_| Errno::EIO)
    }

    fn record(&self, op: &JournalOp) -> std::result::Result<(), Errno> {
        append_journal(&self.journal, op).map_err(|_| Errno::EIO)
    }

    fn clear_record(&self) -> std::result::Result<(), Errno> {
        clear_journal(&self.journal).map_err(|_| Errno::EIO)
    }

    fn update_entry(&self, path: &str, meta: EntryMeta) {
        let ino = inode_for(path);
        self.snapshot
            .lock()
            .unwrap()
            .entries
            .insert(path.to_string(), meta);
        self.path_to_ino
            .lock()
            .unwrap()
            .insert(path.to_string(), ino);
        self.ino_to_path
            .lock()
            .unwrap()
            .insert(ino, path.to_string());
    }

    fn remove_entry(&self, path: &str) {
        self.snapshot.lock().unwrap().entries.remove(path);
        if let Some(ino) = self.path_to_ino.lock().unwrap().remove(path) {
            self.ino_to_path.lock().unwrap().remove(&ino);
        }
    }

    fn path_for(&self, ino: INodeNo) -> Option<String> {
        self.ino_to_path
            .lock()
            .unwrap()
            .get(&u64::from(ino))
            .cloned()
    }

    fn attr_for(&self, path: &str, meta: Option<&EntryMeta>) -> FileAttr {
        let kind = meta.map(|m| &m.kind).unwrap_or(&EntryKind::Dir);
        let size = meta.map(|m| m.size).unwrap_or(0);
        let modified = meta.map(|m| m.modified).unwrap_or(0).max(0) as u64;
        let time = UNIX_EPOCH + Duration::from_secs(modified);
        FileAttr {
            ino: INodeNo(inode_for(path)),
            size,
            blocks: size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: match kind {
                EntryKind::File => FileType::RegularFile,
                EntryKind::Dir => FileType::Directory,
                EntryKind::Symlink => FileType::Symlink,
            },
            perm: if meta.map(|m| m.mode).unwrap_or(0) != 0 {
                meta.map(|m| m.mode as u16).unwrap_or(0o755)
            } else {
                match kind {
                    EntryKind::File => 0o644,
                    EntryKind::Dir => 0o755,
                    EntryKind::Symlink => 0o777,
                }
            },
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }
}

impl Filesystem for MobfsFuse {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_path = match self.path_for(parent) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let name = match name.to_str() {
            Some(name) => name,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let path = join_rel(&parent_path, name);
        match self.client.lock().unwrap().stat(&path) {
            Ok(Some(meta)) => {
                self.update_entry(&path, meta.clone());
                reply.entry(
                    &TTL,
                    &self.attr_for(&path, Some(&meta)),
                    fuser::Generation(0),
                );
            }
            Ok(None) => {
                self.remove_entry(&path);
                reply.error(Errno::ENOENT);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        if u64::from(ino) == 1 {
            reply.attr(&TTL, &self.attr_for("", None));
            return;
        }
        let path = match self.path_for(ino) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        match self.client.lock().unwrap().stat(&path) {
            Ok(Some(meta)) => {
                self.update_entry(&path, meta.clone());
                reply.attr(&TTL, &self.attr_for(&path, Some(&meta)));
            }
            Ok(None) => {
                self.remove_entry(&path);
                reply.error(Errno::ENOENT);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let dir = match self.path_for(ino) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let remote_entries = match self.client.lock().unwrap().list_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let mut entries = vec![(u64::from(ino), FileType::Directory, ".".to_string())];
        entries.push((1, FileType::Directory, "..".to_string()));
        for (name, meta) in remote_entries {
            let path = join_rel(&dir, &name);
            let kind = match meta.kind {
                EntryKind::File => FileType::RegularFile,
                EntryKind::Dir => FileType::Directory,
                EntryKind::Symlink => FileType::Symlink,
            };
            self.update_entry(&path, meta);
            entries.push((inode_for(&path), kind, name));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        if self.path_for(ino).is_some() {
            reply.opened(fuser::FileHandle(u64::from(ino)), FopenFlags::empty());
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let path = match self.path_for(ino) {
            Some(path) if !path.is_empty() => path,
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        match self.client.lock().unwrap().stat(&path) {
            Ok(Some(meta)) => self.update_entry(&path, meta),
            Ok(None) => {
                self.remove_entry(&path);
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot
            .entries
            .get(&path)
            .and_then(|meta| meta.link_target.as_ref())
        {
            Some(target) => reply.data(target.as_bytes()),
            None => reply.error(Errno::EINVAL),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let path = match self.path_for(ino) {
            Some(path) if !path.is_empty() => path,
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let key = (path.clone(), offset, size);
        match self
            .client
            .lock()
            .unwrap()
            .read_file_chunk(&path, offset, size as u64)
        {
            Ok((data, _)) => {
                self.read_cache.lock().unwrap().insert(key, data.clone());
                reply.data(&data);
            }
            Err(_) => match self.read_cache.lock().unwrap().get(&key) {
                Some(data) => reply.data(data),
                None => reply.error(Errno::EIO),
            },
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let path = match self.path_for(ino) {
            Some(path) if !path.is_empty() => path,
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        match self
            .client
            .lock()
            .unwrap()
            .write_file_at(&path, offset, data.to_vec())
        {
            Ok(()) => {
                self.invalidate_path_cache(&path);
                match self.refresh_or_eio() {
                    Ok(()) => reply.written(data.len() as u32),
                    Err(errno) => reply.error(errno),
                }
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        if self.path_for(ino).is_some() {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        if self.path_for(ino).is_some() {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<fuser::FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let path = match self.path_for(ino) {
            Some(path) if !path.is_empty() => path,
            Some(_) => {
                reply.attr(&TTL, &self.attr_for("", None));
                return;
            }
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        if let Some(size) = size {
            if self
                .record(&JournalOp::Truncate {
                    path: path.clone(),
                    size,
                })
                .is_err()
            {
                reply.error(Errno::EIO);
                return;
            }
            if self.client.lock().unwrap().truncate(&path, size).is_err() {
                reply.error(Errno::EIO);
                return;
            }
            self.invalidate_path_cache(&path);
            if let Err(errno) = self.clear_record().and_then(|_| self.refresh_or_eio()) {
                reply.error(errno);
                return;
            }
        }
        let modified = mtime.and_then(time_or_now_secs);
        if (mode.is_some() || modified.is_some())
            && self
                .record(&JournalOp::SetMetadata {
                    path: path.clone(),
                    mode,
                    modified,
                })
                .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        if (mode.is_some() || modified.is_some())
            && (self
                .client
                .lock()
                .unwrap()
                .set_metadata(&path, mode, modified)
                .is_err())
        {
            reply.error(Errno::EIO);
            return;
        }
        if (mode.is_some() || modified.is_some())
            && let Err(errno) = self.clear_record().and_then(|_| self.refresh_or_eio())
        {
            reply.error(errno);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.attr(&TTL, &self.attr_for(&path, Some(meta))),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent_path = match self.path_for(parent) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let name = match name.to_str() {
            Some(name) => name,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let path = join_rel(&parent_path, name);
        if self
            .record(&JournalOp::Truncate {
                path: path.clone(),
                size: 0,
            })
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        if self.client.lock().unwrap().truncate(&path, 0).is_err()
            || self
                .client
                .lock()
                .unwrap()
                .set_metadata(&path, Some(mode), None)
                .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        self.invalidate_path_cache(&path);
        if let Err(errno) = self.clear_record().and_then(|_| self.refresh_or_eio()) {
            reply.error(errno);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.created(
                &TTL,
                &self.attr_for(&path, Some(meta)),
                fuser::Generation(0),
                fuser::FileHandle(inode_for(&path)),
                FopenFlags::empty(),
            ),
            None => reply.error(Errno::EIO),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent_path = match self.path_for(parent) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let name = match name.to_str() {
            Some(name) => name,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let path = join_rel(&parent_path, name);
        if self
            .record(&JournalOp::Mkdir { path: path.clone() })
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        if self.client.lock().unwrap().mkdir_p(&path).is_err()
            || self
                .client
                .lock()
                .unwrap()
                .set_metadata(&path, Some(mode), None)
                .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        if let Err(errno) = self.clear_record().and_then(|_| self.refresh_or_eio()) {
            reply.error(errno);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.entry(
                &TTL,
                &self.attr_for(&path, Some(meta)),
                fuser::Generation(0),
            ),
            None => reply.error(Errno::EIO),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let parent_path = match self.path_for(parent) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let Some(name) = link_name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let Some(target) = target.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        if self
            .record(&JournalOp::Symlink {
                path: path.clone(),
                target: target.to_string(),
            })
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        if self
            .client
            .lock()
            .unwrap()
            .create_symlink(&path, target)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        self.invalidate_path_cache(&path);
        if let Err(errno) = self.clear_record().and_then(|_| self.refresh_or_eio()) {
            reply.error(errno);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.entry(
                &TTL,
                &self.attr_for(&path, Some(meta)),
                fuser::Generation(0),
            ),
            None => reply.error(Errno::EIO),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_child(parent, name, false, reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_child(parent, name, true, reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let from_parent = match self.path_for(parent) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let to_parent = match self.path_for(newparent) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let Some(newname) = newname.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let from = join_rel(&from_parent, name);
        let to = join_rel(&to_parent, newname);
        if self
            .record(&JournalOp::Rename {
                from: from.clone(),
                to: to.clone(),
            })
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        match self.client.lock().unwrap().rename(&from, &to) {
            Ok(()) => {
                self.invalidate_path_cache(&from);
                self.invalidate_path_cache(&to);
                match self.clear_record().and_then(|_| self.refresh_or_eio()) {
                    Ok(()) => reply.ok(),
                    Err(errno) => reply.error(errno),
                }
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }
}

impl MobfsFuse {
    fn remove_child(&self, parent: INodeNo, name: &OsStr, dir: bool, reply: ReplyEmpty) {
        let parent_path = match self.path_for(parent) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let name = match name.to_str() {
            Some(name) => name,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let path = join_rel(&parent_path, name);
        let meta = EntryMeta {
            kind: if dir { EntryKind::Dir } else { EntryKind::File },
            size: 0,
            modified: 0,
            sha256: None,
            mode: 0,
            link_target: None,
        };
        if self
            .record(&JournalOp::Remove {
                path: path.clone(),
                dir,
            })
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        match self.client.lock().unwrap().remove(&path, &meta) {
            Ok(()) => {
                self.invalidate_path_cache(&path);
                match self.clear_record().and_then(|_| self.refresh_or_eio()) {
                    Ok(()) => reply.ok(),
                    Err(errno) => reply.error(errno),
                }
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }
}

fn append_journal(path: &Path, op: &JournalOp) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string(op)?))?;
    Ok(())
}

fn clear_journal(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn replay_journal(path: &Path, client: &mut RemoteClient) -> Result<()> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for line in data.lines().filter(|line| !line.trim().is_empty()) {
        let op: JournalOp = serde_json::from_str(line)?;
        apply_journal_op(client, &op)?;
    }
    clear_journal(path)
}

fn apply_journal_op(client: &mut RemoteClient, op: &JournalOp) -> Result<()> {
    match op {
        JournalOp::Truncate { path, size } => client.truncate(path, *size),
        JournalOp::SetMetadata {
            path,
            mode,
            modified,
        } => client.set_metadata(path, *mode, *modified),
        JournalOp::Mkdir { path } => client.mkdir_p(path),
        JournalOp::Symlink { path, target } => client.create_symlink(path, target),
        JournalOp::Rename { from, to } => client.rename(from, to),
        JournalOp::Remove { path, dir } => {
            let meta = EntryMeta {
                kind: if *dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                size: 0,
                modified: 0,
                sha256: None,
                mode: 0,
                link_target: None,
            };
            client.remove(path, &meta)
        }
    }
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn time_or_now_secs(value: TimeOrNow) -> Option<i64> {
    let time = match value {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    };
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

fn inode_for(path: &str) -> u64 {
    if path.is_empty() {
        return 1;
    }
    let mut hash = 1469598103934665603_u64;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    hash.max(2)
}
