use crate::config::{AppConfig, LocalConfig, RemoteConfig, SyncConfig, parse_remote};
use crate::error::Result;
use crate::remote::RemoteClient;
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use fuser::{
    Config, Errno, FileAttr, FileType, Filesystem, FopenFlags, INodeNo, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

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
            token: token.or_else(|| std::env::var("MOBFS_TOKEN").ok()),
        },
        local: LocalConfig {
            root: mountpoint.to_path_buf(),
        },
        sync: SyncConfig {
            ignore: vec![
                ".mobfs".to_string(),
                ".git".to_string(),
                "target".to_string(),
                "node_modules".to_string(),
                ".mobfs.toml".to_string(),
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
}

impl MobfsFuse {
    fn new(config: AppConfig) -> Result<Self> {
        let mut client = RemoteClient::connect(config)?;
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
            },
            perm: match kind {
                EntryKind::File => 0o644,
                EntryKind::Dir => 0o755,
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
        if self.refresh().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.entry(
                &TTL,
                &self.attr_for(&path, Some(meta)),
                fuser::Generation(0),
            ),
            None => reply.error(Errno::ENOENT),
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
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.attr(&TTL, &self.attr_for(&path, Some(meta))),
            None => reply.error(Errno::ENOENT),
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
        if self.refresh().is_err() {
            reply.error(Errno::EIO);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        let mut entries = vec![(u64::from(ino), FileType::Directory, ".".to_string())];
        entries.push((1, FileType::Directory, "..".to_string()));
        for (path, meta) in &snapshot.entries {
            if parent_of(path) == dir {
                let name = path.rsplit('/').next().unwrap_or(path).to_string();
                let kind = match meta.kind {
                    EntryKind::File => FileType::RegularFile,
                    EntryKind::Dir => FileType::Directory,
                };
                entries.push((inode_for(path), kind, name));
            }
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
        match self
            .client
            .lock()
            .unwrap()
            .read_file_chunk(&path, offset, size as u64)
        {
            Ok((data, _)) => reply.data(&data),
            Err(_) => reply.error(Errno::EIO),
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
                let _ = self.refresh();
                reply.written(data.len() as u32);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<fuser::FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
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
        if let Some(size) = size
            && (self.client.lock().unwrap().truncate(&path, size).is_err()
                || self.refresh().is_err())
        {
            reply.error(Errno::EIO);
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
        _mode: u32,
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
        if self.client.lock().unwrap().truncate(&path, 0).is_err() || self.refresh().is_err() {
            reply.error(Errno::EIO);
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
        _mode: u32,
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
        if self.client.lock().unwrap().mkdir_p(&path).is_err() || self.refresh().is_err() {
            reply.error(Errno::EIO);
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
        match self.client.lock().unwrap().rename(&from, &to) {
            Ok(()) => {
                let _ = self.refresh();
                reply.ok();
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
        };
        match self.client.lock().unwrap().remove(&path, &meta) {
            Ok(()) => {
                let _ = self.refresh();
                reply.ok();
            }
            Err(_) => reply.error(Errno::EIO),
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

fn parent_of(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
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
