use crate::config::{AppConfig, LocalConfig, RemoteConfig, SyncConfig, parse_remote};
use crate::error::Result;
use crate::protocol::{ChangeEvent, ChangeKind, FsStats};
use crate::remote::RemoteClient;
use crate::snapshot::{EntryKind, EntryMeta};
use fuser::{
    Config, Errno, FileAttr, FileType, Filesystem, FopenFlags, INodeNo, KernelConfig, MountOption,
    Notifier, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: u64 = 1024 * 1024;
const WRITE_BUFFER_LIMIT: usize = 8 * 1024 * 1024;
const MAX_INFLIGHT_UPLOAD_BYTES: u64 = 128 * 1024 * 1024;
const PREFETCH_BATCH_BLOCKS: u64 = 8;
const PREFETCH_MAX_FILE_BYTES: u64 = 64 * 1024;
const PREFETCH_MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const SNAPSHOT_MAX_ENTRIES: u64 = 250_000;
const STATFS_TTL: Duration = Duration::from_secs(10);
const STATFS_BLOCK: u64 = 4096;
const FEED_POLL: Duration = Duration::from_secs(10);
const FOREGROUND_WORKERS: usize = 16;

#[derive(Debug, Clone)]
pub struct MountOptions {
    pub connections: usize,
    pub prefetch_connections: usize,
    pub cache_mib: u64,
    pub readahead_mib: u64,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub volname: Option<String>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fskit: bool,
    pub open_when_ready: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            connections: 4,
            prefetch_connections: 8,
            cache_mib: 512,
            readahead_mib: 128,
            volname: None,
            fskit: false,
            open_when_ready: false,
        }
    }
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn request_shutdown(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn mount(config: AppConfig, mountpoint: PathBuf, options: MountOptions) -> Result<()> {
    prepare_mountpoint(&mountpoint)?;
    let ttl = Duration::from_secs(config.sync.cache_ttl_secs.min(60));
    let (fs, feed) = MobfsFuse::new(config, ttl, &options)?;
    let shared = fs.shared.clone();
    let mut fuse_config = Config::default();
    fuse_config.mount_options = mount_options(&options, &mountpoint);
    let session = fuser::spawn_mount2(fs, &mountpoint, &fuse_config).map_err(|error| {
        crate::error::MobfsError::Remote(format!(
            "failed to mount FUSE filesystem: {error}. On macOS, install macFUSE and allow its system extension in System Settings if prompted"
        ))
    })?;
    *lock(&shared.notifier) = Some(session.notifier());
    if let Some((client, epoch, cursor)) = feed {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("mobfs-feed".to_string())
            .spawn(move || run_change_feed(shared, client, epoch, cursor))?;
    }
    unsafe {
        libc::signal(
            libc::SIGINT,
            request_shutdown as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            request_shutdown as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            request_shutdown as *const () as libc::sighandler_t,
        );
    }
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
        if SHUTDOWN.load(Ordering::SeqCst) {
            crate::ui::info("unmounting", mountpoint.display().to_string());
            shared.flush_all_buffers();
            shared.stop.store(true, Ordering::SeqCst);
            return session.umount_and_join().map_err(Into::into);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    shared.stop.store(true, Ordering::SeqCst);
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

pub fn prepare_mountpoint(mountpoint: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if !Path::new("/Library/Filesystems/macfuse.fs").exists() {
        return Err(crate::error::MobfsError::Config(
            "macFUSE is not installed; install macFUSE (brew install --cask macfuse), approve its system extension, then retry `mobfs mount`".to_string(),
        ));
    }
    if is_stale_mount(mountpoint) {
        crate::ui::warn(format!(
            "cleaning up stale mount at {}",
            mountpoint.display()
        ));
        force_unmount(mountpoint);
    }
    if mountpoint.exists() && !mountpoint.is_dir() {
        return Err(crate::error::MobfsError::Config(format!(
            "mountpoint {} exists but is not a directory",
            mountpoint.display()
        )));
    }
    if let Err(error) = std::fs::create_dir_all(mountpoint) {
        if cfg!(target_os = "macos")
            && error.kind() == std::io::ErrorKind::PermissionDenied
            && mountpoint.starts_with("/Volumes")
        {
            return Ok(());
        }
        return Err(error.into());
    }
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

fn is_stale_mount(mountpoint: &Path) -> bool {
    match std::fs::read_dir(mountpoint) {
        Ok(_) => false,
        Err(error) => matches!(
            error.raw_os_error(),
            Some(libc::ENOTCONN) | Some(libc::ENXIO) | Some(libc::EIO)
        ),
    }
}

pub fn force_unmount(mountpoint: &Path) {
    let commands: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("diskutil", &["unmount", "force"]), ("umount", &["-f"])]
    } else {
        &[
            ("fusermount3", &["-uz"]),
            ("fusermount", &["-uz"]),
            ("umount", &["-l"]),
        ]
    };
    for (program, args) in commands {
        let ok = std::process::Command::new(program)
            .args(*args)
            .arg(mountpoint)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
}

pub fn is_mounted(mountpoint: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(parent) = mountpoint.parent() else {
            return false;
        };
        match (std::fs::metadata(mountpoint), std::fs::metadata(parent)) {
            (Ok(mount), Ok(parent)) => mount.dev() != parent.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mountpoint;
        false
    }
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
            user: target.user,
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct WorkerPool {
    sender: mpsc::Sender<Job>,
}

impl WorkerPool {
    fn new(name: &str, threads: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..threads.max(1) {
            let receiver = receiver.clone();
            let _ = std::thread::Builder::new()
                .name(format!("{name}-{index}"))
                .spawn(move || {
                    loop {
                        let job = lock(&receiver).recv();
                        match job {
                            Ok(job) => job(),
                            Err(_) => break,
                        }
                    }
                });
        }
        Self { sender }
    }

    fn run(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.sender.send(Box::new(job));
    }
}

struct ClientPool {
    slots: Vec<Mutex<Option<RemoteClient>>>,
    next: AtomicUsize,
    config: AppConfig,
    client_id: u64,
    endpoint: Option<(String, u16)>,
}

impl ClientPool {
    fn new(
        config: AppConfig,
        client_id: u64,
        endpoint: Option<(String, u16)>,
        size: usize,
        first: Option<RemoteClient>,
    ) -> Self {
        let mut slots = Vec::with_capacity(size.max(1));
        slots.push(Mutex::new(first));
        while slots.len() < size.max(1) {
            slots.push(Mutex::new(None));
        }
        Self {
            slots,
            next: AtomicUsize::new(0),
            config,
            client_id,
            endpoint,
        }
    }

    fn with<T>(&self, action: impl FnOnce(&mut RemoteClient) -> Result<T>) -> Result<T> {
        for slot in &self.slots {
            match slot.try_lock() {
                Ok(mut guard) => return self.use_slot(&mut guard, action),
                Err(TryLockError::Poisoned(error)) => {
                    return self.use_slot(&mut error.into_inner(), action);
                }
                Err(TryLockError::WouldBlock) => {}
            }
        }
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        self.use_slot(&mut lock(&self.slots[index]), action)
    }

    fn use_slot<T>(
        &self,
        slot: &mut Option<RemoteClient>,
        action: impl FnOnce(&mut RemoteClient) -> Result<T>,
    ) -> Result<T> {
        if slot.is_none() {
            *slot = Some(RemoteClient::connect_with(
                self.config.clone(),
                self.client_id,
                self.endpoint.clone(),
            )?);
        }
        match slot.as_mut() {
            Some(client) => action(client),
            None => Err(crate::error::MobfsError::Remote(
                "connection unavailable".to_string(),
            )),
        }
    }
}

struct Inodes {
    by_path: BTreeMap<String, u64>,
    by_ino: HashMap<u64, String>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut by_path = BTreeMap::new();
        let mut by_ino = HashMap::new();
        by_path.insert(String::new(), 1);
        by_ino.insert(1, String::new());
        Self {
            by_path,
            by_ino,
            next: 2,
        }
    }

    fn ino(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert(path.to_string(), ino);
        self.by_ino.insert(ino, path.to_string());
        ino
    }

    fn get(&self, path: &str) -> Option<u64> {
        self.by_path.get(path).copied()
    }

    fn path(&self, ino: u64) -> Option<String> {
        self.by_ino.get(&ino).cloned()
    }

    fn remove_tree(&mut self, path: &str) {
        for key in subtree_keys(&self.by_path, path) {
            if let Some(ino) = self.by_path.remove(&key) {
                self.by_ino.remove(&ino);
            }
        }
    }

    fn rename(&mut self, from: &str, to: &str) {
        self.remove_tree(to);
        for key in subtree_keys(&self.by_path, from) {
            if let Some(ino) = self.by_path.remove(&key) {
                let moved = format!("{to}{}", &key[from.len()..]);
                self.by_path.insert(moved.clone(), ino);
                self.by_ino.insert(ino, moved);
            }
        }
    }
}

fn subtree_keys<V>(map: &BTreeMap<String, V>, path: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if map.contains_key(path) {
        keys.push(path.to_string());
    }
    let prefix = format!("{path}/");
    keys.extend(
        map.range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .map(|(key, _)| key.clone()),
    );
    keys
}

type BlockKey = (String, u64);

struct Inflight {
    done: Mutex<bool>,
    ready: Condvar,
}

impl Inflight {
    fn wait(&self) {
        let mut done = lock(&self.done);
        while !*done {
            done = self
                .ready
                .wait(done)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn finish(&self) {
        *lock(&self.done) = true;
        self.ready.notify_all();
    }
}

struct BlockStore {
    blocks: HashMap<BlockKey, (Arc<Vec<u8>>, u64)>,
    lru: BTreeMap<u64, BlockKey>,
    generations: HashMap<String, u64>,
    tick: u64,
    bytes: u64,
    capacity: u64,
}

struct BlockCache {
    store: Mutex<BlockStore>,
    inflight: Mutex<HashMap<BlockKey, Arc<Inflight>>>,
}

impl BlockCache {
    fn new(capacity: u64) -> Self {
        Self {
            store: Mutex::new(BlockStore {
                blocks: HashMap::new(),
                lru: BTreeMap::new(),
                generations: HashMap::new(),
                tick: 0,
                bytes: 0,
                capacity: capacity.max(BLOCK_SIZE * 4),
            }),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, path: &str, index: u64) -> Option<Arc<Vec<u8>>> {
        let mut store = lock(&self.store);
        let key = (path.to_string(), index);
        let old_tick = store.blocks.get(&key)?.1;
        store.tick += 1;
        let tick = store.tick;
        store.lru.remove(&old_tick);
        store.lru.insert(tick, key.clone());
        let entry = store.blocks.get_mut(&key)?;
        entry.1 = tick;
        Some(entry.0.clone())
    }

    fn contains(&self, path: &str, index: u64) -> bool {
        let key = (path.to_string(), index);
        lock(&self.store).blocks.contains_key(&key) || lock(&self.inflight).contains_key(&key)
    }

    fn generation(&self, path: &str) -> u64 {
        lock(&self.store)
            .generations
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    fn insert(&self, path: &str, index: u64, data: Arc<Vec<u8>>, generation: u64) {
        let mut store = lock(&self.store);
        if store.generations.get(path).copied().unwrap_or(0) != generation {
            return;
        }
        let key = (path.to_string(), index);
        if let Some((old, tick)) = store.blocks.remove(&key) {
            store.bytes = store.bytes.saturating_sub(old.len() as u64);
            store.lru.remove(&tick);
        }
        store.tick += 1;
        let tick = store.tick;
        store.bytes += data.len() as u64;
        store.lru.insert(tick, key.clone());
        store.blocks.insert(key, (data, tick));
        while store.bytes > store.capacity {
            let Some((_, oldest)) = store.lru.pop_first() else {
                break;
            };
            if let Some((old, _)) = store.blocks.remove(&oldest) {
                store.bytes = store.bytes.saturating_sub(old.len() as u64);
            }
        }
    }

    fn invalidate_where(&self, paths: &HashSet<String>, keep: impl Fn(&BlockKey) -> bool) {
        let mut store = lock(&self.store);
        for path in paths {
            *store.generations.entry(path.clone()).or_insert(0) += 1;
        }
        let doomed = store
            .blocks
            .keys()
            .filter(|key| paths.contains(&key.0) && !keep(key))
            .cloned()
            .collect::<Vec<_>>();
        for key in doomed {
            if let Some((old, tick)) = store.blocks.remove(&key) {
                store.bytes = store.bytes.saturating_sub(old.len() as u64);
                store.lru.remove(&tick);
            }
        }
    }

    fn invalidate_path(&self, path: &str) {
        self.invalidate_where(&HashSet::from([path.to_string()]), |_| false);
    }

    fn invalidate_range(&self, path: &str, offset: u64, len: u64) {
        let first = offset / BLOCK_SIZE;
        let last = offset.saturating_add(len.max(1)).saturating_sub(1) / BLOCK_SIZE;
        self.invalidate_where(&HashSet::from([path.to_string()]), |key| {
            key.1 < first || key.1 > last
        });
    }

    fn invalidate_tree(&self, path: &str) {
        let prefix = format!("{path}/");
        let paths = lock(&self.store)
            .blocks
            .keys()
            .map(|key| key.0.clone())
            .filter(|cached| cached == path || cached.starts_with(&prefix))
            .chain(std::iter::once(path.to_string()))
            .collect::<HashSet<_>>();
        self.invalidate_where(&paths, |_| false);
    }

    fn clear(&self) {
        let mut store = lock(&self.store);
        let paths = store
            .blocks
            .keys()
            .map(|key| key.0.clone())
            .collect::<HashSet<_>>();
        for path in paths {
            *store.generations.entry(path).or_insert(0) += 1;
        }
        store.blocks.clear();
        store.lru.clear();
        store.bytes = 0;
    }

    fn fetch_run(&self, path: &str, first: u64, count: u64, pool: &ClientPool) {
        let mut flights = Vec::new();
        {
            let store = lock(&self.store);
            let mut inflight = lock(&self.inflight);
            for index in first..first + count {
                let key = (path.to_string(), index);
                if store.blocks.contains_key(&key) || inflight.contains_key(&key) {
                    break;
                }
                let flight = Arc::new(Inflight {
                    done: Mutex::new(false),
                    ready: Condvar::new(),
                });
                inflight.insert(key, flight.clone());
                flights.push(flight);
            }
        }
        if flights.is_empty() {
            return;
        }
        let generation = self.generation(path);
        let blocks = flights.len() as u64;
        if let Ok((frames, _)) =
            pool.with(|client| client.read_range(path, first * BLOCK_SIZE, blocks * BLOCK_SIZE))
        {
            for (position, block) in into_blocks(frames).into_iter().enumerate() {
                self.insert(path, first + position as u64, Arc::new(block), generation);
            }
        }
        let mut inflight = lock(&self.inflight);
        for (position, flight) in flights.iter().enumerate() {
            inflight.remove(&(path.to_string(), first + position as u64));
            flight.finish();
        }
    }

    fn fetch(&self, path: &str, index: u64, pool: &ClientPool) -> Result<Arc<Vec<u8>>> {
        if let Some(data) = self.get(path, index) {
            return Ok(data);
        }
        let key = (path.to_string(), index);
        let (flight, leader) = {
            let mut inflight = lock(&self.inflight);
            match inflight.get(&key) {
                Some(flight) => (flight.clone(), false),
                None => {
                    let flight = Arc::new(Inflight {
                        done: Mutex::new(false),
                        ready: Condvar::new(),
                    });
                    inflight.insert(key.clone(), flight.clone());
                    (flight, true)
                }
            }
        };
        if !leader {
            flight.wait();
            if let Some(data) = self.get(path, index) {
                return Ok(data);
            }
            let (frames, _) =
                pool.with(|client| client.read_range(path, index * BLOCK_SIZE, BLOCK_SIZE))?;
            return Ok(Arc::new(frames.concat()));
        }
        let generation = self.generation(path);
        let result = pool.with(|client| client.read_range(path, index * BLOCK_SIZE, BLOCK_SIZE));
        let result = result.map(|(frames, _)| {
            let data = Arc::new(into_blocks(frames).into_iter().next().unwrap_or_default());
            self.insert(path, index, data.clone(), generation);
            data
        });
        lock(&self.inflight).remove(&key);
        flight.finish();
        result
    }
}

struct PendingWrite {
    ino: u64,
    path: String,
    offset: u64,
    data: Vec<u8>,
}

struct ReadAhead {
    next_offset: u64,
    window: u64,
    epoch: u64,
}

type DirEntries = Vec<(String, EntryMeta)>;
type DirListing = Arc<Vec<(u64, FileType, String)>>;

struct Shared {
    pool: ClientPool,
    prefetch_pool: ClientPool,
    inodes: Mutex<Inodes>,
    metas: Mutex<BTreeMap<String, EntryMeta>>,
    dirs: Mutex<BTreeMap<String, (Instant, DirEntries)>>,
    blocks: BlockCache,
    next_fh: AtomicU64,
    dir_handles: Mutex<HashMap<u64, DirListing>>,
    write_buffers: Mutex<BTreeMap<u64, PendingWrite>>,
    read_ahead: Mutex<HashMap<u64, ReadAhead>>,
    prefetch_queued: Mutex<HashSet<BlockKey>>,
    versions: Mutex<HashMap<u64, (u64, u64)>>,
    xattrs: Mutex<HashMap<String, BTreeMap<String, Vec<u8>>>>,
    statfs: Mutex<Option<(Instant, FsStats)>>,
    feed_live: AtomicBool,
    notifier: Mutex<Option<Notifier>>,
    stop: AtomicBool,
    journal: PathBuf,
    root_meta: EntryMeta,
    ttl: Duration,
    ignore: Vec<String>,
    readahead_blocks: u64,
    workers: WorkerPool,
    prefetchers: WorkerPool,
    upload_pool: ClientPool,
    uploaders: WorkerPool,
    uploads: Mutex<Uploads>,
    uploads_changed: Condvar,
}

struct Upload {
    fh: u64,
    path: String,
    offset: u64,
    len: u64,
}

#[derive(Default)]
struct Uploads {
    next: u64,
    bytes: u64,
    inflight: HashMap<u64, Upload>,
    errors: HashMap<u64, Errno>,
}

struct MobfsFuse {
    shared: Arc<Shared>,
}

type FeedStart = Option<(RemoteClient, u64, u64)>;

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
    WriteAt {
        path: String,
        offset: u64,
        data: Vec<u8>,
    },
    WriteBlob {
        path: String,
        offset: u64,
        blob: String,
    },
}

impl MobfsFuse {
    fn new(config: AppConfig, ttl: Duration, options: &MountOptions) -> Result<(Self, FeedStart)> {
        let journal = mountfs_journal_path(&config);
        let ignore = config.sync.ignore.clone();
        let client_id = OsRng.next_u64() | 1;
        let mut client = RemoteClient::connect_with(config.clone(), client_id, None)?;
        replay_journal(&journal, &mut client)?;
        let endpoint = Some(client.endpoint());
        let feed = RemoteClient::connect_with(config.clone(), client_id, endpoint.clone())
            .and_then(|mut feed| {
                let batch = feed.watch_changes(0, None, Duration::ZERO)?;
                Ok((feed, batch.epoch, batch.cursor))
            })
            .map_err(|error| {
                crate::ui::warn(format!(
                    "live change feed unavailable ({error}); falling back to cache TTLs"
                ))
            })
            .ok();
        let (snapshot, complete_dirs) = client.snapshot_meta(SNAPSHOT_MAX_ENTRIES)?;
        let now = Instant::now();
        let mut dirs: BTreeMap<String, (Instant, DirEntries)> = complete_dirs
            .into_iter()
            .map(|dir| (dir, (now, Vec::new())))
            .collect();
        for (path, meta) in &snapshot.entries {
            let (parent, name) = split_parent(path);
            if let Some((_, entries)) = dirs.get_mut(parent) {
                entries.push((name.to_string(), meta.clone()));
            }
        }
        let small_files = snapshot
            .entries
            .iter()
            .filter(|(_, meta)| {
                meta.kind == EntryKind::File && meta.size <= PREFETCH_MAX_FILE_BYTES
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let prefetched = client
            .read_small_files(
                small_files,
                PREFETCH_MAX_FILE_BYTES,
                PREFETCH_MAX_TOTAL_BYTES,
            )
            .unwrap_or_default();
        let blocks = BlockCache::new(options.cache_mib.saturating_mul(1024 * 1024));
        for (path, data) in prefetched {
            blocks.insert(&path, 0, Arc::new(data), 0);
        }
        let pool = ClientPool::new(
            config.clone(),
            client_id,
            endpoint.clone(),
            options.connections,
            Some(client),
        );
        let prefetch_pool = ClientPool::new(
            config.clone(),
            client_id,
            endpoint.clone(),
            options.prefetch_connections,
            None,
        );
        let upload_connections = options.connections.saturating_mul(2).max(1);
        let upload_pool = ClientPool::new(config, client_id, endpoint, upload_connections, None);
        let shared = Arc::new(Shared {
            pool,
            prefetch_pool,
            inodes: Mutex::new(Inodes::new()),
            metas: Mutex::new(snapshot.entries),
            dirs: Mutex::new(dirs),
            blocks,
            next_fh: AtomicU64::new(1),
            dir_handles: Mutex::new(HashMap::new()),
            write_buffers: Mutex::new(BTreeMap::new()),
            read_ahead: Mutex::new(HashMap::new()),
            prefetch_queued: Mutex::new(HashSet::new()),
            versions: Mutex::new(HashMap::new()),
            xattrs: Mutex::new(HashMap::new()),
            statfs: Mutex::new(None),
            feed_live: AtomicBool::new(false),
            notifier: Mutex::new(None),
            stop: AtomicBool::new(false),
            journal,
            root_meta: EntryMeta {
                kind: EntryKind::Dir,
                size: 0,
                modified: unix_now_secs(),
                sha256: None,
                mode: 0o755,
                link_target: None,
            },
            ttl,
            ignore,
            readahead_blocks: options.readahead_mib,
            workers: WorkerPool::new("mobfs-io", FOREGROUND_WORKERS),
            prefetchers: WorkerPool::new("mobfs-prefetch", options.prefetch_connections.max(1)),
            upload_pool,
            uploaders: WorkerPool::new("mobfs-upload", upload_connections),
            uploads: Mutex::new(Uploads::default()),
            uploads_changed: Condvar::new(),
        });
        Ok((Self { shared }, feed))
    }
}

impl Shared {
    fn record(&self, op: &JournalOp) -> std::result::Result<(), Errno> {
        append_journal(&self.journal, op).map_err(|_| Errno::EIO)
    }

    fn clear_record(&self) -> std::result::Result<(), Errno> {
        clear_journal(&self.journal).map_err(|_| Errno::EIO)
    }

    fn recover_journal(&self) -> std::result::Result<(), Errno> {
        for attempt in 0..crate::config::DEFAULT_OP_RETRIES {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(100 * u64::from(attempt).min(20)));
            }
            let result = self.pool.with(|client| {
                client.reconnect()?;
                replay_journal(&self.journal, client)
            });
            if result.is_ok() {
                return Ok(());
            }
        }
        Err(Errno::EIO)
    }

    fn mutate(
        &self,
        op: JournalOp,
        action: impl Fn(&mut RemoteClient) -> Result<()>,
    ) -> std::result::Result<(), Errno> {
        self.record(&op)?;
        match self.pool.with(action) {
            Ok(()) => {}
            Err(crate::error::MobfsError::Server(message)) => {
                self.clear_record()?;
                return Err(errno_for_server_error(&message));
            }
            Err(_) => self.recover_journal()?,
        }
        self.clear_record()
    }

    fn path(&self, ino: INodeNo) -> Option<String> {
        lock(&self.inodes).path(u64::from(ino))
    }

    fn ino(&self, path: &str) -> u64 {
        lock(&self.inodes).ino(path)
    }

    fn meta(&self, path: &str) -> Option<EntryMeta> {
        lock(&self.metas).get(path).cloned()
    }

    fn set_meta(&self, path: &str, meta: EntryMeta) {
        lock(&self.metas).insert(path.to_string(), meta);
    }

    fn ignored(&self, path: &str) -> bool {
        path.split('/')
            .any(|part| crate::local::should_ignore_part(part, &self.ignore))
    }

    fn dir_listing(&self, dir: &str) -> Option<DirEntries> {
        let dirs = lock(&self.dirs);
        let (created, entries) = dirs.get(dir)?;
        if self.ttl.is_zero()
            || self.feed_live.load(Ordering::SeqCst)
            || created.elapsed() <= self.ttl
        {
            Some(entries.clone())
        } else {
            None
        }
    }

    fn known_absent(&self, dir: &str, name: &str) -> bool {
        self.dir_listing(dir)
            .is_some_and(|entries| !entries.iter().any(|(entry, _)| entry == name))
    }

    fn dir_upsert(&self, path: &str, meta: &EntryMeta) {
        let (parent, name) = split_parent(path);
        if let Some((_, entries)) = lock(&self.dirs).get_mut(parent) {
            entries.retain(|(entry, _)| entry != name);
            entries.push((name.to_string(), meta.clone()));
        }
    }

    fn dir_remove(&self, path: &str) {
        let (parent, name) = split_parent(path);
        let mut dirs = lock(&self.dirs);
        if let Some((_, entries)) = dirs.get_mut(parent) {
            entries.retain(|(entry, _)| entry != name);
        }
        for key in subtree_keys(&dirs, path) {
            dirs.remove(&key);
        }
    }

    fn has_pending_write(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        lock(&self.write_buffers)
            .values()
            .any(|pending| pending.path == path || pending.path.starts_with(&prefix))
            || lock(&self.uploads)
                .inflight
                .values()
                .any(|upload| upload.path == path || upload.path.starts_with(&prefix))
    }

    fn take_upload_error(&self, fh: u64) -> std::result::Result<(), Errno> {
        match lock(&self.uploads).errors.remove(&fh) {
            Some(errno) => Err(errno),
            None => Ok(()),
        }
    }

    fn dispatch_fh(self: &Arc<Self>, fh: u64) {
        let pending = lock(&self.write_buffers).remove(&fh);
        let Some(pending) = pending else {
            return;
        };
        let path = lock(&self.inodes).path(pending.ino).unwrap_or(pending.path);
        let offset = pending.offset;
        let len = pending.data.len() as u64;
        let data = pending.data;
        let id = {
            let mut uploads = lock(&self.uploads);
            loop {
                let overlaps = uploads.inflight.values().any(|upload| {
                    upload.path == path
                        && upload.offset < offset + len
                        && offset < upload.offset + upload.len
                });
                let room =
                    uploads.inflight.is_empty() || uploads.bytes + len <= MAX_INFLIGHT_UPLOAD_BYTES;
                if !overlaps && room {
                    break;
                }
                uploads = self
                    .uploads_changed
                    .wait(uploads)
                    .unwrap_or_else(|error| error.into_inner());
            }
            uploads.next += 1;
            let id = uploads.next;
            uploads.bytes += len;
            uploads.inflight.insert(
                id,
                Upload {
                    fh,
                    path: path.clone(),
                    offset,
                    len,
                },
            );
            id
        };
        let shared = self.clone();
        self.uploaders.run(move || {
            let result = shared
                .upload_pool
                .with(|client| client.write_file_at(&path, offset, data));
            shared.blocks.invalidate_range(&path, offset, len);
            let mut uploads = lock(&shared.uploads);
            uploads.inflight.remove(&id);
            uploads.bytes = uploads.bytes.saturating_sub(len);
            if let Err(error) = result {
                let errno = match error {
                    crate::error::MobfsError::Server(message) => errno_for_server_error(&message),
                    _ => Errno::EIO,
                };
                uploads.errors.insert(fh, errno);
            }
            shared.uploads_changed.notify_all();
        });
    }

    fn wait_uploads(&self, done: impl Fn(&Upload) -> bool) {
        let mut uploads = lock(&self.uploads);
        while uploads.inflight.values().any(&done) {
            uploads = self
                .uploads_changed
                .wait(uploads)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn flush_fh(self: &Arc<Self>, fh: u64) -> std::result::Result<(), Errno> {
        self.dispatch_fh(fh);
        self.wait_uploads(|upload| upload.fh == fh);
        self.take_upload_error(fh)
    }

    fn flush_matching(self: &Arc<Self>, path: &str) -> std::result::Result<(), Errno> {
        let prefix = format!("{path}/");
        let matches = |candidate: &str| candidate == path || candidate.starts_with(&prefix);
        let fhs = lock(&self.write_buffers)
            .iter()
            .filter(|(_, pending)| matches(&pending.path))
            .map(|(fh, _)| *fh)
            .collect::<Vec<_>>();
        for fh in &fhs {
            self.dispatch_fh(*fh);
        }
        self.wait_uploads(|upload| matches(&upload.path));
        for fh in fhs {
            self.take_upload_error(fh)?;
        }
        Ok(())
    }

    fn flush_all_buffers(self: &Arc<Self>) {
        let fhs = lock(&self.write_buffers)
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for fh in fhs {
            self.dispatch_fh(fh);
        }
        self.wait_uploads(|_| true);
    }

    fn note_local_write(&self, path: &str, offset: u64, len: u64) {
        let mut metas = lock(&self.metas);
        let entry = metas
            .entry(path.to_string())
            .or_insert_with(|| file_meta(0, 0o644));
        entry.kind = EntryKind::File;
        entry.size = entry.size.max(offset.saturating_add(len));
        entry.modified = unix_now_secs();
        entry.sha256 = None;
    }

    fn stat_remote(&self, path: &str) -> Result<Option<EntryMeta>> {
        let meta = self.pool.with(|client| client.stat(path))?;
        match &meta {
            Some(meta) if !self.has_pending_write(path) => self.set_meta(path, meta.clone()),
            Some(_) => {}
            None => {
                lock(&self.metas).remove(path);
            }
        }
        Ok(meta)
    }

    fn attr(&self, ino: u64, meta: Option<&EntryMeta>) -> FileAttr {
        let meta = meta.or(Some(&self.root_meta));
        let kind = meta.map(|meta| &meta.kind).unwrap_or(&EntryKind::Dir);
        let size = meta.map(|meta| meta.size).unwrap_or(0);
        let modified = meta.map(|meta| meta.modified).unwrap_or(0).max(0) as u64;
        let time = UNIX_EPOCH + Duration::from_secs(modified);
        let mode = meta.map(|meta| meta.mode & 0o7777).unwrap_or(0);
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: file_type(kind),
            perm: if mode != 0 {
                mode as u16
            } else {
                match kind {
                    EntryKind::File => 0o644,
                    EntryKind::Dir => 0o755,
                    EntryKind::Symlink => 0o777,
                }
            },
            nlink: if *kind == EntryKind::Dir { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: BLOCK_SIZE as u32,
        }
    }

    fn bump_version(&self, ino: u64) {
        lock(&self.versions).entry(ino).or_insert((0, 0)).0 += 1;
    }

    fn open_flags(&self, ino: u64) -> FopenFlags {
        let mut versions = lock(&self.versions);
        let entry = versions.entry(ino).or_insert((0, u64::MAX));
        let keep = entry.0 == entry.1;
        entry.1 = entry.0;
        if keep {
            FopenFlags::FOPEN_KEEP_CACHE
        } else {
            FopenFlags::empty()
        }
    }

    fn fs_stats(&self) -> FsStats {
        if let Some((fetched, stats)) = lock(&self.statfs).as_ref()
            && fetched.elapsed() < STATFS_TTL
        {
            return stats.clone();
        }
        match self.pool.with(|client| client.statfs()) {
            Ok(stats) => {
                *lock(&self.statfs) = Some((Instant::now(), stats.clone()));
                stats
            }
            Err(_) => lock(&self.statfs)
                .as_ref()
                .map(|(_, stats)| stats.clone())
                .unwrap_or(FsStats {
                    total_bytes: 1 << 50,
                    free_bytes: 1 << 50,
                    avail_bytes: 1 << 50,
                    files: 1 << 32,
                    free_files: 1 << 32,
                }),
        }
    }

    fn do_read(self: &Arc<Self>, ino: u64, path: String, offset: u64, size: u32, reply: ReplyData) {
        let file_size = self.meta(&path).map(|meta| meta.size);
        if file_size.is_some_and(|len| offset >= len) {
            reply.data(&[]);
            return;
        }
        self.schedule_readahead(ino, &path, offset, u64::from(size), file_size);
        let end = offset.saturating_add(u64::from(size));
        let mut out = Vec::new();
        let mut single: Option<(Arc<Vec<u8>>, usize, usize)> = None;
        let sequential = lock(&self.read_ahead)
            .get(&ino)
            .is_some_and(|state| state.window >= 2);
        let last_block = file_size.map(|len| len.saturating_sub(1) / BLOCK_SIZE);
        for index in offset / BLOCK_SIZE..=end.saturating_sub(1) / BLOCK_SIZE {
            if sequential && !self.blocks.contains(&path, index) {
                let batch_end = (index / PREFETCH_BATCH_BLOCKS + 1) * PREFETCH_BATCH_BLOCKS;
                let batch_end = last_block.map_or(batch_end, |last| batch_end.min(last + 1));
                self.blocks.fetch_run(
                    &path,
                    index,
                    batch_end.saturating_sub(index).max(1),
                    &self.pool,
                );
            }
            let block = match self.blocks.fetch(&path, index, &self.pool) {
                Ok(block) => block,
                Err(error) if out.is_empty() && single.is_none() => {
                    reply.error(match error {
                        crate::error::MobfsError::Server(message) => {
                            errno_for_server_error(&message)
                        }
                        _ => Errno::EIO,
                    });
                    return;
                }
                Err(_) => break,
            };
            let start = index * BLOCK_SIZE;
            let from = (offset.max(start) - start) as usize;
            let to = ((end.min(start + BLOCK_SIZE) - start) as usize).min(block.len());
            let short = (block.len() as u64) < BLOCK_SIZE;
            if from < to {
                match single.take() {
                    None if out.is_empty() => single = Some((block, from, to)),
                    previous => {
                        if let Some((first, first_from, first_to)) = previous {
                            out.extend_from_slice(&first[first_from..first_to]);
                        }
                        out.extend_from_slice(&block[from..to]);
                    }
                }
            }
            if short {
                break;
            }
        }
        match single {
            Some((block, from, to)) => reply.data(&block[from..to]),
            None => reply.data(&out),
        }
    }

    fn schedule_readahead(
        self: &Arc<Self>,
        ino: u64,
        path: &str,
        offset: u64,
        size: u64,
        file_size: Option<u64>,
    ) {
        if self.readahead_blocks == 0 {
            return;
        }
        let (window, epoch) = {
            let mut states = lock(&self.read_ahead);
            let state = states.entry(ino).or_insert(ReadAhead {
                next_offset: 0,
                window: 0,
                epoch: 0,
            });
            if offset.abs_diff(state.next_offset) <= 2 * BLOCK_SIZE {
                state.window = (state.window * 2).clamp(2, self.readahead_blocks);
            } else {
                state.epoch += 1;
                state.window = 1;
            }
            state.next_offset = offset.saturating_add(size);
            (state.window, state.epoch)
        };
        let current = offset.saturating_add(size.saturating_sub(1)) / BLOCK_SIZE;
        let last = file_size.map(|len| len.saturating_sub(1) / BLOCK_SIZE);
        let horizon = match last {
            Some(last) => (current + window).min(last),
            None => current + window,
        };
        let mut first = current + 1;
        while first <= horizon {
            let batch_end = ((first / PREFETCH_BATCH_BLOCKS) + 1) * PREFETCH_BATCH_BLOCKS;
            let end = if window >= PREFETCH_BATCH_BLOCKS {
                batch_end
            } else {
                batch_end.min(horizon + 1)
            };
            let end = match last {
                Some(last) => end.min(last + 1),
                None => end,
            };
            let run_first = first;
            first = end;
            let missing = (run_first..end).any(|index| !self.blocks.contains(path, index));
            if !missing || !lock(&self.prefetch_queued).insert((path.to_string(), run_first)) {
                continue;
            }
            let shared = self.clone();
            let path = path.to_string();
            self.prefetchers.run(move || {
                let current_epoch = lock(&shared.read_ahead).get(&ino).map(|state| state.epoch);
                if current_epoch == Some(epoch) && !shared.stop.load(Ordering::SeqCst) {
                    let mut index = run_first;
                    while index < end {
                        if shared.blocks.contains(&path, index) {
                            index += 1;
                            continue;
                        }
                        let mut stop = index;
                        while stop < end && !shared.blocks.contains(&path, stop) {
                            stop += 1;
                        }
                        shared
                            .blocks
                            .fetch_run(&path, index, stop - index, &shared.prefetch_pool);
                        index = stop;
                    }
                }
                lock(&shared.prefetch_queued).remove(&(path.clone(), run_first));
            });
        }
    }

    fn apply_change(&self, event: &ChangeEvent) {
        let path = event.path.as_str();
        if path.is_empty() || self.ignored(path) || self.has_pending_write(path) {
            return;
        }
        let (parent, name) = split_parent(path);
        let (ino, parent_ino) = {
            let inodes = lock(&self.inodes);
            (inodes.get(path), inodes.get(parent))
        };
        let mut inval_inode = None;
        let mut inval_entry = false;
        match &event.kind {
            ChangeKind::Write { offset, len } => {
                self.blocks.invalidate_range(path, *offset, *len);
                lock(&self.metas).remove(path);
                if let Some(ino) = ino {
                    self.bump_version(ino);
                    inval_inode = Some((ino, *offset as i64, *len as i64));
                }
            }
            ChangeKind::Metadata => {
                lock(&self.metas).remove(path);
                inval_inode = ino.map(|ino| (ino, -1, 0));
            }
            ChangeKind::Replaced => {
                {
                    let mut metas = lock(&self.metas);
                    for key in subtree_keys(&metas, path) {
                        metas.remove(&key);
                    }
                }
                {
                    let mut dirs = lock(&self.dirs);
                    for key in subtree_keys(&dirs, path) {
                        dirs.remove(&key);
                    }
                }
                self.blocks.invalidate_tree(path);
                let inos = {
                    let inodes = lock(&self.inodes);
                    subtree_keys(&inodes.by_path, path)
                        .iter()
                        .filter_map(|key| inodes.get(key))
                        .collect::<Vec<_>>()
                };
                for ino in inos {
                    self.bump_version(ino);
                }
                inval_inode = ino.map(|ino| (ino, 0, 0));
                inval_entry = true;
            }
        }
        lock(&self.dirs).remove(parent);
        if let Some(notifier) = lock(&self.notifier).clone() {
            if let Some((ino, offset, len)) = inval_inode {
                let _ = notifier.inval_inode(INodeNo(ino), offset, len);
            }
            if inval_entry && let Some(parent_ino) = parent_ino {
                let _ = notifier.inval_entry(INodeNo(parent_ino), OsStr::new(name));
            }
        }
    }

    fn invalidate_all(&self) {
        {
            let pending = lock(&self.write_buffers)
                .values()
                .map(|pending| pending.path.clone())
                .collect::<HashSet<_>>();
            lock(&self.metas).retain(|path, _| pending.contains(path));
        }
        lock(&self.dirs).clear();
        self.blocks.clear();
        let inos = lock(&self.inodes)
            .by_ino
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for ino in inos {
            self.bump_version(ino);
        }
    }
}

fn run_change_feed(shared: Arc<Shared>, mut client: RemoteClient, mut epoch: u64, mut cursor: u64) {
    let mut failures = 0_u32;
    while !shared.stop.load(Ordering::SeqCst) {
        match client.watch_changes(epoch, Some(cursor), FEED_POLL) {
            Ok(batch) => {
                failures = 0;
                if batch.reset {
                    shared.invalidate_all();
                } else {
                    for event in &batch.events {
                        shared.apply_change(event);
                    }
                }
                shared.feed_live.store(batch.live, Ordering::SeqCst);
                epoch = batch.epoch;
                cursor = batch.cursor;
            }
            Err(_) => {
                shared.feed_live.store(false, Ordering::SeqCst);
                failures = failures.saturating_add(1);
                std::thread::sleep(Duration::from_millis(250 * u64::from(failures.min(20))));
                let _ = client.reconnect();
            }
        }
    }
}

impl Filesystem for MobfsFuse {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.set_max_readahead(BLOCK_SIZE as u32);
        let _ = config.set_max_write(BLOCK_SIZE as u32);
        let _ = config.set_max_background(64);
        Ok(())
    }

    fn destroy(&mut self) {
        self.shared.flush_all_buffers();
        self.shared.stop.store(true, Ordering::SeqCst);
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.shared.path(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        if self.shared.ignored(&path) {
            reply.error(Errno::ENOENT);
            return;
        }
        let shared = &self.shared;
        if let Some(meta) = shared.meta(&path) {
            let ino = shared.ino(&path);
            reply.entry(
                &shared.ttl,
                &shared.attr(ino, Some(&meta)),
                fuser::Generation(0),
            );
            return;
        }
        if shared.known_absent(&parent_path, name) {
            reply.error(Errno::ENOENT);
            return;
        }
        let shared = self.shared.clone();
        self.shared
            .workers
            .run(move || match shared.stat_remote(&path) {
                Ok(Some(meta)) => {
                    let ino = shared.ino(&path);
                    reply.entry(
                        &shared.ttl,
                        &shared.attr(ino, Some(&meta)),
                        fuser::Generation(0),
                    );
                }
                Ok(None) => reply.error(Errno::ENOENT),
                Err(_) => reply.error(Errno::EIO),
            });
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        let shared = &self.shared;
        let Some(path) = shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if path.is_empty() {
            reply.attr(&shared.ttl, &shared.attr(1, None));
            return;
        }
        if let Some(meta) = shared.meta(&path) {
            reply.attr(&shared.ttl, &shared.attr(u64::from(ino), Some(&meta)));
            return;
        }
        let shared = self.shared.clone();
        self.shared
            .workers
            .run(move || match shared.stat_remote(&path) {
                Ok(Some(meta)) => {
                    reply.attr(&shared.ttl, &shared.attr(u64::from(ino), Some(&meta)))
                }
                Ok(None) => reply.error(Errno::ENOENT),
                Err(_) => reply.error(Errno::EIO),
            });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        if self.shared.path(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let fh = self.shared.next_fh.fetch_add(1, Ordering::SeqCst);
        reply.opened(fuser::FileHandle(fh), FopenFlags::empty());
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        reply: ReplyEmpty,
    ) {
        lock(&self.shared.dir_handles).remove(&u64::from(fh));
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
            && let Some(listing) = lock(&self.shared.dir_handles).get(&fh).cloned()
        {
            send_listing(&listing, offset, reply);
            return;
        }
        let Some(dir) = self.shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let parent_ino = self.shared.ino(split_parent(&dir).0);
        let build = move |shared: &Shared, entries: DirEntries, fresh: bool| -> DirListing {
            let mut listing = vec![
                (u64::from(ino), FileType::Directory, ".".to_string()),
                (parent_ino, FileType::Directory, "..".to_string()),
            ];
            for (name, meta) in entries {
                let path = join_rel(&dir, &name);
                if fresh && !shared.has_pending_write(&path) {
                    shared.set_meta(&path, meta.clone());
                }
                listing.push((shared.ino(&path), file_type(&meta.kind), name));
            }
            Arc::new(listing)
        };
        let dir = self.shared.path(ino).unwrap_or_default();
        if let Some(entries) = self.shared.dir_listing(&dir) {
            let listing = build(&self.shared, entries, false);
            lock(&self.shared.dir_handles).insert(fh, listing.clone());
            send_listing(&listing, offset, reply);
            return;
        }
        let shared = self.shared.clone();
        self.shared.workers.run(
            move || match shared.pool.with(|client| client.list_dir(&dir)) {
                Ok(entries) => {
                    lock(&shared.dirs).insert(dir.clone(), (Instant::now(), entries.clone()));
                    let listing = build(&shared, entries, true);
                    lock(&shared.dir_handles).insert(fh, listing.clone());
                    send_listing(&listing, offset, reply);
                }
                Err(_) => reply.error(Errno::EIO),
            },
        );
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        if self.shared.path(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let fh = self.shared.next_fh.fetch_add(1, Ordering::SeqCst);
        let flags = self.shared.open_flags(u64::from(ino));
        reply.opened(fuser::FileHandle(fh), flags);
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
        match self.shared.flush_fh(u64::from(fh)) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.shared.path(ino).filter(|path| !path.is_empty()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let meta = match self.shared.meta(&path) {
            Some(meta) => Some(meta),
            None => match self.shared.stat_remote(&path) {
                Ok(meta) => meta,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            },
        };
        match meta.and_then(|meta| meta.link_target) {
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
        let Some(path) = self.shared.path(ino).filter(|path| !path.is_empty()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Err(errno) = self.shared.flush_matching(&path) {
            reply.error(errno);
            return;
        }
        let shared = self.shared.clone();
        self.shared
            .workers
            .run(move || shared.do_read(u64::from(ino), path, offset, size, reply));
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
        let shared = &self.shared;
        let Some(path) = shared.path(ino).filter(|path| !path.is_empty()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if shared.ignored(&path) {
            reply.error(Errno::EACCES);
            return;
        }
        let fh = u64::from(fh);
        if let Err(errno) = shared.take_upload_error(fh) {
            reply.error(errno);
            return;
        }
        let flush_needed = {
            let mut buffers = lock(&shared.write_buffers);
            match buffers.get_mut(&fh) {
                Some(pending)
                    if pending.ino == u64::from(ino)
                        && pending.offset.saturating_add(pending.data.len() as u64) == offset
                        && pending.data.len().saturating_add(data.len()) <= WRITE_BUFFER_LIMIT =>
                {
                    pending.data.extend_from_slice(data);
                    false
                }
                Some(_) => true,
                None => {
                    buffers.insert(
                        fh,
                        PendingWrite {
                            ino: u64::from(ino),
                            path: path.clone(),
                            offset,
                            data: data.to_vec(),
                        },
                    );
                    false
                }
            }
        };
        if flush_needed {
            shared.dispatch_fh(fh);
            if let Err(errno) = shared.take_upload_error(fh) {
                reply.error(errno);
                return;
            }
            lock(&shared.write_buffers).insert(
                fh,
                PendingWrite {
                    ino: u64::from(ino),
                    path: path.clone(),
                    offset,
                    data: data.to_vec(),
                },
            );
        }
        shared
            .blocks
            .invalidate_range(&path, offset, data.len() as u64);
        shared.note_local_write(&path, offset, data.len() as u64);
        reply.written(data.len() as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.shared.flush_fh(u64::from(fh)) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
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
        let Some(path) = self.shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Err(errno) = self.shared.flush_fh(u64::from(fh)) {
            reply.error(errno);
            return;
        }
        match self.shared.pool.with(|client| client.fsync(&path)) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let shared = self.shared.clone();
        self.shared.workers.run(move || {
            let stats = shared.fs_stats();
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
        let Some(path) = self.shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let name = name.to_string_lossy().to_string();
        let mut xattrs = lock(&self.shared.xattrs);
        let attrs = xattrs.entry(path).or_default();
        let exists = attrs.contains_key(&name);
        if flags & libc::XATTR_CREATE != 0 && exists {
            reply.error(Errno::EEXIST);
            return;
        }
        if flags & libc::XATTR_REPLACE != 0 && !exists {
            reply.error(Errno::NO_XATTR);
            return;
        }
        let stored = attrs.entry(name).or_default();
        let position = position as usize;
        if position == 0 {
            *stored = value.to_vec();
        } else {
            if stored.len() < position + value.len() {
                stored.resize(position + value.len(), 0);
            }
            stored[position..position + value.len()].copy_from_slice(value);
        }
        reply.ok();
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(path) = self.shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let value = lock(&self.shared.xattrs)
            .get(&path)
            .and_then(|attrs| attrs.get(name.to_string_lossy().as_ref()).cloned());
        match value {
            None => reply.error(Errno::NO_XATTR),
            Some(value) if size == 0 => reply.size(value.len() as u32),
            Some(value) if value.len() > size as usize => reply.error(Errno::ERANGE),
            Some(value) => reply.data(&value),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let Some(path) = self.shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mut names = Vec::new();
        if let Some(attrs) = lock(&self.shared.xattrs).get(&path) {
            for name in attrs.keys() {
                names.extend_from_slice(name.as_bytes());
                names.push(0);
            }
        }
        if size == 0 {
            reply.size(names.len() as u32);
        } else if names.len() > size as usize {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(&names);
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let removed = lock(&self.shared.xattrs)
            .get_mut(&path)
            .and_then(|attrs| attrs.remove(name.to_string_lossy().as_ref()));
        match removed {
            Some(_) => reply.ok(),
            None => reply.error(Errno::NO_XATTR),
        }
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
        let shared = &self.shared;
        let Some(path) = shared.path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if path.is_empty() {
            reply.attr(&shared.ttl, &shared.attr(1, None));
            return;
        }
        if let Err(errno) = shared.flush_matching(&path) {
            reply.error(errno);
            return;
        }
        if let Some(size) = size {
            let op = JournalOp::Truncate {
                path: path.clone(),
                size,
            };
            if let Err(errno) = shared.mutate(op, |client| client.truncate(&path, size)) {
                reply.error(errno);
                return;
            }
            shared.blocks.invalidate_path(&path);
            let mut metas = lock(&shared.metas);
            let entry = metas
                .entry(path.clone())
                .or_insert_with(|| file_meta(size, 0o644));
            entry.size = size;
            entry.modified = unix_now_secs();
        }
        let modified = mtime.and_then(time_or_now_secs);
        let mode = mode.map(|mode| mode & 0o7777);
        if mode.is_some() || modified.is_some() {
            let op = JournalOp::SetMetadata {
                path: path.clone(),
                mode,
                modified,
            };
            if let Err(errno) =
                shared.mutate(op, |client| client.set_metadata(&path, mode, modified))
            {
                reply.error(errno);
                return;
            }
            if let Some(entry) = lock(&shared.metas).get_mut(&path) {
                if let Some(mode) = mode {
                    entry.mode = mode;
                }
                if let Some(modified) = modified {
                    entry.modified = modified;
                }
            }
        }
        let meta = match shared.meta(&path) {
            Some(meta) => Some(meta),
            None => shared.stat_remote(&path).ok().flatten(),
        };
        match meta {
            Some(meta) => reply.attr(&shared.ttl, &shared.attr(u64::from(ino), Some(&meta))),
            None => reply.error(Errno::ENOENT),
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
        let shared = &self.shared;
        let Some(parent_path) = shared.path(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        if shared.ignored(&path) {
            reply.error(Errno::EACCES);
            return;
        }
        if flags & libc::O_EXCL != 0 && shared.meta(&path).is_some() {
            reply.error(Errno::EEXIST);
            return;
        }
        let mode = mode & !umask & 0o7777;
        let op = JournalOp::Truncate {
            path: path.clone(),
            size: 0,
        };
        let result = shared.mutate(op, |client| {
            client.truncate(&path, 0)?;
            client.set_metadata(&path, Some(mode), None)
        });
        if let Err(errno) = result {
            reply.error(errno);
            return;
        }
        let meta = file_meta(0, mode);
        shared.blocks.invalidate_path(&path);
        shared.set_meta(&path, meta.clone());
        shared.dir_upsert(&path, &meta);
        let ino = shared.ino(&path);
        shared.bump_version(ino);
        let flags = shared.open_flags(ino);
        let fh = shared.next_fh.fetch_add(1, Ordering::SeqCst);
        reply.created(
            &shared.ttl,
            &shared.attr(ino, Some(&meta)),
            fuser::Generation(0),
            fuser::FileHandle(fh),
            flags,
        );
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
        let shared = &self.shared;
        let Some(parent_path) = shared.path(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        if shared.ignored(&path) {
            reply.error(Errno::EACCES);
            return;
        }
        let mode = mode & !umask & 0o7777;
        let op = JournalOp::Mkdir { path: path.clone() };
        let result = shared.mutate(op, |client| {
            client.mkdir_p(&path)?;
            client.set_metadata(&path, Some(mode), None)
        });
        if let Err(errno) = result {
            reply.error(errno);
            return;
        }
        let meta = EntryMeta {
            kind: EntryKind::Dir,
            size: 0,
            modified: unix_now_secs(),
            sha256: None,
            mode,
            link_target: None,
        };
        shared.set_meta(&path, meta.clone());
        shared.dir_upsert(&path, &meta);
        lock(&shared.dirs).insert(path.clone(), (Instant::now(), Vec::new()));
        let ino = shared.ino(&path);
        reply.entry(
            &shared.ttl,
            &shared.attr(ino, Some(&meta)),
            fuser::Generation(0),
        );
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let shared = &self.shared;
        let Some(parent_path) = shared.path(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let (Some(name), Some(target)) = (link_name.to_str(), target.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        if shared.ignored(&path) {
            reply.error(Errno::EACCES);
            return;
        }
        let op = JournalOp::Symlink {
            path: path.clone(),
            target: target.to_string(),
        };
        if let Err(errno) = shared.mutate(op, |client| client.create_symlink(&path, target)) {
            reply.error(errno);
            return;
        }
        let meta = EntryMeta {
            kind: EntryKind::Symlink,
            size: target.len() as u64,
            modified: unix_now_secs(),
            sha256: None,
            mode: 0o777,
            link_target: Some(target.to_string()),
        };
        shared.set_meta(&path, meta.clone());
        shared.dir_upsert(&path, &meta);
        let ino = shared.ino(&path);
        reply.entry(
            &shared.ttl,
            &shared.attr(ino, Some(&meta)),
            fuser::Generation(0),
        );
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
        flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let shared = &self.shared;
        let (Some(from_parent), Some(to_parent)) = (shared.path(parent), shared.path(newparent))
        else {
            reply.error(Errno::ENOENT);
            return;
        };
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let from = join_rel(&from_parent, name);
        let to = join_rel(&to_parent, newname);
        if shared.ignored(&to) {
            reply.error(Errno::EACCES);
            return;
        }
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
        if noreplace
            && (shared.meta(&to).is_some()
                || (!shared.known_absent(&to_parent, newname)
                    && matches!(shared.stat_remote(&to), Ok(Some(_)))))
        {
            reply.error(Errno::EEXIST);
            return;
        }
        if shared.flush_matching(&from).is_err() || shared.flush_matching(&to).is_err() {
            reply.error(Errno::EIO);
            return;
        }
        let op = JournalOp::Rename {
            from: from.clone(),
            to: to.clone(),
        };
        if let Err(errno) = shared.mutate(op, |client| client.rename(&from, &to)) {
            reply.error(errno);
            return;
        }
        shared.blocks.invalidate_tree(&from);
        shared.blocks.invalidate_tree(&to);
        {
            let mut metas = lock(&shared.metas);
            for key in subtree_keys(&metas, &to) {
                metas.remove(&key);
            }
            for key in subtree_keys(&metas, &from) {
                if let Some(meta) = metas.remove(&key) {
                    metas.insert(format!("{to}{}", &key[from.len()..]), meta);
                }
            }
        }
        {
            let mut dirs = lock(&shared.dirs);
            for key in subtree_keys(&dirs, &to) {
                dirs.remove(&key);
            }
            for key in subtree_keys(&dirs, &from) {
                if let Some(listing) = dirs.remove(&key) {
                    dirs.insert(format!("{to}{}", &key[from.len()..]), listing);
                }
            }
        }
        {
            let mut xattrs = lock(&shared.xattrs);
            xattrs.remove(&to);
            if let Some(attrs) = xattrs.remove(&from) {
                xattrs.insert(to.clone(), attrs);
            }
        }
        lock(&shared.inodes).rename(&from, &to);
        shared.dir_remove(&from);
        match shared.meta(&to) {
            Some(meta) => shared.dir_upsert(&to, &meta),
            None => {
                lock(&shared.dirs).remove(&to_parent);
            }
        }
        reply.ok();
    }
}

impl MobfsFuse {
    fn remove_child(&self, parent: INodeNo, name: &OsStr, dir: bool, reply: ReplyEmpty) {
        let shared = &self.shared;
        let Some(parent_path) = shared.path(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        if shared.ignored(&path) {
            reply.ok();
            return;
        }
        if dir {
            match shared.pool.with(|client| client.list_dir(&path)) {
                Ok(entries) if !entries.is_empty() => {
                    reply.error(Errno::ENOTEMPTY);
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            }
        }
        lock(&shared.write_buffers).retain(|_, pending| {
            pending.path != path && !pending.path.starts_with(&format!("{path}/"))
        });
        let meta = EntryMeta {
            kind: if dir { EntryKind::Dir } else { EntryKind::File },
            size: 0,
            modified: 0,
            sha256: None,
            mode: 0,
            link_target: None,
        };
        let op = JournalOp::Remove {
            path: path.clone(),
            dir,
        };
        if let Err(errno) = shared.mutate(op, |client| client.remove(&path, &meta)) {
            reply.error(errno);
            return;
        }
        shared.blocks.invalidate_tree(&path);
        {
            let mut metas = lock(&shared.metas);
            for key in subtree_keys(&metas, &path) {
                metas.remove(&key);
            }
        }
        {
            let mut xattrs = lock(&shared.xattrs);
            xattrs.retain(|key, _| key != &path && !key.starts_with(&format!("{path}/")));
        }
        shared.dir_remove(&path);
        lock(&shared.inodes).remove_tree(&path);
        reply.ok();
    }
}

fn into_blocks(frames: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let aligned = frames
        .iter()
        .rev()
        .skip(1)
        .all(|frame| frame.len() as u64 == BLOCK_SIZE)
        && frames
            .last()
            .is_none_or(|frame| frame.len() as u64 <= BLOCK_SIZE);
    if aligned {
        return frames;
    }
    frames
        .concat()
        .chunks(BLOCK_SIZE as usize)
        .map(<[u8]>::to_vec)
        .collect()
}

fn send_listing(listing: &[(u64, FileType, String)], offset: u64, mut reply: ReplyDirectory) {
    for (index, (ino, kind, name)) in listing.iter().enumerate().skip(offset as usize) {
        if reply.add(INodeNo(*ino), (index + 1) as u64, *kind, name) {
            break;
        }
    }
    reply.ok();
}

fn file_type(kind: &EntryKind) -> FileType {
    match kind {
        EntryKind::File => FileType::RegularFile,
        EntryKind::Dir => FileType::Directory,
        EntryKind::Symlink => FileType::Symlink,
    }
}

fn file_meta(size: u64, mode: u32) -> EntryMeta {
    EntryMeta {
        kind: EntryKind::File,
        size,
        modified: unix_now_secs(),
        sha256: None,
        mode,
        link_target: None,
    }
}

fn split_parent(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

pub fn pending_journal_ops(config: &AppConfig) -> Result<usize> {
    let path = mountfs_journal_path(config);
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    Ok(data.lines().filter(|line| !line.trim().is_empty()).count())
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

fn blob_dir(journal: &Path) -> PathBuf {
    journal.with_extension("blobs")
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
    let _ = std::fs::remove_dir_all(blob_dir(path));
    Ok(())
}

fn replay_journal(path: &Path, client: &mut RemoteClient) -> Result<()> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for line in data.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(op) = serde_json::from_str::<JournalOp>(line) else {
            crate::ui::warn("skipping unreadable mount journal entry");
            continue;
        };
        match apply_journal_op(path, client, &op) {
            Ok(()) => {}
            Err(crate::error::MobfsError::Server(message)) => {
                crate::ui::warn(format!(
                    "dropping journaled operation rejected by mobfsd: {message}"
                ));
            }
            Err(error) => return Err(error),
        }
    }
    clear_journal(path)
}

fn errno_for_server_error(message: &str) -> Errno {
    let known = [
        ("No such file or directory", Errno::ENOENT),
        ("Permission denied", Errno::EACCES),
        ("not allowed by mobfsd", Errno::EACCES),
        ("File exists", Errno::EEXIST),
        ("Directory not empty", Errno::ENOTEMPTY),
        ("Is a directory", Errno::EISDIR),
        ("Not a directory", Errno::ENOTDIR),
        ("No space left", Errno::ENOSPC),
        ("Read-only file system", Errno::EROFS),
        ("File name too long", Errno::ENAMETOOLONG),
        ("invalid path", Errno::EINVAL),
    ];
    known
        .iter()
        .find(|(needle, _)| message.contains(needle))
        .map(|(_, errno)| *errno)
        .unwrap_or(Errno::EIO)
}

fn apply_journal_op(journal: &Path, client: &mut RemoteClient, op: &JournalOp) -> Result<()> {
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
        JournalOp::WriteAt { path, offset, data } => {
            client.write_file_at(path, *offset, data.clone())
        }
        JournalOp::WriteBlob { path, offset, blob } => {
            if blob.contains('/') || blob.contains("..") {
                return Err(crate::error::MobfsError::InvalidPath(blob.clone()));
            }
            let data = std::fs::read(blob_dir(journal).join(blob))?;
            client.write_file_at(path, *offset, data)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inodes_stay_stable_across_renames() {
        let mut inodes = Inodes::new();
        let dir = inodes.ino("a");
        let file = inodes.ino("a/b.txt");
        let lock_file = inodes.ino("a/config.lock");
        let sibling = inodes.ino("a-b");
        inodes.rename("a/config.lock", "a/config");
        assert_eq!(inodes.path(lock_file).as_deref(), Some("a/config"));
        assert_eq!(inodes.get("a/config.lock"), None);
        assert_ne!(inodes.ino("a/config.lock"), lock_file);
        inodes.rename("a", "z");
        assert_eq!(inodes.path(dir).as_deref(), Some("z"));
        assert_eq!(inodes.path(file).as_deref(), Some("z/b.txt"));
        assert_eq!(inodes.path(sibling).as_deref(), Some("a-b"));
    }

    #[test]
    fn block_cache_evicts_lru_and_honors_generations() {
        let cache = BlockCache::new(BLOCK_SIZE * 4);
        for index in 0..6 {
            cache.insert("clip.mov", index, Arc::new(vec![0; BLOCK_SIZE as usize]), 0);
        }
        assert!(cache.get("clip.mov", 0).is_none());
        assert!(cache.get("clip.mov", 5).is_some());
        let generation = cache.generation("clip.mov");
        cache.invalidate_range("clip.mov", BLOCK_SIZE * 5 + 10, 1);
        assert!(cache.get("clip.mov", 5).is_none());
        assert!(cache.get("clip.mov", 4).is_some());
        cache.insert("clip.mov", 5, Arc::new(vec![1]), generation);
        assert!(cache.get("clip.mov", 5).is_none());
    }
}
