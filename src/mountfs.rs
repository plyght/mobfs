use crate::config::{AppConfig, LocalConfig, RemoteConfig, SyncConfig, parse_remote};
use crate::error::Result;
use crate::remote::RemoteClient;
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use fuser::{
    Config, Errno, FileAttr, FileType, Filesystem, FopenFlags, INodeNo, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock, ReplyOpen,
    ReplyWrite, ReplyXattr, Request, TimeOrNow,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn mount(config: AppConfig, mountpoint: PathBuf) -> Result<()> {
    prepare_mountpoint(&mountpoint)?;
    let ttl = Duration::from_secs(config.sync.cache_ttl_secs.min(60));
    let fs = MobfsFuse::new(config, ttl)?;
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RW,
        MountOption::FSName("mobfs".to_string()),
        MountOption::Subtype("mobfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    fuser::mount2(fs, mountpoint, &config).map_err(|error| {
        crate::error::MobfsError::Remote(format!(
            "failed to mount FUSE filesystem: {error}. On macOS, install macFUSE and allow its system extension in System Settings if prompted"
        ))
    })?;
    Ok(())
}

pub fn prepare_mountpoint(mountpoint: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if !Path::new("/Library/Filesystems/macfuse.fs").exists() {
        return Err(crate::error::MobfsError::Config(
            "macFUSE is not installed; install macFUSE, approve its system extension, then retry `mobfs mount`".to_string(),
        ));
    }
    if mountpoint.exists() && !mountpoint.is_dir() {
        return Err(crate::error::MobfsError::Config(format!(
            "mountpoint {} exists but is not a directory",
            mountpoint.display()
        )));
    }
    std::fs::create_dir_all(mountpoint)?;
    let mut unexpected = Vec::new();
    for item in std::fs::read_dir(mountpoint)? {
        let item = item?;
        let name = item.file_name();
        if name != ".DS_Store" && name != ".localized" {
            unexpected.push(name.to_string_lossy().to_string());
        }
    }
    if !unexpected.is_empty() {
        return Err(crate::error::MobfsError::Config(format!(
            "mountpoint {} is not empty; unmount or choose a clean directory. Unexpected entries: {}",
            mountpoint.display(),
            unexpected.join(", ")
        )));
    }
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
                "._*".to_string(),
                ".mobfs-mountfs-journal.jsonl".to_string(),
            ],
            connect_retries: crate::config::DEFAULT_CONNECT_RETRIES,
            operation_retries: crate::config::DEFAULT_OP_RETRIES,
            cache_ttl_secs: 1,
        },
    })
}

type DirEntries = Vec<(String, EntryMeta)>;
type DirCache = BTreeMap<String, (Instant, DirEntries)>;

struct MobfsFuse {
    client: Mutex<RemoteClient>,
    snapshot: Mutex<Snapshot>,
    path_to_ino: Mutex<BTreeMap<String, u64>>,
    ino_to_path: Mutex<BTreeMap<u64, String>>,
    read_cache: Mutex<BTreeMap<(String, u64, u32), Vec<u8>>>,
    file_cache: Mutex<BTreeMap<String, Vec<u8>>>,
    dir_cache: Mutex<DirCache>,
    handle_to_path: Mutex<BTreeMap<u64, String>>,
    journal: PathBuf,
    ttl: Duration,
    ignore: Vec<String>,
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
    fn new(config: AppConfig, ttl: Duration) -> Result<Self> {
        let journal = mountfs_journal_path(&config);
        let ignore = config.sync.ignore.clone();
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
        let dir_cache = dir_cache_from_snapshot(&snapshot);
        Ok(Self {
            ignore,
            client: Mutex::new(client),
            snapshot: Mutex::new(snapshot),
            path_to_ino: Mutex::new(path_to_ino),
            ino_to_path: Mutex::new(ino_to_path),
            read_cache: Mutex::new(BTreeMap::new()),
            file_cache: Mutex::new(BTreeMap::new()),
            dir_cache: Mutex::new(dir_cache),
            handle_to_path: Mutex::new(BTreeMap::new()),
            journal,
            ttl,
        })
    }

