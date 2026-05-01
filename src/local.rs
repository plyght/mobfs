use crate::config::{AppConfig, SNAPSHOT_FILE, STATE_DIR};
use crate::error::{MobfsError, Result};
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub fn snapshot(config: &AppConfig) -> Result<Snapshot> {
    let mut entries = std::collections::BTreeMap::new();
    for item in WalkDir::new(&config.local.root).into_iter() {
        let item = item?;
        let path = item.path();
        if path == config.local.root {
            continue;
        }
        let rel = relative_path(&config.local.root, path)?;
        if should_ignore_rel(config, &rel) {
            continue;
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            let target = target
                .to_str()
                .ok_or_else(|| MobfsError::InvalidPath(path.display().to_string()))?
                .to_string();
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::Symlink,
                    size: target.len() as u64,
                    modified: modified_secs(&metadata),
                    sha256: Some(hex::encode(Sha256::digest(target.as_bytes()))),
                    mode: mode(&metadata),
                    link_target: Some(target),
                },
            );
        } else if metadata.is_dir() {
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: 0,
                    sha256: None,
                    mode: mode(&metadata),
                    link_target: None,
                },
            );
        } else if metadata.is_file() {
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::File,
                    size: metadata.len(),
                    modified: modified_secs(&metadata),
                    sha256: Some(file_sha256(path)?),
                    mode: mode(&metadata),
                    link_target: None,
                },
            );
        }
    }
    Ok(Snapshot { entries })
}

pub fn load_snapshot(config: &AppConfig) -> Result<Snapshot> {
    let path = config.local.root.join(STATE_DIR).join(SNAPSHOT_FILE);
    if !path.exists() {
        return Ok(Snapshot::default());
    }
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

pub fn save_snapshot(config: &AppConfig, snapshot: &Snapshot) -> Result<()> {
    let dir = config.local.root.join(STATE_DIR);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(SNAPSHOT_FILE), toml::to_string_pretty(snapshot)?)?;
    Ok(())
}

pub fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| MobfsError::InvalidPath(path.display().to_string()))?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .ok_or_else(|| MobfsError::InvalidPath(path.display().to_string()))?
                    .to_string(),
            ),
            _ => return Err(MobfsError::InvalidPath(path.display().to_string())),
        }
    }
    Ok(parts.join("/"))
}

pub fn should_ignore_rel(config: &AppConfig, rel: &str) -> bool {
    rel.split('/')
        .any(|part| config.sync.ignore.iter().any(|ignore| ignore == part))
}

pub fn should_ignore_path(config: &AppConfig, path: &Path) -> bool {
    relative_path(&config.local.root, path)
        .map(|rel| should_ignore_rel(config, &rel))
        .unwrap_or(true)
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
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

fn modified_secs(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LocalConfig, RemoteConfig, SyncConfig};
    use std::path::PathBuf;

    fn config() -> AppConfig {
        AppConfig {
            remote: RemoteConfig {
                backend: crate::config::StorageBackend::Daemon,
                host: "h".to_string(),
                user: "u".to_string(),
                path: "/r".to_string(),
                port: 22,
                identity: None,
                ssh_tunnel: false,
                token: None,
            },
            local: LocalConfig {
                root: PathBuf::from("."),
            },
            sync: SyncConfig {
                ignore: vec![".git".to_string(), "target".to_string()],
                connect_retries: 1,
                operation_retries: 1,
            },
        }
    }

    #[test]
    fn ignores_configured_segments() {
        let config = config();
        assert!(should_ignore_rel(&config, ".git/config"));
        assert!(should_ignore_rel(&config, "app/target/debug"));
        assert!(!should_ignore_rel(&config, "src/main.rs"));
    }
}
