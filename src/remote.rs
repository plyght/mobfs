use crate::config::{AppConfig, StorageBackend};
use crate::crypto::SecureStream;
use crate::daemon;
use crate::error::{MobfsError, Result};
use crate::protocol::{self, PROTOCOL_VERSION, Request, Response};
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use std::fs::{self, File};
use std::io::Write;
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

pub struct RemoteClient {
    config: AppConfig,
    stream: SecureStream,
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
        let stream = TcpStream::connect((config.remote.host.as_str(), port))?;
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
            Response::Hello { version } if version == PROTOCOL_VERSION => {
                Ok(Self { config, stream })
            }
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

    pub fn download_file(&mut self, rel: &str, meta: &EntryMeta) -> Result<()> {
        let local = self.config.local.root.join(rel);
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent)?;
        }
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        let data = match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::ReadFile {
                    root: root.clone(),
                    rel: rel.clone(),
                },
            )
        })? {
            Response::File { data } => data,
            _ => return Err(MobfsError::Remote("invalid read response".to_string())),
        };
        let temp = atomic_temp_path(&local);
        File::create(&temp)?.write_all(&data)?;
        fs::rename(&temp, &local)?;
        daemon::set_mtime(&local, meta.modified)?;
        Ok(())
    }

    pub fn upload_file(&mut self, rel: &str) -> Result<()> {
        let local = self.config.local.root.join(rel);
        let data = fs::read(&local)?;
        let root = self.config.remote.path.clone();
        let rel = rel.to_string();
        self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::WriteFile {
                    root: root.clone(),
                    rel: rel.clone(),
                    data: data.clone(),
                },
            )
        })?;
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
        match self.op(|stream, _| {
            protocol::send(
                stream,
                &Request::Run {
                    root: root.clone(),
                    command: command.clone(),
                },
            )
        })? {
            Response::RunResult {
                code,
                stdout,
                stderr,
            } => Ok((code, stdout, stderr)),
            _ => Err(MobfsError::Remote("invalid run response".to_string())),
        }
    }

    fn reconnect(&mut self) -> Result<()> {
        let next = Self::connect(self.config.clone())?;
        self.stream = next.stream;
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
