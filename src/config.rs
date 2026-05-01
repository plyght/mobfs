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
    pub host: String,
    pub user: String,
    pub path: String,
    pub port: u16,
    pub identity: Option<PathBuf>,
    pub token: Option<String>,
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
    let (host, path) = input
        .split_once(':')
        .ok_or_else(|| MobfsError::InvalidRemote("expected host:/absolute/path".to_string()))?;
    if host.is_empty() || path.is_empty() || !path.starts_with('/') {
        return Err(MobfsError::InvalidRemote(
            "expected host:/absolute/path".to_string(),
        ));
    }
    Ok(RemoteTarget {
        host: host.to_string(),
        path: path.trim_end_matches('/').to_string(),
    })
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
