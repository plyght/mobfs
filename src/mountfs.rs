use crate::config::AppConfig;
use crate::error::Result;
use crate::snapshot::{EntryKind, EntryMeta};
use crate::vfs::{self, BLOCK_SIZE, Invalidation, MountOptions, Vfs, VfsResult, WorkerPool};
use fuser::{
    Config, Errno, FileAttr, FileType, Filesystem, FopenFlags, INodeNo, KernelConfig, MountOption,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STATFS_BLOCK: u64 = 4096;
const FOREGROUND_WORKERS: usize = 16;

type DirListing = Arc<Vec<(u64, FileType, String)>>;

struct MobfsFuse {
    vfs: Arc<Vfs>,
    workers: WorkerPool,
    dir_handles: Arc<Mutex<HashMap<u64, DirListing>>>,
}

pub fn mount(config: AppConfig, mountpoint: PathBuf, options: MountOptions) -> Result<()> {
    #[cfg(target_os = "macos")]
    if !Path::new("/Library/Filesystems/macfuse.fs").exists() {
        return Err(crate::error::MobfsError::Config(
            "macFUSE is not installed; use the built-in NFS backend (`--backend nfs`, the macOS default) or install macFUSE".to_string(),
        ));
    }
    vfs::prepare_mountpoint(&mountpoint)?;
    let vfs = Vfs::start(config, &options)?;
    let fs = MobfsFuse {
        vfs: vfs.clone(),
        workers: WorkerPool::new("mobfs-io", FOREGROUND_WORKERS),
        dir_handles: Arc::new(Mutex::new(HashMap::new())),
    };
    let mut fuse_config = Config::default();
    fuse_config.mount_options = mount_options(&options, &mountpoint);
    let session = fuser::spawn_mount2(fs, &mountpoint, &fuse_config).map_err(|error| {
        crate::error::MobfsError::Remote(format!(
            "failed to mount FUSE filesystem: {error}. On macOS, install macFUSE and allow its system extension in System Settings if prompted"
        ))
    })?;
    let notifier = session.notifier();
    vfs.set_invalidator(move |invalidation| match invalidation {
        Invalidation::Inode { ino, offset, len } => {
            let _ = notifier.inval_inode(INodeNo(ino), offset, len);
        }
        Invalidation::Entry { parent, name } => {
            let _ = notifier.inval_entry(INodeNo(parent), OsStr::new(&name));
        }
    });
    vfs::install_signal_handlers();
    crate::ui::ok(format!("mounted {}", mountpoint.display()));
    if options.open_when_ready {
        let path = mountpoint.clone();
        std::thread::spawn(move || {
            let _ = crate::sync::open_path(&path);
        });
    }
    loop {
        if session.guard.is_finished() {
            break;
        }
        if vfs::shutdown_requested() {
            crate::ui::info("unmounting", mountpoint.display().to_string());
            vfs.shutdown();
            return session.umount_and_join().map_err(Into::into);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    vfs.shutdown();
    session.join()?;
    Ok(())
}

fn mount_options(options: &MountOptions, mountpoint: &Path) -> Vec<MountOption> {
    #[allow(unused_mut)]
    let mut mount_options = vec![
        MountOption::RW,
        MountOption::FSName("mobfs".to_string()),
        MountOption::Subtype("mobfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    #[cfg(target_os = "macos")]
    {
        let volname = options
            .volname
            .clone()
            .or_else(|| {
                mountpoint
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "MobFS".to_string())
            .replace([',', '='], " ");
        mount_options.push(MountOption::CUSTOM(format!("volname={volname}")));
        mount_options.push(MountOption::CUSTOM("noappledouble".to_string()));
        mount_options.push(MountOption::CUSTOM(format!("iosize={BLOCK_SIZE}")));
        mount_options.push(MountOption::CUSTOM("daemon_timeout=600".to_string()));
        if options.fskit {
            mount_options.push(MountOption::CUSTOM("backend=fskit".to_string()));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (options, mountpoint);
    mount_options
}

fn errno(error: vfs::Errno) -> Errno {
    Errno::from_i32(error.0)
}

fn attr(ino: u64, meta: &EntryMeta) -> FileAttr {
    let time = UNIX_EPOCH + Duration::from_secs(meta.modified.max(0) as u64);
    let mode = meta.mode & 0o7777;
    FileAttr {
        ino: INodeNo(ino),
        size: meta.size,
        blocks: meta.size.div_ceil(512),
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
        kind: file_type(&meta.kind),
        perm: if mode != 0 {
            mode as u16
        } else {
            default_perm(&meta.kind)
        },
        nlink: if meta.kind == EntryKind::Dir { 2 } else { 1 },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        flags: 0,
        blksize: BLOCK_SIZE as u32,
    }
}

fn default_perm(kind: &EntryKind) -> u16 {
    match kind {
        EntryKind::File => 0o644,
        EntryKind::Dir => 0o755,
        EntryKind::Symlink => 0o777,
    }
}

fn file_type(kind: &EntryKind) -> FileType {
    match kind {
        EntryKind::File => FileType::RegularFile,
        EntryKind::Dir => FileType::Directory,
        EntryKind::Symlink => FileType::Symlink,
    }
}

fn name_str(name: &OsStr) -> VfsResult<&str> {
    name.to_str().ok_or(vfs::Errno::EINVAL)
}

fn reply_entry(vfs: &Vfs, reply: ReplyEntry, result: VfsResult<(u64, EntryMeta)>) {
    match result {
        Ok((ino, meta)) => reply.entry(&vfs.ttl(), &attr(ino, &meta), fuser::Generation(0)),
        Err(error) => reply.error(errno(error)),
    }
}

fn reply_empty(reply: ReplyEmpty, result: VfsResult<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(errno(error)),
    }
}

fn send_listing(listing: &[(u64, FileType, String)], offset: u64, mut reply: ReplyDirectory) {
    for (index, (ino, kind, name)) in listing.iter().enumerate().skip(offset as usize) {
        if reply.add(INodeNo(*ino), (index + 1) as u64, *kind, name) {
            break;
        }
    }
    reply.ok();
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

impl Filesystem for MobfsFuse {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.set_max_readahead(BLOCK_SIZE as u32);
        let _ = config.set_max_write(BLOCK_SIZE as u32);
        let _ = config.set_max_background(64);
        Ok(())
    }

    fn destroy(&mut self) {
        self.vfs.shutdown();
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = match name_str(name) {
            Ok(name) => name.to_string(),
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        };
        if let Some(result) = self.vfs.lookup_fast(u64::from(parent), &name) {
            reply_entry(&self.vfs, reply, result);
            return;
        }
        let vfs = self.vfs.clone();
        self.workers.run(move || {
            let result = vfs.lookup(u64::from(parent), &name);
            reply_entry(&vfs, reply, result);
        });
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        let ino = u64::from(ino);
        let respond = move |vfs: &Vfs, reply: ReplyAttr, result: VfsResult<EntryMeta>| match result
        {
            Ok(meta) => reply.attr(&vfs.ttl(), &attr(ino, &meta)),
            Err(error) => reply.error(errno(error)),
        };
        if let Some(result) = self.vfs.getattr_fast(ino) {
            respond(&self.vfs, reply, result);
            return;
        }
        let vfs = self.vfs.clone();
        self.workers.run(move || {
            let result = vfs.getattr(ino);
            respond(&vfs, reply, result);
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        match self.vfs.path_of(u64::from(ino)) {
            Ok(_) => reply.opened(fuser::FileHandle(self.vfs.alloc_fh()), FopenFlags::empty()),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        reply: ReplyEmpty,
    ) {
        vfs::lock(&self.dir_handles).remove(&u64::from(fh));
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        let fh = u64::from(fh);
        if offset > 0
            && let Some(listing) = vfs::lock(&self.dir_handles).get(&fh).cloned()
        {
            send_listing(&listing, offset, reply);
            return;
        }
        let ino = u64::from(ino);
        let parent = self.vfs.parent_of(ino);
        let build = move |items: Vec<vfs::DirItem>| -> DirListing {
            let mut listing = vec![
                (ino, FileType::Directory, ".".to_string()),
                (parent, FileType::Directory, "..".to_string()),
            ];
            listing.extend(
                items
                    .into_iter()
                    .map(|(child, name, meta)| (child, file_type(&meta.kind), name)),
            );
            Arc::new(listing)
        };
        if let Some(result) = self.vfs.list_dir_fast(ino) {
            match result {
                Ok(items) => {
                    let listing = build(items);
                    vfs::lock(&self.dir_handles).insert(fh, listing.clone());
                    send_listing(&listing, offset, reply);
                }
                Err(error) => reply.error(errno(error)),
            }
            return;
        }
        let vfs = self.vfs.clone();
        let dir_handles = self.dir_handles.clone();
        self.workers.run(move || match vfs.list_dir(ino) {
            Ok(items) => {
                let listing = build(items);
                vfs::lock(&dir_handles).insert(fh, listing.clone());
                send_listing(&listing, offset, reply);
            }
            Err(error) => reply.error(errno(error)),
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        match self.vfs.open(u64::from(ino)) {
            Ok((fh, keep_cache)) => reply.opened(
                fuser::FileHandle(fh),
                if keep_cache {
                    FopenFlags::FOPEN_KEEP_CACHE
                } else {
                    FopenFlags::empty()
                },
            ),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.vfs.flush(u64::from(fh)));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.vfs.readlink(u64::from(ino)) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(error) => reply.error(errno(error)),
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
        let vfs = self.vfs.clone();
        self.workers
            .run(move || match vfs.read(u64::from(ino), offset, size) {
                Ok(data) => reply.data(data.as_slice()),
                Err(error) => reply.error(errno(error)),
            });
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
        match self.vfs.write(u64::from(ino), u64::from(fh), offset, data) {
            Ok(()) => reply.written(data.len() as u32),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.vfs.flush(u64::from(fh)));
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: fuser::FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.vfs.fsync(u64::from(ino), u64::from(fh)));
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let vfs = self.vfs.clone();
        self.workers.run(move || {
            let stats = vfs.statfs();
            reply.statfs(
                stats.total_bytes / STATFS_BLOCK,
                stats.free_bytes / STATFS_BLOCK,
                stats.avail_bytes / STATFS_BLOCK,
                stats.files,
                stats.free_files,
                STATFS_BLOCK as u32,
                255,
                STATFS_BLOCK as u32,
            );
        });
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        let name = name.to_string_lossy();
        reply_empty(
            reply,
            self.vfs
                .setxattr(u64::from(ino), &name, value, flags, position),
        );
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self.vfs.getxattr(u64::from(ino), &name.to_string_lossy()) {
            Ok(value) if size == 0 => reply.size(value.len() as u32),
            Ok(value) if value.len() > size as usize => reply.error(Errno::ERANGE),
            Ok(value) => reply.data(&value),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.vfs.listxattr(u64::from(ino)) {
            Ok(names) if size == 0 => reply.size(names.len() as u32),
            Ok(names) if names.len() > size as usize => reply.error(Errno::ERANGE),
            Ok(names) => reply.data(&names),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(
            reply,
            self.vfs
                .removexattr(u64::from(ino), &name.to_string_lossy()),
        );
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
        let ino = u64::from(ino);
        match self
            .vfs
            .setattr(ino, mode, size, mtime.and_then(time_or_now_secs))
        {
            Ok(meta) => reply.attr(&self.vfs.ttl(), &attr(ino, &meta)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = name_str(name).and_then(|name| {
            self.vfs.create(
                u64::from(parent),
                name,
                mode & !umask,
                flags & libc::O_EXCL != 0,
            )
        });
        let (ino, meta) = match result {
            Ok(created) => created,
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        };
        match self.vfs.open(ino) {
            Ok((fh, keep_cache)) => reply.created(
                &self.vfs.ttl(),
                &attr(ino, &meta),
                fuser::Generation(0),
                fuser::FileHandle(fh),
                if keep_cache {
                    FopenFlags::FOPEN_KEEP_CACHE
                } else {
                    FopenFlags::empty()
                },
            ),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let result =
            name_str(name).and_then(|name| self.vfs.mkdir(u64::from(parent), name, mode & !umask));
        reply_entry(&self.vfs, reply, result);
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let result = match (link_name.to_str(), target.to_str()) {
            (Some(name), Some(target)) => self.vfs.symlink(u64::from(parent), name, target),
            _ => Err(vfs::Errno::EINVAL),
        };
        reply_entry(&self.vfs, reply, result);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result =
            name_str(name).and_then(|name| self.vfs.remove(u64::from(parent), name, Some(false)));
        reply_empty(reply, result);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result =
            name_str(name).and_then(|name| self.vfs.remove(u64::from(parent), name, Some(true)));
        reply_empty(reply, result);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let bits = flags.bits();
        let (noreplace, exchange) = if cfg!(target_os = "linux") {
            (bits & 1 != 0, bits & 2 != 0)
        } else {
            (bits & 4 != 0, bits & 2 != 0)
        };
        if exchange {
            reply.error(Errno::EINVAL);
            return;
        }
        let result = name_str(name).and_then(|name| {
            name_str(newname).and_then(|newname| {
                self.vfs.rename(
                    u64::from(parent),
                    name,
                    u64::from(newparent),
                    newname,
                    noreplace,
                )
            })
        });
        reply_empty(reply, result);
    }
}
