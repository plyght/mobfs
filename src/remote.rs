use crate::config::{AppConfig, StorageBackend};
use crate::crypto::SecureStream;
use crate::daemon;
use crate::error::{MobfsError, Result};
use crate::protocol::{self, FsStats, PROTOCOL_VERSION, Request, Response, RunStream};
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use sha2::Digest;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const TRANSFER_CHUNK_SIZE: usize = 1024 * 1024;
const STREAM_WRITE_CHUNK_SIZE: usize = 1024 * 1024;

pub struct RemoteClient {
    config: AppConfig,
    stream: SecureStream,
    tunnel: Option<Child>,
    client_id: u64,
    shared_endpoint: Option<(String, u16)>,
    endpoint: (String, u16),
    op_nonce: u64,
    op_counter: u64,
}

#[cfg_attr(not(feature = "fuse"), allow(dead_code))]
pub struct ChangeBatch {
    pub epoch: u64,
    pub cursor: u64,
    pub events: Vec<crate::protocol::ChangeEvent>,
    pub reset: bool,
    pub live: bool,
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        if let Some(child) = &mut self.tunnel {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl RemoteClient {
    pub fn connect(config: AppConfig) -> Result<Self> {
        Self::connect_with(config, 0, None)
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn connect_with(
        config: AppConfig,
        client_id: u64,
        shared_endpoint: Option<(String, u16)>,
    ) -> Result<Self> {
        with_backoff(config.sync.connect_retries, || {
            Self::try_connect(config.clone(), client_id, shared_endpoint.clone())
        })
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn endpoint(&self) -> (String, u16) {
        self.endpoint.clone()
    }

    fn try_connect(
        config: AppConfig,
        client_id: u64,
        shared_endpoint: Option<(String, u16)>,
    ) -> Result<Self> {
        if config.remote.backend != StorageBackend::Daemon {
            return Err(MobfsError::Config(format!(
                "backend {:?} is configured but this command needs a live mobfs daemon",
                config.remote.backend
            )));
        }
        let port = config.remote.port;
        let shared = shared_endpoint.as_ref().and_then(|(host, port)| {
            TcpStream::connect((host.as_str(), *port))
                .ok()
                .map(|stream| (host.clone(), *port, stream))
        });
        let (host, port, tunnel, stream) = match shared {
            Some((host, port, stream)) => (host, port, None, stream),
            None => {
                let (host, port, tunnel) = if config.remote.ssh_tunnel {
                    start_ssh_tunnel(&config.remote.host, &config.remote.user, port)?
                } else {
                    (config.remote.host.clone(), port, None)
                };
                let stream = TcpStream::connect((host.as_str(), port))?;
                (host, port, tunnel, stream)
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        stream.set_write_timeout(Some(Duration::from_secs(15)))?;
        let token = config
            .remote
            .token
            .clone()
            .or_else(|| std::env::var("MOBFS_TOKEN").ok())
            .ok_or_else(|| {
                MobfsError::Config(
                    "remote token missing; pass --token or set MOBFS_TOKEN".to_string(),
                )
            })?;
        let mut stream = SecureStream::client(stream, &token)?;
        match protocol::send(&mut stream, &Request::Hello)? {
            Response::Hello { version } if version == PROTOCOL_VERSION => {
                if client_id != 0 {
                    protocol::send(&mut stream, &Request::SetClientId { id: client_id })?;
                }
                Ok(Self {
                    config,
                    stream,
                    tunnel,
                    client_id,
                    shared_endpoint,
                    endpoint: (host, port),
                    op_nonce: rand_core::RngCore::next_u64(&mut rand_core::OsRng),
                    op_counter: 0,
                })
            }
            Response::Hello { version } => Err(MobfsError::Remote(format!(
                "protocol version mismatch: client {PROTOCOL_VERSION}, server {version}; install the same mobfs version on both machines, then restart the remote daemon with `mobfs connect ... --restart`"
            ))),
            _ => Err(MobfsError::Remote("invalid hello response".to_string())),
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let root = self.config.remote.path.clone();
        let ignore = self.config.sync.ignore.clone();
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Snapshot {
                    root: root.clone(),
                    ignore: ignore.clone(),
                },
            )
        })? {
            Response::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(MobfsError::Remote("invalid snapshot response".to_string())),
        }
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn stat(&mut self, rel: &str) -> Result<Option<EntryMeta>> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Stat {
                    root: root.clone(),
                    rel: rel.clone(),
                },
            )
        })? {
            Response::Stat(meta) => Ok(meta),
            _ => Err(MobfsError::Remote("invalid stat response".to_string())),
        }
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn list_dir(&mut self, rel: &str) -> Result<Vec<(String, EntryMeta)>> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::ListDir {
                    root: root.clone(),
                    rel: rel.clone(),
                },
            )
        })? {
            Response::DirEntries(entries) => {
                let ignore = self.config.sync.ignore.clone();
                Ok(entries
                    .into_iter()
                    .filter(|(name, _)| !crate::local::should_ignore_part(name, &ignore))
                    .collect())
            }
            _ => Err(MobfsError::Remote("invalid list response".to_string())),
        }
    }

    pub fn read_file_chunk(&mut self, rel: &str, offset: u64, len: u64) -> Result<(Vec<u8>, bool)> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::ReadFileChunk {
                    root: root.clone(),
                    rel: rel.clone(),
                    offset,
                    len,
                },
            )
        })? {
            Response::FileChunk { data, eof } => Ok((data, eof)),
            _ => Err(MobfsError::Remote("invalid read response".to_string())),
        }
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn read_small_files(
        &mut self,
        rels: Vec<String>,
        max_file_bytes: u64,
        max_total_bytes: u64,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let root = self.config.remote.path.clone();
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::ReadSmallFiles {
                    root: root.clone(),
                    rels: rels.clone(),
                    max_file_bytes,
                    max_total_bytes,
                },
            )
        })? {
            Response::SmallFiles(files) => Ok(files),
            _ => Err(MobfsError::Remote(
                "invalid small-files response".to_string(),
            )),
        }
    }

    pub fn download_file(&mut self, rel: &str, meta: &EntryMeta) -> Result<()> {
        let local = self.config.local.root.join(rel);
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent)?;
        }
        if meta.kind == EntryKind::Symlink {
            let target = meta
                .link_target
                .as_ref()
                .ok_or_else(|| MobfsError::Remote("symlink target missing".to_string()))?;
            let _ = fs::remove_file(&local);
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &local)?;
            #[cfg(not(unix))]
            return Err(MobfsError::Remote(
                "symlinks are not supported on this platform".to_string(),
            ));
            return Ok(());
        }
        let rel = rel.to_string();
        let temp = atomic_temp_path(&local);
        let mut file = File::create(&temp)?;
        let mut offset = 0_u64;
        loop {
            let (data, eof) = self.read_file_chunk(&rel, offset, TRANSFER_CHUNK_SIZE as u64)?;
            file.write_all(&data)?;
            offset = offset.saturating_add(data.len() as u64);
            if eof {
                break;
            }
        }
        drop(file);
        fs::rename(&temp, &local)?;
        daemon::set_mode(&local, meta.mode)?;
        daemon::set_mtime(&local, meta.modified)?;
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn write_file_at(&mut self, rel: &str, offset: u64, data: Vec<u8>) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let op_id = self.next_op_id("write-at");
        self.op(|stream, _| {
            protocol::send_with_byte_stream(
                stream,
                &Request::WriteFileAtStream {
                    root: root.clone(),
                    rel: rel.clone(),
                    offset,
                    len: data.len() as u64,
                    op_id: Some(op_id.clone()),
                },
                &data,
                STREAM_WRITE_CHUNK_SIZE,
            )
        })?;
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn truncate(&mut self, rel: &str, size: u64) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let op_id = self.next_op_id("truncate");
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Truncate {
                    root: root.clone(),
                    rel: rel.clone(),
                    size,
                    op_id: Some(op_id.clone()),
                },
            )
        })?;
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn fsync(&mut self, rel: &str) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Fsync {
                    root: root.clone(),
                    rel: rel.clone(),
                },
            )
        })?;
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let root = self.config.remote.path.clone();
        let from = from.to_string();
        let to = to.to_string();
        let op_id = self.next_op_id("rename");
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Rename {
                    root: root.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    op_id: Some(op_id.clone()),
                },
            )
        })?;
        Ok(())
    }

    pub fn upload_file(&mut self, rel: &str) -> Result<()> {
        let local = self.config.local.root.join(rel);
        let metadata = fs::symlink_metadata(&local)?;
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&local)?
                .to_str()
                .ok_or_else(|| MobfsError::InvalidPath(local.display().to_string()))?
                .to_string();
            let op_id = self.next_op_id("symlink");
            self.op(|stream, _| {
                protocol::send(
                    stream,
                    &Request::Symlink {
                        root: root.clone(),
                        rel: rel.clone(),
                        target: target.clone(),
                        op_id: Some(op_id.clone()),
                    },
                )
            })?;
            return Ok(());
        }
        let upload_id = upload_id_for(&rel, &metadata);
        let journal_op = crate::journal::JournalOp::Upload {
            rel: rel.clone(),
            upload_id: upload_id.clone(),
        };
        crate::journal::record(&self.config, journal_op.clone())?;
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::WriteFileStart {
                    root: root.clone(),
                    rel: rel.clone(),
                    upload_id: upload_id.clone(),
                },
            )
        })?;
        let mut file = File::open(&local)?;
        let mut hasher = sha2::Sha256::new();
        let mut offset = match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::WriteFileOffset {
                    root: root.clone(),
                    rel: rel.clone(),
                    upload_id: upload_id.clone(),
                },
            )
        })? {
            Response::FileOffset(value) => value.min(metadata.len()),
            _ => {
                return Err(MobfsError::Remote(
                    "invalid upload offset response".to_string(),
                ));
            }
        };
        let mut buffer = vec![0_u8; TRANSFER_CHUNK_SIZE];
        if offset > 0 {
            let mut remaining = offset;
            while remaining > 0 {
                let read =
                    file.read(&mut buffer[..remaining.min(TRANSFER_CHUNK_SIZE as u64) as usize])?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                remaining = remaining.saturating_sub(read as u64);
            }
        }
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            let data = buffer[..read].to_vec();
            self.op(|stream, _| {
                protocol::send(
                    stream,
                    &Request::WriteFileChunk {
                        root: root.clone(),
                        rel: rel.clone(),
                        upload_id: upload_id.clone(),
                        offset,
                        data: data.clone(),
                    },
                )
            })?;
            offset = offset.saturating_add(read as u64);
        }
        let sha256 = hex::encode(hasher.finalize());
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::WriteFileFinish {
                    root: root.clone(),
                    rel: rel.clone(),
                    upload_id: upload_id.clone(),
                    sha256: sha256.clone(),
                    mode: mode(&metadata),
                },
            )
        })?;
        crate::journal::complete(&self.config, &journal_op)?;
        Ok(())
    }

    pub fn mkdir_p(&mut self, path: &str) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .trim_start_matches('/')
            .to_string();
        let op_id = self.next_op_id("mkdir");
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Mkdir {
                    root: root.clone(),
                    rel: rel.clone(),
                    op_id: Some(op_id.clone()),
                },
            )
        })?;
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn create_symlink(&mut self, rel: &str, target: &str) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let target = target.to_string();
        let op_id = self.next_op_id("symlink");
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Symlink {
                    root: root.clone(),
                    rel: rel.clone(),
                    target: target.clone(),
                    op_id: Some(op_id.clone()),
                },
            )
        })?;
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn set_metadata(
        &mut self,
        rel: &str,
        mode: Option<u32>,
        modified: Option<i64>,
    ) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let op_id = self.next_op_id("metadata");
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::SetMetadata {
                    root: root.clone(),
                    rel: rel.clone(),
                    mode,
                    modified,
                    op_id: Some(op_id.clone()),
                },
            )
        })?;
        Ok(())
    }

    pub fn remove(&mut self, rel: &str, meta: &EntryMeta) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let dir = meta.kind == EntryKind::Dir;
        let op_id = self.next_op_id("remove");
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Remove {
                    root: root.clone(),
                    rel: rel.clone(),
                    dir,
                    op_id: Some(op_id.clone()),
                },
            )
        })?;
        Ok(())
    }

    pub fn run(&mut self, command: Vec<String>) -> Result<(Option<i32>, Vec<u8>, Vec<u8>)> {
        let root = self.config.remote.path.clone();
        self.op(|stream, _| {
            protocol::write_frame(
                stream,
                &Request::Run {
                    root: root.clone(),
                    command: command.clone(),
                },
            )?;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            loop {
                match protocol::read_frame::<Response>(stream)? {
                    Response::RunOutput { stream, data } => match stream {
                        RunStream::Stdout => {
                            print!("{}", String::from_utf8_lossy(&data));
                            stdout.extend(data);
                        }
                        RunStream::Stderr => {
                            eprint!("{}", String::from_utf8_lossy(&data));
                            stderr.extend(data);
                        }
                    },
                    Response::RunResult { code, .. } => return Ok((code, stdout, stderr)),
                    Response::Error { message } => return Err(MobfsError::Remote(message)),
                    _ => return Err(MobfsError::Remote("invalid run response".to_string())),
                }
            }
        })
    }

    pub fn reconnect(&mut self) -> Result<()> {
        let mut next = Self::connect_with(
            self.config.clone(),
            self.client_id,
            self.shared_endpoint.clone(),
        )?;
        std::mem::swap(&mut self.stream, &mut next.stream);
        std::mem::swap(&mut self.tunnel, &mut next.tunnel);
        std::mem::swap(&mut self.endpoint, &mut next.endpoint);
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn read_range(&mut self, rel: &str, offset: u64, len: u64) -> Result<(Vec<u8>, bool)> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        self.op(|stream, _| {
            protocol::send_expecting_bytes(
                stream,
                &Request::ReadRange {
                    root: root.clone(),
                    rel: rel.clone(),
                    offset,
                    len,
                },
            )
        })
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn snapshot_meta(&mut self, max_entries: u64) -> Result<(Snapshot, Vec<String>)> {
        let root = self.config.remote.path.clone();
        let ignore = self.config.sync.ignore.clone();
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::SnapshotMeta {
                    root: root.clone(),
                    ignore: ignore.clone(),
                    max_entries,
                },
            )
        })? {
            Response::SnapshotMeta {
                snapshot,
                complete_dirs,
            } => Ok((snapshot, complete_dirs)),
            _ => Err(MobfsError::Remote("invalid snapshot response".to_string())),
        }
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn watch_changes(
        &mut self,
        epoch: u64,
        since: Option<u64>,
        timeout: Duration,
    ) -> Result<ChangeBatch> {
        let root = self.config.remote.path.clone();
        let ignore = self.config.sync.ignore.clone();
        let wait = timeout.min(Duration::from_secs(20));
        self.stream
            .set_read_timeout(Some(wait + Duration::from_secs(15)))?;
        let result = protocol::send(
            &mut self.stream,
            &Request::WatchChanges {
                root,
                ignore,
                epoch,
                since,
                timeout_ms: wait.as_millis() as u64,
            },
        );
        let _ = self.stream.set_read_timeout(Some(Duration::from_secs(15)));
        match result? {
            Response::Changes {
                epoch,
                cursor,
                events,
                reset,
                live,
            } => Ok(ChangeBatch {
                epoch,
                cursor,
                events,
                reset,
                live,
            }),
            _ => Err(MobfsError::Remote(
                "invalid change feed response".to_string(),
            )),
        }
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn statfs(&mut self) -> Result<FsStats> {
        let root = self.config.remote.path.clone();
        match self
            .op(|stream, _| protocol::send(stream, &Request::StatFs { root: root.clone() }))?
        {
            Response::StatFs(stats) => Ok(stats),
            _ => Err(MobfsError::Remote("invalid statfs response".to_string())),
        }
    }

    pub fn search(&mut self, query: &str, limit: u64) -> Result<Vec<(String, EntryMeta)>> {
        let root = self.config.remote.path.clone();
        let ignore = self.config.sync.ignore.clone();
        let query = query.to_string();
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Search {
                    root: root.clone(),
                    query: query.clone(),
                    ignore: ignore.clone(),
                    limit,
                },
            )
        })? {
            Response::SearchResults(results) => Ok(results),
            _ => Err(MobfsError::Remote("invalid search response".to_string())),
        }
    }

    fn next_op_id(&mut self, kind: &str) -> String {
        self.op_counter += 1;
        op_id_for(
            kind,
            &[&self.op_nonce.to_string(), &self.op_counter.to_string()],
        )
    }

    fn op<T>(
        &mut self,
        mut action: impl FnMut(&mut SecureStream, &AppConfig) -> Result<T>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            match action(&mut self.stream, &self.config) {
                Ok(value) => return Ok(value),
                Err(error @ MobfsError::Server(_)) => return Err(error),
                Err(error) if attempt < self.config.sync.operation_retries => {
                    attempt += 1;
                    crate::ui::warn(format!(
                        "remote operation failed: {error}; reconnecting ({attempt})"
                    ));
                    thread::sleep(backoff(attempt));
                    self.reconnect()?;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn start_ssh_tunnel(
    host: &str,
    user: &str,
    remote_port: u16,
) -> Result<(String, u16, Option<Child>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let local_port = listener.local_addr()?.port();
    drop(listener);
    let ssh_target = if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    };
    let mut child = Command::new("ssh")
        .arg("-N")
        .arg("-L")
        .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"))
        .arg(ssh_target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait()? {
            Some(status) => {
                return Err(MobfsError::Remote(format!(
                    "ssh tunnel exited before local port was reachable: {status}"
                )));
            }
            None => match TcpStream::connect(("127.0.0.1", local_port)) {
                Ok(_) => return Ok(("127.0.0.1".to_string(), local_port, Some(child))),
                Err(error) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(MobfsError::Remote(format!(
                        "ssh tunnel local port 127.0.0.1:{local_port} was not reachable: {error}"
                    )));
                }
                Err(_) => thread::sleep(Duration::from_millis(25)),
            },
        }
    }
}

fn mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn op_id_for(kind: &str, parts: &[&str]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(kind.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn upload_id_for(rel: &str, metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let input = format!("{rel}:{}:{modified}", metadata.len());
    hex::encode(sha2::Sha256::digest(input.as_bytes()))
}

fn atomic_temp_path(path: &std::path::Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    path.with_file_name(format!(".{name}.mobfs-tmp-{}", std::process::id()))
}

fn with_backoff<T>(retries: u32, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 0;
    loop {
        match f() {
            Ok(value) => return Ok(value),
            Err(error) if attempt < retries => {
                attempt += 1;
                crate::ui::warn(format!("connect failed: {error}; retrying ({attempt})"));
                thread::sleep(backoff(attempt));
            }
            Err(error) => return Err(error),
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(2_u64.saturating_pow(attempt.min(5))))
}
