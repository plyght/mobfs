use crate::error::{MobfsError, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const CONFIG_FILE: &str = ".mobfs.toml";
pub const STATE_DIR: &str = ".mobfs";
pub const SNAPSHOT_FILE: &str = "snapshot.toml";
pub const DEFAULT_CONNECT_RETRIES: u32 = 8;
pub const DEFAULT_OP_RETRIES: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub remote: RemoteConfig,
    pub local: LocalConfig,
    pub sync: SyncConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    #[serde(default = "default_backend")]
    pub backend: StorageBackend,
    pub host: String,
    pub user: String,
    pub path: String,
    pub port: u16,
    pub identity: Option<PathBuf>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StorageBackend {
    Daemon,
    R2,
    S3,
    Icloud,
    Gdrive,
}

fn default_backend() -> StorageBackend {
    StorageBackend::Daemon
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalConfig {
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub ignore: Vec<String>,
    pub connect_retries: u32,
    pub operation_retries: u32,
}

pub struct RemoteTarget {
    pub backend: StorageBackend,
    pub host: String,
    pub path: String,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = find_config_path()?;
        let raw = fs::read_to_string(&path)?;
        let mut config: AppConfig = toml::from_str(&raw)?;
        if config.local.root.is_relative() {
            let base = path
                .parent()
                .ok_or_else(|| MobfsError::Config("config path has no parent".to_string()))?;
            config.local.root = base.join(&config.local.root);
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        fs::write(CONFIG_FILE, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn parse_remote(input: &str) -> Result<RemoteTarget> {
    if let Some((scheme, rest)) = input.split_once("://") {
        let backend = match scheme {
            "icloud" => StorageBackend::Icloud,
            "gdrive" | "google-drive" => StorageBackend::Gdrive,
            "r2" => StorageBackend::R2,
            "s3" => StorageBackend::S3,
            "file" => StorageBackend::Icloud,
            _ => {
                return Err(MobfsError::InvalidRemote(format!(
                    "unsupported backend scheme: {scheme}"
                )));
            }
        };
        let path = if rest.starts_with('/') {
            rest.to_string()
        } else {
            format!("/{rest}")
        };
        return Ok(RemoteTarget {
            backend,
            host: scheme.to_string(),
            path: expand_home(&path),
        });
    }

    let (host, path) = input.split_once(':').ok_or_else(|| {
        MobfsError::InvalidRemote(
            "expected host:/absolute/path or backend:///absolute/path".to_string(),
        )
    })?;
    if host.is_empty() || path.is_empty() || !path.starts_with('/') {
        return Err(MobfsError::InvalidRemote(
            "expected host:/absolute/path or backend:///absolute/path".to_string(),
        ));
    }
    Ok(RemoteTarget {
        backend: StorageBackend::Daemon,
        host: host.to_string(),
        path: path.trim_end_matches('/').to_string(),
    })
}

fn expand_home(path: &str) -> String {
    if path == "/~" || path.starts_with("/~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.display(), &path[2..]);
        }
    }
    path.trim_end_matches('/').to_string()
}

fn find_config_path() -> Result<PathBuf> {
    let mut current = std::env::current_dir()?;
    loop {
        let candidate = current.join(CONFIG_FILE);
        if candidate.exists() {
            return Ok(candidate);
        }
        if !current.pop() {
            return Err(MobfsError::Config(format!(
                "{CONFIG_FILE} not found; run mobfs init host:/remote/path"
            )));
        }
    }
}
