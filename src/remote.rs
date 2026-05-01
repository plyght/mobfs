use crate::config::{AppConfig, StorageBackend};
use crate::crypto::SecureStream;
use crate::daemon;
use crate::error::{MobfsError, Result};
use crate::protocol::{self, PROTOCOL_VERSION, Request, Response, RunStream};
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

pub struct RemoteClient {
    config: AppConfig,
    stream: SecureStream,
    tunnel: Option<Child>,
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
        with_backoff(config.sync.connect_retries, || {
            Self::try_connect(config.clone())
        })
    }

    fn try_connect(config: AppConfig) -> Result<Self> {
        if config.remote.backend != StorageBackend::Daemon {
            return Err(MobfsError::Config(format!(
                "backend {:?} is configured but this command needs a live mobfs daemon",
                config.remote.backend
            )));
        }
        let port = config.remote.port;
        let (host, port, tunnel) = if config.remote.ssh_tunnel {
            start_ssh_tunnel(&config.remote.host, port)?
        } else {
            (config.remote.host.clone(), port, None)
        };
        let stream = TcpStream::connect((host.as_str(), port))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
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
            Response::Hello { version } if version == PROTOCOL_VERSION => Ok(Self {
                config,
                stream,
                tunnel,
            }),
            Response::Hello { version } => Err(MobfsError::Remote(format!(
                "protocol version mismatch: client {PROTOCOL_VERSION}, server {version}"
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
            Response::DirEntries(entries) => Ok(entries),
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
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::WriteFileAt {
                    root: root.clone(),
                    rel: rel.clone(),
                    offset,
                    data: data.clone(),
                },
            )
        })?;
        Ok(())
    }

    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub fn truncate(&mut self, rel: &str, size: u64) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Truncate {
                    root: root.clone(),
                    rel: rel.clone(),
                    size,
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
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Rename {
                    root: root.clone(),
                    from: from.clone(),
                    to: to.clone(),
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
            self.op(|stream, _| {
                protocol::send(
                    stream,
                    &Request::Symlink {
                        root: root.clone(),
                        rel: rel.clone(),
                        target: target.clone(),
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
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Mkdir {
                    root: root.clone(),
                    rel: rel.clone(),
                },
            )
        })?;
        Ok(())
    }

    pub fn create_symlink(&mut self, rel: &str, target: &str) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let target = target.to_string();
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Symlink {
                    root: root.clone(),
                    rel: rel.clone(),
                    target: target.clone(),
                },
            )
        })?;
        Ok(())
    }

    pub fn set_metadata(
        &mut self,
        rel: &str,
        mode: Option<u32>,
        modified: Option<i64>,
    ) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::SetMetadata {
                    root: root.clone(),
                    rel: rel.clone(),
                    mode,
                    modified,
                },
            )
        })?;
        Ok(())
    }

    pub fn remove(&mut self, rel: &str, meta: &EntryMeta) -> Result<()> {
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let dir = meta.kind == EntryKind::Dir;
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Remove {
                    root: root.clone(),
                    rel: rel.clone(),
                    dir,
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

    fn reconnect(&mut self) -> Result<()> {
        let mut next = Self::connect(self.config.clone())?;
        std::mem::swap(&mut self.stream, &mut next.stream);
        std::mem::swap(&mut self.tunnel, &mut next.tunnel);
        Ok(())
    }

    fn op<T>(
        &mut self,
        mut action: impl FnMut(&mut SecureStream, &AppConfig) -> Result<T>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            match action(&mut self.stream, &self.config) {
                Ok(value) => return Ok(value),
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

fn start_ssh_tunnel(host: &str, remote_port: u16) -> Result<(String, u16, Option<Child>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let local_port = listener.local_addr()?.port();
    drop(listener);
    let mut child = Command::new("ssh")
        .arg("-N")
        .arg("-L")
        .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"))
        .arg(host)
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