    fn invalidate_path_cache(&self, path: &str) {
        self.read_cache
            .lock()
            .unwrap()
            .retain(|(cached, _, _), _| cached != path && !cached.starts_with(&format!("{path}/")));
        self.file_cache
            .lock()
            .unwrap()
            .retain(|cached, _| cached != path && !cached.starts_with(&format!("{path}/")));
        self.dir_cache.lock().unwrap().retain(|cached, _| {
            cached != path
                && !path.starts_with(&format!("{cached}/"))
                && !cached.starts_with(&format!("{path}/"))
        });
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

    fn fresh_meta(&self, path: &str) -> Option<EntryMeta> {
        self.snapshot.lock().unwrap().entries.get(path).cloned()
    }

    fn ignored_rel(&self, path: &str) -> bool {
        path.split('/')
            .any(|part| crate::local::should_ignore_part(part, &self.ignore))
    }

    fn update_file_write(&self, path: &str, offset: u64, len: usize) {
        let mut snapshot = self.snapshot.lock().unwrap();
        let entry = snapshot
            .entries
            .entry(path.to_string())
            .or_insert_with(|| EntryMeta {
                kind: EntryKind::File,
                size: 0,
                modified: unix_now_secs(),
                sha256: None,
                mode: 0o644,
                link_target: None,
            });
        entry.kind = EntryKind::File;
        entry.size = entry.size.max(offset.saturating_add(len as u64));
        entry.modified = unix_now_secs();
        entry.sha256 = None;
        drop(snapshot);
        let ino = inode_for(path);
        self.path_to_ino
            .lock()
            .unwrap()
            .insert(path.to_string(), ino);
        self.ino_to_path
            .lock()
            .unwrap()
            .insert(ino, path.to_string());
    }

    fn update_dir_entry(&self, path: &str, mode: u32) {
        self.update_entry(
            path,
            EntryMeta {
                kind: EntryKind::Dir,
                size: 0,
                modified: 0,
                sha256: None,
                mode,
                link_target: None,
            },
        );
    }

    fn update_symlink_entry(&self, path: &str, target: &str) {
        self.update_entry(
            path,
            EntryMeta {
                kind: EntryKind::Symlink,
                size: target.len() as u64,
                modified: unix_now_secs(),
                sha256: None,
                mode: 0o777,
                link_target: Some(target.to_string()),
            },
        );
    }

    fn update_file_size(&self, path: &str, size: u64) {
        let mut snapshot = self.snapshot.lock().unwrap();
        let entry = snapshot
            .entries
            .entry(path.to_string())
            .or_insert_with(|| EntryMeta {
                kind: EntryKind::File,
                size,
                modified: unix_now_secs(),
                sha256: None,
                mode: 0o644,
                link_target: None,
            });
        entry.kind = EntryKind::File;
        entry.size = size;
        entry.modified = unix_now_secs();
        entry.sha256 = None;
        drop(snapshot);
        let ino = inode_for(path);
        self.path_to_ino
            .lock()
            .unwrap()
            .insert(path.to_string(), ino);
        self.ino_to_path
            .lock()
            .unwrap()
            .insert(ino, path.to_string());
    }

    fn apply_metadata_update(&self, path: &str, mode: Option<u32>, modified: Option<i64>) {
        if let Some(entry) = self.snapshot.lock().unwrap().entries.get_mut(path) {
            if let Some(mode) = mode {
                entry.mode = mode;
            }
            if let Some(modified) = modified {
                entry.modified = modified;
            }
        }
    }

    fn cached_dir_entries(&self, dir: &str) -> Option<DirEntries> {
        let cache = self.dir_cache.lock().unwrap();
        let (created, entries) = cache.get(dir)?;
        if self.ttl.is_zero() || created.elapsed() <= self.ttl {
            Some(entries.clone())
        } else {
            None
        }
    }

    fn cache_dir_entries(&self, dir: &str, entries: &[(String, EntryMeta)]) {
        self.dir_cache
            .lock()
            .unwrap()
            .insert(dir.to_string(), (Instant::now(), entries.to_vec()));
    }

    fn remove_entry(&self, path: &str) {
        self.snapshot.lock().unwrap().entries.remove(path);
        if let Some(ino) = self.path_to_ino.lock().unwrap().remove(path) {
            self.ino_to_path.lock().unwrap().remove(&ino);
        }
    }

    fn remove_tree_entries(&self, path: &str) {
        let prefix = format!("{path}/");
        let paths = self
            .snapshot
            .lock()
            .unwrap()
            .entries
            .keys()
            .filter(|entry| *entry == path || entry.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        for path in paths {
            self.remove_entry(&path);
        }
    }

    fn rename_tree_entries(&self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let entries = self
            .snapshot
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|(path, _)| *path == from || path.starts_with(&prefix))
            .map(|(path, meta)| (path.clone(), meta.clone()))
            .collect::<Vec<_>>();
        for (old_path, meta) in entries {
            let suffix = old_path.strip_prefix(from).unwrap_or("");
            self.remove_entry(&old_path);
            self.update_entry(&format!("{to}{suffix}"), meta);
        }
    }

    fn path_for(&self, ino: INodeNo) -> Option<String> {
        self.ino_to_path
            .lock()
            .unwrap()
            .get(&u64::from(ino))
            .cloned()
    }

    fn path_for_handle(&self, ino: INodeNo, fh: fuser::FileHandle) -> Option<String> {
        self.path_for(ino).or_else(|| {
            self.handle_to_path
                .lock()
                .unwrap()
                .get(&u64::from(fh))
                .cloned()
        })
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
        if self.ignored_rel(&path) {
            reply.error(Errno::ENOENT);
            return;
        }
        if let Some(meta) = self.fresh_meta(&path) {
            reply.entry(
                &self.ttl,
                &self.attr_for(&path, Some(&meta)),
                fuser::Generation(0),
            );
            return;
        }
        match self.client.lock().unwrap().stat(&path) {
            Ok(Some(meta)) => {
                self.update_entry(&path, meta.clone());
                reply.entry(
                    &self.ttl,
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
            reply.attr(&self.ttl, &self.attr_for("", None));
            return;
        }
        let path = match self.path_for(ino) {
            Some(path) => path,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        if let Some(meta) = self.fresh_meta(&path) {
            reply.attr(&self.ttl, &self.attr_for(&path, Some(&meta)));
            return;
        }
        match self.client.lock().unwrap().stat(&path) {
            Ok(Some(meta)) => {
                self.update_entry(&path, meta.clone());
                reply.attr(&self.ttl, &self.attr_for(&path, Some(&meta)));
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
        let remote_entries = match self.cached_dir_entries(&dir) {
            Some(entries) => entries,
            None => match self.client.lock().unwrap().list_dir(&dir) {
                Ok(entries) => {
                    self.cache_dir_entries(&dir, &entries);
                    entries
                }
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            },
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
        if let Some(path) = self.path_for(ino) {
            self.handle_to_path
                .lock()
                .unwrap()
                .insert(u64::from(ino), path);
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
        if self.fresh_meta(&path).is_none() {
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
        if let Some(meta) = self.fresh_meta(&path)
            && meta.kind == EntryKind::File
            && meta.size <= 1024 * 1024
        {
            if let Some(data) = self.file_cache.lock().unwrap().get(&path).cloned() {
                reply.data(slice_read(&data, offset, size));
                return;
            }
            match self
                .client
                .lock()
                .unwrap()
                .read_file_chunk(&path, 0, meta.size)
            {
                Ok((data, _)) => {
                    let chunk = slice_read(&data, offset, size).to_vec();
                    self.file_cache.lock().unwrap().insert(path.clone(), data);
                    reply.data(&chunk);
                    return;
                }
                Err(_) => match self.read_cache.lock().unwrap().get(&key) {
                    Some(data) => {
                        reply.data(data);
                        return;
                    }
                    None => {}
                },
            }
        }
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
        fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let path = match self.path_for_handle(ino, fh) {
            Some(path) if !path.is_empty() => path,
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        if self.ignored_rel(&path) {
            reply.error(Errno::EACCES);
            return;
        }
        let write_result = {
            self.client
                .lock()
                .unwrap()
                .write_file_at(&path, offset, data.to_vec())
        };
        match write_result {
            Ok(()) => {
                self.invalidate_path_cache(&path);
                self.update_file_write(&path, offset, data.len());
                reply.written(data.len() as u32);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.path_for_handle(ino, fh) {
            Some(_) => reply.ok(),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: fuser::FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.path_for_handle(ino, fh) {
            Some(path) => match self.client.lock().unwrap().fsync(&path) {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(Errno::EIO),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOTSUP);
    }

    fn getlk(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        reply.locked(start, end, typ, pid);
    }

    fn setlk(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
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
                reply.attr(&self.ttl, &self.attr_for("", None));
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
            let truncate_result = { self.client.lock().unwrap().truncate(&path, size) };
            if truncate_result.is_err() {
                reply.error(Errno::EIO);
                return;
            }
            self.invalidate_path_cache(&path);
            self.update_file_size(&path, size);
            if let Err(errno) = self.clear_record() {
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
        let metadata_result = if mode.is_some() || modified.is_some() {
            Some(
                self.client
                    .lock()
                    .unwrap()
                    .set_metadata(&path, mode, modified),
            )
        } else {
            None
        };
        if metadata_result
            .as_ref()
            .map(Result::is_err)
            .unwrap_or(false)
        {
            reply.error(Errno::EIO);
            return;
        }
        if mode.is_some() || modified.is_some() {
            self.apply_metadata_update(&path, mode, modified);
            if let Err(errno) = self.clear_record() {
                reply.error(errno);
                return;
            }
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.attr(&self.ttl, &self.attr_for(&path, Some(meta))),
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
        if self.ignored_rel(&path) {
            reply.error(Errno::EACCES);
            return;
        }
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
        let create_result = {
            let mut client = self.client.lock().unwrap();
            client
                .truncate(&path, 0)
                .and_then(|_| client.set_metadata(&path, Some(mode), None))
        };
        if create_result.is_err() {
            reply.error(Errno::EIO);
            return;
        }
        self.invalidate_path_cache(&path);
        self.update_file_size(&path, 0);
        self.apply_metadata_update(&path, Some(mode), None);
        if let Err(errno) = self.clear_record() {
            reply.error(errno);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => {
                let handle = inode_for(&path);
                self.handle_to_path
                    .lock()
                    .unwrap()
                    .insert(handle, path.clone());
                reply.created(
                    &self.ttl,
                    &self.attr_for(&path, Some(meta)),
                    fuser::Generation(0),
                    fuser::FileHandle(handle),
                    FopenFlags::empty(),
                );
            }
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
        if self.ignored_rel(&path) {
            reply.error(Errno::EACCES);
            return;
        }
        if self
            .record(&JournalOp::Mkdir { path: path.clone() })
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        let mkdir_result = {
            let mut client = self.client.lock().unwrap();
            client
                .mkdir_p(&path)
                .and_then(|_| client.set_metadata(&path, Some(mode), None))
        };
        if mkdir_result.is_err() {
            reply.error(Errno::EIO);
            return;
        }
        self.invalidate_path_cache(&path);
        self.update_dir_entry(&path, mode);
        if let Err(errno) = self.clear_record() {
            reply.error(errno);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.entry(
                &self.ttl,
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
        if self.ignored_rel(&path) {
            reply.error(Errno::EACCES);
            return;
        }
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
        let symlink_result = { self.client.lock().unwrap().create_symlink(&path, target) };
        if symlink_result.is_err() {
            reply.error(Errno::EIO);
            return;
        }
        self.invalidate_path_cache(&path);
        self.update_symlink_entry(&path, target);
        if let Err(errno) = self.clear_record() {
            reply.error(errno);
            return;
        }
        let snapshot = self.snapshot.lock().unwrap();
        match snapshot.entries.get(&path) {
            Some(meta) => reply.entry(
                &self.ttl,
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
        if self.ignored_rel(&to) {
            reply.error(Errno::EACCES);
            return;
        }
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
        let rename_result = { self.client.lock().unwrap().rename(&from, &to) };
        match rename_result {
            Ok(()) => {
                self.invalidate_path_cache(&from);
                self.invalidate_path_cache(&to);
                self.rename_tree_entries(&from, &to);
                match self.clear_record() {
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
        if self.ignored_rel(&path) {
            reply.ok();
            return;
        }
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
        let remove_result = { self.client.lock().unwrap().remove(&path, &meta) };
        match remove_result {
            Ok(()) => {
                self.invalidate_path_cache(&path);
                self.remove_tree_entries(&path);
                match self.clear_record() {
                    Ok(()) => reply.ok(),
                    Err(errno) => reply.error(errno),
                }
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }
}

fn dir_cache_from_snapshot(snapshot: &Snapshot) -> DirCache {
    let now = Instant::now();
    let mut dirs: BTreeMap<String, DirEntries> = BTreeMap::new();
    for (path, meta) in &snapshot.entries {
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent, name)) => (parent.to_string(), name.to_string()),
            None => (String::new(), path.clone()),
        };
        dirs.entry(parent).or_default().push((name, meta.clone()));
    }
    dirs.into_iter()
        .map(|(path, entries)| (path, (now, entries)))
        .collect()
}

fn mountfs_journal_path(config: &AppConfig) -> PathBuf {
    let input = format!(
        "{}:{}:{}",
        config.remote.host,
        config.remote.path,
        config.local.root.display()
    );
    let name = hex::encode(sha2::Sha256::digest(input.as_bytes()));
    std::env::temp_dir()
        .join("mobfs")
        .join("mountfs-journals")
        .join(format!("{name}.jsonl"))
}

fn append_journal(path: &Path, op: &JournalOp) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(serde_json::to_string(op)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

fn clear_journal(path: &Path) -> Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(file) => file.sync_data()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
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

fn slice_read(data: &[u8], offset: u64, size: u32) -> &[u8] {
    let start = (offset as usize).min(data.len());
    let end = start.saturating_add(size as usize).min(data.len());
    &data[start..end]
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
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
