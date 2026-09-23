use crate::config::{AppConfig, LocalConfig, RemoteConfig, SyncConfig, parse_remote};
use crate::error::{MobfsError, Result};
use crate::protocol::{ChangeEvent, ChangeKind, FsStats};
use crate::remote::RemoteClient;
use crate::snapshot::{EntryKind, EntryMeta};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const BLOCK_SIZE: u64 = 1024 * 1024;
pub const ROOT_INO: u64 = 1;
const WRITE_BUFFER_LIMIT: usize = 8 * 1024 * 1024;
const WRITE_IDLE_FLUSH: Duration = Duration::from_millis(400);
const MAX_INFLIGHT_UPLOAD_BYTES: u64 = 128 * 1024 * 1024;
const PREFETCH_BATCH_BLOCKS: u64 = 8;
const PREFETCH_MAX_FILE_BYTES: u64 = 64 * 1024;
const PREFETCH_MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const SNAPSHOT_MAX_ENTRIES: u64 = 250_000;
#[cfg_attr(not(feature = "fuse"), allow(dead_code))]
const STATFS_TTL: Duration = Duration::from_secs(10);
const FEED_POLL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Fuse,
    Nfs,
}

#[derive(Debug, Clone)]
pub struct MountOptions {
    pub backend: Backend,
    pub connections: usize,
    pub prefetch_connections: usize,
    pub cache_mib: u64,
    pub readahead_mib: u64,
    #[cfg_attr(not(all(feature = "fuse", target_os = "macos")), allow(dead_code))]
    pub volname: Option<String>,
    #[cfg_attr(not(all(feature = "fuse", target_os = "macos")), allow(dead_code))]
    pub fskit: bool,
    pub open_when_ready: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            backend: default_backend(),
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

pub fn default_backend() -> Backend {
    if cfg!(all(
        feature = "nfs",
        any(target_os = "macos", not(feature = "fuse"))
    )) {
        Backend::Nfs
    } else {
        Backend::Fuse
    }
}

pub fn mount(config: AppConfig, mountpoint: PathBuf, options: MountOptions) -> Result<()> {
    match options.backend {
        #[cfg(feature = "fuse")]
        Backend::Fuse => crate::mountfs::mount(config, mountpoint, options),
        #[cfg(feature = "nfs")]
        Backend::Nfs => crate::nfsmount::mount(config, mountpoint, options),
        #[allow(unreachable_patterns)]
        backend => {
            let _ = (config, mountpoint);
            Err(MobfsError::Config(format!(
                "this mobfs build does not include the {backend:?} mount backend"
            )))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    pub const ENOENT: Errno = Errno(libc::ENOENT);
    pub const EIO: Errno = Errno(libc::EIO);
    pub const EINVAL: Errno = Errno(libc::EINVAL);
    pub const EACCES: Errno = Errno(libc::EACCES);
    pub const EEXIST: Errno = Errno(libc::EEXIST);
    pub const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY);
    pub const EISDIR: Errno = Errno(libc::EISDIR);
    pub const ENOTDIR: Errno = Errno(libc::ENOTDIR);
    pub const ENOSPC: Errno = Errno(libc::ENOSPC);
    pub const EROFS: Errno = Errno(libc::EROFS);
    pub const ENAMETOOLONG: Errno = Errno(libc::ENAMETOOLONG);
    pub const ESTALE: Errno = Errno(libc::ESTALE);
    #[cfg(target_os = "linux")]
    pub const NO_XATTR: Errno = Errno(libc::ENODATA);
    #[cfg(not(target_os = "linux"))]
    pub const NO_XATTR: Errno = Errno(libc::ENOATTR);
}

pub type VfsResult<T> = std::result::Result<T, Errno>;

pub type DirItem = (u64, String, EntryMeta);

pub enum ReadData {
    Block(Arc<Vec<u8>>, usize, usize),
    Owned(Vec<u8>),
}

impl ReadData {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            ReadData::Block(block, from, to) => &block[*from..*to],
            ReadData::Owned(data) => data,
        }
    }
}

#[cfg_attr(not(feature = "fuse"), allow(dead_code))]
pub enum Invalidation {
    Inode { ino: u64, offset: i64, len: i64 },
    Entry { parent: u64, name: String },
}

type Invalidator = Box<dyn Fn(Invalidation) + Send + Sync>;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn request_shutdown(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn install_signal_handlers() {
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(signal, request_shutdown as *const () as libc::sighandler_t);
        }
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

pub fn prepare_mountpoint(mountpoint: &Path) -> Result<()> {
    if is_stale_mount(mountpoint) {
        crate::ui::warn(format!(
            "cleaning up stale mount at {}",
            mountpoint.display()
        ));
        force_unmount(mountpoint);
    }
    if mountpoint.exists() && !mountpoint.is_dir() {
        return Err(MobfsError::Config(format!(
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
        return Err(MobfsError::Config(format!(
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
            Some(libc::ENOTCONN) | Some(libc::ENXIO) | Some(libc::EIO) | Some(libc::ESTALE)
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
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = mountpoint.parent() else {
        return false;
    };
    match (std::fs::metadata(mountpoint), std::fs::metadata(parent)) {
        (Ok(mount), Ok(parent)) => mount.dev() != parent.dev(),
        _ => false,
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

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

type Job = Box<dyn FnOnce() + Send + 'static>;

pub(crate) struct WorkerPool {
    sender: mpsc::Sender<Job>,
}

impl WorkerPool {
    pub(crate) fn new(name: &str, threads: usize) -> Self {
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

    pub(crate) fn run(&self, job: impl FnOnce() + Send + 'static) {
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
            None => Err(MobfsError::Remote("connection unavailable".to_string())),
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
        by_path.insert(String::new(), ROOT_INO);
        by_ino.insert(ROOT_INO, String::new());
        Self {
            by_path,
            by_ino,
            next: ROOT_INO + 1,
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
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: Mutex::new(false),
            ready: Condvar::new(),
        })
    }

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
                let flight = Inflight::new();
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
                    let flight = Inflight::new();
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
    touched: Instant,
}

struct ReadAhead {
    next_offset: u64,
    window: u64,
    epoch: u64,
}

type DirEntries = Vec<(String, EntryMeta)>;

struct Upload {
    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
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

pub struct Vfs {
    pool: ClientPool,
    prefetch_pool: ClientPool,
    upload_pool: ClientPool,
    inodes: Mutex<Inodes>,
    metas: Mutex<BTreeMap<String, EntryMeta>>,
    dirs: Mutex<BTreeMap<String, (Instant, DirEntries)>>,
    sidecars: Mutex<HashMap<String, (EntryMeta, Vec<u8>)>>,
    blocks: BlockCache,
    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    next_fh: AtomicU64,
    write_buffers: Mutex<BTreeMap<u64, PendingWrite>>,
    read_ahead: Mutex<HashMap<u64, ReadAhead>>,
    prefetch_queued: Mutex<HashSet<BlockKey>>,
    versions: Mutex<HashMap<u64, (u64, u64)>>,
    xattrs: Mutex<HashMap<String, BTreeMap<String, Vec<u8>>>>,
    statfs: Mutex<Option<(Instant, FsStats)>>,
    feed_live: AtomicBool,
    invalidator: Mutex<Option<Arc<Invalidator>>>,
    stop: AtomicBool,
    journal: PathBuf,
    root_meta: EntryMeta,
    ttl: Duration,
    ignore: Vec<String>,
    readahead_blocks: u64,
    prefetchers: WorkerPool,
    uploaders: WorkerPool,
    uploads: Mutex<Uploads>,
    uploads_changed: Condvar,
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

impl Vfs {
    pub fn start(config: AppConfig, options: &MountOptions) -> Result<Arc<Self>> {
        let ttl = Duration::from_secs(config.sync.cache_ttl_secs.min(60));
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
        let vfs = Arc::new(Self {
            pool,
            prefetch_pool,
            upload_pool,
            inodes: Mutex::new(Inodes::new()),
            metas: Mutex::new(snapshot.entries),
            dirs: Mutex::new(dirs),
            sidecars: Mutex::new(HashMap::new()),
            blocks,
            next_fh: AtomicU64::new(1),
            write_buffers: Mutex::new(BTreeMap::new()),
            read_ahead: Mutex::new(HashMap::new()),
            prefetch_queued: Mutex::new(HashSet::new()),
            versions: Mutex::new(HashMap::new()),
            xattrs: Mutex::new(HashMap::new()),
            statfs: Mutex::new(None),
            feed_live: AtomicBool::new(false),
            invalidator: Mutex::new(None),
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
            prefetchers: WorkerPool::new("mobfs-prefetch", options.prefetch_connections.max(1)),
            uploaders: WorkerPool::new("mobfs-upload", upload_connections),
            uploads: Mutex::new(Uploads::default()),
            uploads_changed: Condvar::new(),
        });
        if let Some((client, epoch, cursor)) = feed {
            let feed_vfs = vfs.clone();
            std::thread::Builder::new()
                .name("mobfs-feed".to_string())
                .spawn(move || run_change_feed(feed_vfs, client, epoch, cursor))?;
        }
        let flusher = vfs.clone();
        std::thread::Builder::new()
            .name("mobfs-flush".to_string())
            .spawn(move || run_idle_flusher(flusher))?;
        Ok(vfs)
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn set_invalidator(&self, invalidator: impl Fn(Invalidation) + Send + Sync + 'static) {
        *lock(&self.invalidator) = Some(Arc::new(Box::new(invalidator)));
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn shutdown(self: &Arc<Self>) {
        self.flush_all_buffers();
        self.stop.store(true, Ordering::SeqCst);
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }

    pub fn path_of(&self, ino: u64) -> VfsResult<String> {
        lock(&self.inodes).path(ino).ok_or(Errno::ESTALE)
    }

    pub fn parent_of(&self, ino: u64) -> u64 {
        match self.path_of(ino) {
            Ok(path) => self.ino(split_parent(&path).0),
            Err(_) => ROOT_INO,
        }
    }

    fn child_path(&self, parent: u64, name: &str) -> VfsResult<String> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(Errno::EINVAL);
        }
        if name.len() > 255 {
            return Err(Errno::ENAMETOOLONG);
        }
        let parent_path = self.path_of(parent)?;
        Ok(join_rel(&parent_path, name))
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

    fn sidecar_meta(&self, path: &str) -> Option<EntryMeta> {
        lock(&self.sidecars).get(path).map(|(meta, _)| meta.clone())
    }

    #[cfg_attr(not(feature = "nfs"), allow(dead_code))]
    pub fn data_version(&self, ino: u64) -> u64 {
        lock(&self.versions).get(&ino).map_or(0, |entry| entry.0)
    }

    fn record(&self, op: &JournalOp) -> VfsResult<()> {
        append_journal(&self.journal, op).map_err(|_| Errno::EIO)
    }

    fn clear_record(&self) -> VfsResult<()> {
        clear_journal(&self.journal).map_err(|_| Errno::EIO)
    }

    fn recover_journal(&self) -> VfsResult<()> {
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
    ) -> VfsResult<()> {
        self.record(&op)?;
        match self.pool.with(action) {
            Ok(()) => {}
            Err(MobfsError::Server(message)) => {
                self.clear_record()?;
                return Err(errno_for_server_error(&message));
            }
            Err(_) => self.recover_journal()?,
        }
        self.clear_record()
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

    fn take_upload_error(&self, fh: u64) -> VfsResult<()> {
        match lock(&self.uploads).errors.remove(&fh) {
            Some(errno) => Err(errno),
            None => Ok(()),
        }
    }

    fn dispatch_fh(self: &Arc<Self>, fh: u64) {
        let pending = lock(&self.write_buffers).remove(&fh);
        if let Some(pending) = pending {
            self.dispatch_pending(fh, pending);
        }
    }

    fn dispatch_pending(self: &Arc<Self>, fh: u64, pending: PendingWrite) {
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
        let vfs = self.clone();
        self.uploaders.run(move || {
            let result = vfs
                .upload_pool
                .with(|client| client.write_file_at(&path, offset, data));
            vfs.blocks.invalidate_range(&path, offset, len);
            let mut uploads = lock(&vfs.uploads);
            uploads.inflight.remove(&id);
            uploads.bytes = uploads.bytes.saturating_sub(len);
            if let Err(error) = result {
                uploads.errors.insert(fh, errno_for(error));
            }
            vfs.uploads_changed.notify_all();
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

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn flush(self: &Arc<Self>, fh: u64) -> VfsResult<()> {
        self.dispatch_fh(fh);
        self.wait_uploads(|upload| upload.fh == fh);
        self.take_upload_error(fh)
    }

    fn flush_matching(self: &Arc<Self>, path: &str) -> VfsResult<()> {
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

    pub fn flush_all_buffers(self: &Arc<Self>) {
        let fhs = lock(&self.write_buffers)
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for fh in fhs {
            self.dispatch_fh(fh);
        }
        self.wait_uploads(|_| true);
    }

    fn flush_idle(self: &Arc<Self>) {
        let idle = lock(&self.write_buffers)
            .iter()
            .filter(|(_, pending)| pending.touched.elapsed() >= WRITE_IDLE_FLUSH)
            .map(|(fh, _)| *fh)
            .collect::<Vec<_>>();
        for fh in idle {
            self.dispatch_fh(fh);
        }
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

    fn stat_remote(&self, path: &str) -> VfsResult<Option<EntryMeta>> {
        let meta = self
            .pool
            .with(|client| client.stat(path))
            .map_err(errno_for)?;
        match &meta {
            Some(meta) if !self.has_pending_write(path) => self.set_meta(path, meta.clone()),
            Some(_) => {}
            None => {
                lock(&self.metas).remove(path);
            }
        }
        Ok(meta)
    }

    fn bump_version(&self, ino: u64) {
        lock(&self.versions).entry(ino).or_insert((0, 0)).0 += 1;
    }

    pub fn lookup_fast(&self, parent: u64, name: &str) -> Option<VfsResult<(u64, EntryMeta)>> {
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(errno) => return Some(Err(errno)),
        };
        if is_sidecar(&path) {
            return Some(
                self.sidecar_meta(&path)
                    .map(|meta| (self.ino(&path), meta))
                    .ok_or(Errno::ENOENT),
            );
        }
        if self.ignored(&path) {
            return Some(Err(Errno::ENOENT));
        }
        if let Some(meta) = self.meta(&path) {
            return Some(Ok((self.ino(&path), meta)));
        }
        let parent_path = split_parent(&path).0.to_string();
        if self.known_absent(&parent_path, name) {
            return Some(Err(Errno::ENOENT));
        }
        None
    }

    pub fn lookup(&self, parent: u64, name: &str) -> VfsResult<(u64, EntryMeta)> {
        if let Some(result) = self.lookup_fast(parent, name) {
            return result;
        }
        let path = self.child_path(parent, name)?;
        match self.stat_remote(&path)? {
            Some(meta) => Ok((self.ino(&path), meta)),
            None => Err(Errno::ENOENT),
        }
    }

    pub fn getattr_fast(&self, ino: u64) -> Option<VfsResult<EntryMeta>> {
        let path = match self.path_of(ino) {
            Ok(path) => path,
            Err(errno) => return Some(Err(errno)),
        };
        if path.is_empty() {
            return Some(Ok(self.root_meta.clone()));
        }
        if is_sidecar(&path) {
            return Some(self.sidecar_meta(&path).ok_or(Errno::ENOENT));
        }
        self.meta(&path).map(Ok)
    }

    pub fn getattr(&self, ino: u64) -> VfsResult<EntryMeta> {
        if let Some(result) = self.getattr_fast(ino) {
            return result;
        }
        let path = self.path_of(ino)?;
        self.stat_remote(&path)?.ok_or(Errno::ENOENT)
    }

    fn build_listing(&self, dir: &str, entries: DirEntries, fresh: bool) -> Vec<DirItem> {
        entries
            .into_iter()
            .map(|(name, meta)| {
                let path = join_rel(dir, &name);
                if fresh && !self.has_pending_write(&path) {
                    self.set_meta(&path, meta.clone());
                }
                let meta = if fresh {
                    meta
                } else {
                    self.meta(&path).unwrap_or(meta)
                };
                (self.ino(&path), name, meta)
            })
            .collect()
    }

    pub fn list_dir_fast(&self, ino: u64) -> Option<VfsResult<Vec<DirItem>>> {
        let dir = match self.path_of(ino) {
            Ok(dir) => dir,
            Err(errno) => return Some(Err(errno)),
        };
        let entries = self.dir_listing(&dir)?;
        Some(Ok(self.build_listing(&dir, entries, false)))
    }

    pub fn list_dir(&self, ino: u64) -> VfsResult<Vec<DirItem>> {
        if let Some(result) = self.list_dir_fast(ino) {
            return result;
        }
        let dir = self.path_of(ino)?;
        let entries = self
            .pool
            .with(|client| client.list_dir(&dir))
            .map_err(errno_for)?;
        lock(&self.dirs).insert(dir.clone(), (Instant::now(), entries.clone()));
        Ok(self.build_listing(&dir, entries, true))
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn open(&self, ino: u64) -> VfsResult<(u64, bool)> {
        self.path_of(ino)?;
        let mut versions = lock(&self.versions);
        let entry = versions.entry(ino).or_insert((0, u64::MAX));
        let keep_cache = entry.0 == entry.1;
        entry.1 = entry.0;
        Ok((self.alloc_fh(), keep_cache))
    }

    pub fn readlink(&self, ino: u64) -> VfsResult<String> {
        self.getattr(ino)?.link_target.ok_or(Errno::EINVAL)
    }

    pub fn read(self: &Arc<Self>, ino: u64, offset: u64, size: u32) -> VfsResult<ReadData> {
        let path = self.path_of(ino)?;
        if path.is_empty() {
            return Err(Errno::EISDIR);
        }
        if is_sidecar(&path) {
            let sidecars = lock(&self.sidecars);
            let (_, data) = sidecars.get(&path).ok_or(Errno::ENOENT)?;
            let start = (offset as usize).min(data.len());
            let end = start.saturating_add(size as usize).min(data.len());
            return Ok(ReadData::Owned(data[start..end].to_vec()));
        }
        self.flush_matching(&path)?;
        let file_size = self.meta(&path).map(|meta| meta.size);
        if file_size.is_some_and(|len| offset >= len) {
            return Ok(ReadData::Owned(Vec::new()));
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
                Err(error) if out.is_empty() && single.is_none() => return Err(errno_for(error)),
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
        Ok(match single {
            Some((block, from, to)) => ReadData::Block(block, from, to),
            None => ReadData::Owned(out),
        })
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
            let vfs = self.clone();
            let path = path.to_string();
            self.prefetchers.run(move || {
                let current_epoch = lock(&vfs.read_ahead).get(&ino).map(|state| state.epoch);
                if current_epoch == Some(epoch) && !vfs.stop.load(Ordering::SeqCst) {
                    let mut index = run_first;
                    while index < end {
                        if vfs.blocks.contains(&path, index) {
                            index += 1;
                            continue;
                        }
                        let mut stop = index;
                        while stop < end && !vfs.blocks.contains(&path, stop) {
                            stop += 1;
                        }
                        vfs.blocks
                            .fetch_run(&path, index, stop - index, &vfs.prefetch_pool);
                        index = stop;
                    }
                }
                lock(&vfs.prefetch_queued).remove(&(path.clone(), run_first));
            });
        }
    }

    pub fn write(self: &Arc<Self>, ino: u64, fh: u64, offset: u64, data: &[u8]) -> VfsResult<()> {
        let path = self.path_of(ino)?;
        if path.is_empty() {
            return Err(Errno::EISDIR);
        }
        if is_sidecar(&path) {
            let mut sidecars = lock(&self.sidecars);
            let (meta, stored) = sidecars.get_mut(&path).ok_or(Errno::ENOENT)?;
            let end = offset as usize + data.len();
            if stored.len() < end {
                stored.resize(end, 0);
            }
            stored[offset as usize..end].copy_from_slice(data);
            meta.size = stored.len() as u64;
            meta.modified = unix_now_secs();
            return Ok(());
        }
        if self.ignored(&path) {
            return Err(Errno::EACCES);
        }
        self.take_upload_error(fh)?;
        let displaced = {
            let mut buffers = lock(&self.write_buffers);
            match buffers.get_mut(&fh) {
                Some(pending)
                    if pending.ino == ino
                        && pending.offset.saturating_add(pending.data.len() as u64) == offset
                        && pending.data.len().saturating_add(data.len()) <= WRITE_BUFFER_LIMIT =>
                {
                    pending.data.extend_from_slice(data);
                    pending.touched = Instant::now();
                    None
                }
                _ => buffers.insert(
                    fh,
                    PendingWrite {
                        ino,
                        path: path.clone(),
                        offset,
                        data: data.to_vec(),
                        touched: Instant::now(),
                    },
                ),
            }
        };
        if let Some(previous) = displaced {
            self.dispatch_pending(fh, previous);
        }
        self.blocks
            .invalidate_range(&path, offset, data.len() as u64);
        self.note_local_write(&path, offset, data.len() as u64);
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn fsync(self: &Arc<Self>, ino: u64, fh: u64) -> VfsResult<()> {
        let path = self.path_of(ino)?;
        if is_sidecar(&path) {
            return Ok(());
        }
        self.flush(fh)?;
        self.flush_matching(&path)?;
        self.pool
            .with(|client| client.fsync(&path))
            .map_err(errno_for)
    }

    pub fn setattr(
        self: &Arc<Self>,
        ino: u64,
        mode: Option<u32>,
        size: Option<u64>,
        modified: Option<i64>,
    ) -> VfsResult<EntryMeta> {
        let path = self.path_of(ino)?;
        if path.is_empty() {
            return Ok(self.root_meta.clone());
        }
        if is_sidecar(&path) {
            let mut sidecars = lock(&self.sidecars);
            let (meta, data) = sidecars.get_mut(&path).ok_or(Errno::ENOENT)?;
            if let Some(size) = size {
                data.resize(size as usize, 0);
                meta.size = size;
            }
            if let Some(mode) = mode {
                meta.mode = mode & 0o7777;
            }
            if let Some(modified) = modified {
                meta.modified = modified;
            }
            return Ok(meta.clone());
        }
        self.flush_matching(&path)?;
        if let Some(size) = size {
            let op = JournalOp::Truncate {
                path: path.clone(),
                size,
            };
            self.mutate(op, |client| client.truncate(&path, size))?;
            self.blocks.invalidate_path(&path);
            let mut metas = lock(&self.metas);
            let entry = metas
                .entry(path.clone())
                .or_insert_with(|| file_meta(size, 0o644));
            entry.size = size;
            entry.modified = unix_now_secs();
        }
        let mode = mode.map(|mode| mode & 0o7777);
        if mode.is_some() || modified.is_some() {
            let op = JournalOp::SetMetadata {
                path: path.clone(),
                mode,
                modified,
            };
            self.mutate(op, |client| client.set_metadata(&path, mode, modified))?;
            if let Some(entry) = lock(&self.metas).get_mut(&path) {
                if let Some(mode) = mode {
                    entry.mode = mode;
                }
                if let Some(modified) = modified {
                    entry.modified = modified;
                }
            }
        }
        match self.meta(&path) {
            Some(meta) => Ok(meta),
            None => self.stat_remote(&path)?.ok_or(Errno::ENOENT),
        }
    }

    pub fn create(
        self: &Arc<Self>,
        parent: u64,
        name: &str,
        mode: u32,
        exclusive: bool,
    ) -> VfsResult<(u64, EntryMeta)> {
        let path = self.child_path(parent, name)?;
        let mode = mode & 0o7777;
        if is_sidecar(&path) {
            let mut sidecars = lock(&self.sidecars);
            if exclusive && sidecars.contains_key(&path) {
                return Err(Errno::EEXIST);
            }
            let meta = file_meta(0, mode);
            sidecars.insert(path.clone(), (meta.clone(), Vec::new()));
            drop(sidecars);
            return Ok((self.ino(&path), meta));
        }
        if self.ignored(&path) {
            return Err(Errno::EACCES);
        }
        if exclusive
            && (self.meta(&path).is_some()
                || (!self.known_absent(split_parent(&path).0, name)
                    && self.stat_remote(&path)?.is_some()))
        {
            return Err(Errno::EEXIST);
        }
        let op = JournalOp::Truncate {
            path: path.clone(),
            size: 0,
        };
        self.mutate(op, |client| {
            client.truncate(&path, 0)?;
            client.set_metadata(&path, Some(mode), None)
        })?;
        let meta = file_meta(0, mode);
        self.blocks.invalidate_path(&path);
        self.set_meta(&path, meta.clone());
        self.dir_upsert(&path, &meta);
        let ino = self.ino(&path);
        self.bump_version(ino);
        Ok((ino, meta))
    }

    pub fn mkdir(&self, parent: u64, name: &str, mode: u32) -> VfsResult<(u64, EntryMeta)> {
        let path = self.child_path(parent, name)?;
        if self.ignored(&path) {
            return Err(Errno::EACCES);
        }
        let mode = mode & 0o7777;
        let op = JournalOp::Mkdir { path: path.clone() };
        self.mutate(op, |client| {
            client.mkdir_p(&path)?;
            client.set_metadata(&path, Some(mode), None)
        })?;
        let meta = EntryMeta {
            kind: EntryKind::Dir,
            size: 0,
            modified: unix_now_secs(),
            sha256: None,
            mode,
            link_target: None,
        };
        self.set_meta(&path, meta.clone());
        self.dir_upsert(&path, &meta);
        lock(&self.dirs).insert(path.clone(), (Instant::now(), Vec::new()));
        Ok((self.ino(&path), meta))
    }

    pub fn symlink(&self, parent: u64, name: &str, target: &str) -> VfsResult<(u64, EntryMeta)> {
        let path = self.child_path(parent, name)?;
        if self.ignored(&path) {
            return Err(Errno::EACCES);
        }
        let op = JournalOp::Symlink {
            path: path.clone(),
            target: target.to_string(),
        };
        self.mutate(op, |client| client.create_symlink(&path, target))?;
        let meta = EntryMeta {
            kind: EntryKind::Symlink,
            size: target.len() as u64,
            modified: unix_now_secs(),
            sha256: None,
            mode: 0o777,
            link_target: Some(target.to_string()),
        };
        self.set_meta(&path, meta.clone());
        self.dir_upsert(&path, &meta);
        Ok((self.ino(&path), meta))
    }

    pub fn remove(self: &Arc<Self>, parent: u64, name: &str, dir: Option<bool>) -> VfsResult<()> {
        let path = self.child_path(parent, name)?;
        if is_sidecar(&path) {
            lock(&self.sidecars).remove(&path);
            lock(&self.inodes).remove_tree(&path);
            return Ok(());
        }
        if self.ignored(&path) {
            return Ok(());
        }
        let dir = match dir {
            Some(dir) => dir,
            None => self.lookup(parent, name)?.1.kind == EntryKind::Dir,
        };
        if dir {
            let entries = self
                .pool
                .with(|client| client.list_dir(&path))
                .map_err(errno_for)?;
            if !entries.is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
        }
        lock(&self.write_buffers).retain(|_, pending| {
            pending.path != path && !pending.path.starts_with(&format!("{path}/"))
        });
        self.wait_uploads(|upload| upload.path == path);
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
        self.mutate(op, |client| client.remove(&path, &meta))?;
        self.blocks.invalidate_tree(&path);
        {
            let mut metas = lock(&self.metas);
            for key in subtree_keys(&metas, &path) {
                metas.remove(&key);
            }
        }
        lock(&self.xattrs).retain(|key, _| key != &path && !key.starts_with(&format!("{path}/")));
        self.dir_remove(&path);
        lock(&self.inodes).remove_tree(&path);
        Ok(())
    }

    pub fn rename(
        self: &Arc<Self>,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        noreplace: bool,
    ) -> VfsResult<()> {
        let from = self.child_path(parent, name)?;
        let to = self.child_path(newparent, newname)?;
        match (is_sidecar(&from), is_sidecar(&to)) {
            (true, true) => {
                let mut sidecars = lock(&self.sidecars);
                let entry = sidecars.remove(&from).ok_or(Errno::ENOENT)?;
                if noreplace && sidecars.contains_key(&to) {
                    sidecars.insert(from, entry);
                    return Err(Errno::EEXIST);
                }
                sidecars.insert(to.clone(), entry);
                drop(sidecars);
                lock(&self.inodes).rename(&from, &to);
                return Ok(());
            }
            (false, false) => {}
            _ => return Err(Errno::EACCES),
        }
        if self.ignored(&to) {
            return Err(Errno::EACCES);
        }
        let to_parent = split_parent(&to).0.to_string();
        if noreplace
            && (self.meta(&to).is_some()
                || (!self.known_absent(&to_parent, newname)
                    && matches!(self.stat_remote(&to), Ok(Some(_)))))
        {
            return Err(Errno::EEXIST);
        }
        self.flush_matching(&from)?;
        self.flush_matching(&to)?;
        let op = JournalOp::Rename {
            from: from.clone(),
            to: to.clone(),
        };
        self.mutate(op, |client| client.rename(&from, &to))?;
        self.blocks.invalidate_tree(&from);
        self.blocks.invalidate_tree(&to);
        {
            let mut metas = lock(&self.metas);
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
            let mut dirs = lock(&self.dirs);
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
            let mut xattrs = lock(&self.xattrs);
            xattrs.remove(&to);
            if let Some(attrs) = xattrs.remove(&from) {
                xattrs.insert(to.clone(), attrs);
            }
        }
        lock(&self.inodes).rename(&from, &to);
        self.dir_remove(&from);
        match self.meta(&to) {
            Some(meta) => self.dir_upsert(&to, &meta),
            None => {
                lock(&self.dirs).remove(&to_parent);
            }
        }
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn statfs(&self) -> FsStats {
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

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn setxattr(
        &self,
        ino: u64,
        name: &str,
        value: &[u8],
        flags: i32,
        position: u32,
    ) -> VfsResult<()> {
        let path = self.path_of(ino)?;
        let mut xattrs = lock(&self.xattrs);
        let attrs = xattrs.entry(path).or_default();
        let exists = attrs.contains_key(name);
        if flags & libc::XATTR_CREATE != 0 && exists {
            return Err(Errno::EEXIST);
        }
        if flags & libc::XATTR_REPLACE != 0 && !exists {
            return Err(Errno::NO_XATTR);
        }
        let stored = attrs.entry(name.to_string()).or_default();
        let position = position as usize;
        if position == 0 {
            *stored = value.to_vec();
        } else {
            if stored.len() < position + value.len() {
                stored.resize(position + value.len(), 0);
            }
            stored[position..position + value.len()].copy_from_slice(value);
        }
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn getxattr(&self, ino: u64, name: &str) -> VfsResult<Vec<u8>> {
        let path = self.path_of(ino)?;
        lock(&self.xattrs)
            .get(&path)
            .and_then(|attrs| attrs.get(name).cloned())
            .ok_or(Errno::NO_XATTR)
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn listxattr(&self, ino: u64) -> VfsResult<Vec<u8>> {
        let path = self.path_of(ino)?;
        let mut names = Vec::new();
        if let Some(attrs) = lock(&self.xattrs).get(&path) {
            for name in attrs.keys() {
                names.extend_from_slice(name.as_bytes());
                names.push(0);
            }
        }
        Ok(names)
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn removexattr(&self, ino: u64, name: &str) -> VfsResult<()> {
        let path = self.path_of(ino)?;
        lock(&self.xattrs)
            .get_mut(&path)
            .and_then(|attrs| attrs.remove(name))
            .map(|_| ())
            .ok_or(Errno::NO_XATTR)
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
        let mut invalidations = Vec::new();
        match &event.kind {
            ChangeKind::Write { offset, len } => {
                self.blocks.invalidate_range(path, *offset, *len);
                lock(&self.metas).remove(path);
                if let Some(ino) = ino {
                    self.bump_version(ino);
                    invalidations.push(Invalidation::Inode {
                        ino,
                        offset: *offset as i64,
                        len: *len as i64,
                    });
                }
            }
            ChangeKind::Metadata => {
                lock(&self.metas).remove(path);
                if let Some(ino) = ino {
                    invalidations.push(Invalidation::Inode {
                        ino,
                        offset: -1,
                        len: 0,
                    });
                }
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
                if let Some(ino) = ino {
                    invalidations.push(Invalidation::Inode {
                        ino,
                        offset: 0,
                        len: 0,
                    });
                }
                if let Some(parent) = parent_ino {
                    invalidations.push(Invalidation::Entry {
                        parent,
                        name: name.to_string(),
                    });
                }
            }
        }
        lock(&self.dirs).remove(parent);
        let invalidator = lock(&self.invalidator).clone();
        if let Some(invalidator) = invalidator {
            for invalidation in invalidations {
                invalidator(invalidation);
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

fn run_change_feed(vfs: Arc<Vfs>, mut client: RemoteClient, mut epoch: u64, mut cursor: u64) {
    let mut failures = 0_u32;
    while !vfs.stop.load(Ordering::SeqCst) {
        match client.watch_changes(epoch, Some(cursor), FEED_POLL) {
            Ok(batch) => {
                failures = 0;
                if batch.reset {
                    vfs.invalidate_all();
                } else {
                    for event in &batch.events {
                        vfs.apply_change(event);
                    }
                }
                vfs.feed_live.store(batch.live, Ordering::SeqCst);
                epoch = batch.epoch;
                cursor = batch.cursor;
            }
            Err(_) => {
                vfs.feed_live.store(false, Ordering::SeqCst);
                failures = failures.saturating_add(1);
                std::thread::sleep(Duration::from_millis(250 * u64::from(failures.min(20))));
                let _ = client.reconnect();
            }
        }
    }
}

fn run_idle_flusher(vfs: Arc<Vfs>) {
    while !vfs.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
        vfs.flush_idle();
    }
}

pub fn is_sidecar(path: &str) -> bool {
    let name = split_parent(path).1;
    name.starts_with("._") || name == ".DS_Store"
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

pub fn split_parent(path: &str) -> (&str, &str) {
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
            Err(MobfsError::Server(message)) => {
                crate::ui::warn(format!(
                    "dropping journaled operation rejected by mobfsd: {message}"
                ));
            }
            Err(error) => return Err(error),
        }
    }
    clear_journal(path)
}

fn errno_for(error: MobfsError) -> Errno {
    match error {
        MobfsError::Server(message) => errno_for_server_error(&message),
        _ => Errno::EIO,
    }
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
                return Err(MobfsError::InvalidPath(blob.clone()));
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

pub fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
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

    #[test]
    fn sidecars_cover_apple_metadata_files() {
        assert!(is_sidecar("media/._clip.mov"));
        assert!(is_sidecar(".DS_Store"));
        assert!(is_sidecar("a/b/.DS_Store"));
        assert!(!is_sidecar("media/clip.mov"));
        assert!(!is_sidecar("media/_clip.mov"));
    }
}
