use crate::config::{AppConfig, StorageBackend};
use crate::daemon;
use crate::error::{MobfsError, Result};
use crate::local;
use crate::remote::RemoteClient;
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub enum StorageClient {
    Daemon(RemoteClient),
    Folder(FolderClient),
}

pub struct FolderClient {
    config: AppConfig,
    root: PathBuf,
}

impl StorageClient {
    pub fn connect(config: AppConfig) -> Result<Self> {
        match config.remote.backend {
            StorageBackend::Daemon => Ok(Self::Daemon(RemoteClient::connect(config)?)),
            StorageBackend::Icloud | StorageBackend::Gdrive => {
                Ok(Self::Folder(FolderClient::new(config)?))
            }
            StorageBackend::R2 | StorageBackend::S3 => Err(MobfsError::Config(format!(
                "backend {} is config-ready but not implemented yet",
                backend_label(&config.remote.backend)
            ))),
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        match self {
            Self::Daemon(client) => client.snapshot(),
            Self::Folder(client) => client.snapshot(),
        }
    }

    pub fn download_file(&mut self, rel: &str, meta: &EntryMeta) -> Result<()> {
        match self {
            Self::Daemon(client) => client.download_file(rel, meta),
            Self::Folder(client) => client.download_file(rel, meta),
        }
    }

    pub fn upload_file(&mut self, rel: &str) -> Result<()> {
        match self {
            Self::Daemon(client) => client.upload_file(rel),
            Self::Folder(client) => client.upload_file(rel),
        }
    }

    pub fn mkdir_p(&mut self, rel: &str) -> Result<()> {
        match self {
            Self::Daemon(client) => client.mkdir_p(rel),
            Self::Folder(client) => client.mkdir_p(rel),
        }
    }

    pub fn remove(&mut self, rel: &str, meta: &EntryMeta) -> Result<()> {
        match self {
            Self::Daemon(client) => client.remove(rel, meta),
            Self::Folder(client) => client.remove(rel, meta),
        }
    }

    pub fn run(&mut self, command: Vec<String>) -> Result<(Option<i32>, Vec<u8>, Vec<u8>)> {
        match self {
            Self::Daemon(client) => client.run(command),
            Self::Folder(_) => Err(MobfsError::Config(
                "remote run needs a live mobfs daemon backend".to_string(),
            )),
        }
    }
}

impl FolderClient {
    fn new(config: AppConfig) -> Result<Self> {
        let root = PathBuf::from(&config.remote.path);
        fs::create_dir_all(&root)?;
        Ok(Self { config, root })
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let mut entries = BTreeMap::new();
        for item in WalkDir::new(&self.root).into_iter() {
            let item = item?;
            let path = item.path();
            if path == self.root {
                continue;
            }
            let rel = local::relative_path(&self.root, path)?;
            if local::should_ignore_rel(&self.config, &rel) || provider_noise(&rel) {
                continue;
            }
            let metadata = item.metadata()?;
            if metadata.is_dir() {
                entries.insert(
                    rel,
                    EntryMeta {
                        kind: EntryKind::Dir,
                        size: 0,
                        modified: 0,
                        sha256: None,
                    },
                );
            } else if metadata.is_file() {
                entries.insert(
                    rel,
                    EntryMeta {
                        kind: EntryKind::File,
                        size: metadata.len(),
                        modified: modified_secs(&metadata),
                        sha256: Some(local::file_sha256(path)?),
                    },
                );
            }
        }
        Ok(Snapshot { entries })
    }

    fn download_file(&self, rel: &str, meta: &EntryMeta) -> Result<()> {
        let src = safe_join(&self.root, rel)?;
        let dst = self.config.local.root.join(rel);
        copy_file_atomic(&src, &dst)?;
        daemon::set_mtime(&dst, meta.modified)?;
        Ok(())
    }

    fn upload_file(&self, rel: &str) -> Result<()> {
        let src = self.config.local.root.join(rel);
        let dst = safe_join(&self.root, rel)?;
        copy_file_atomic(&src, &dst)
    }

    fn mkdir_p(&self, rel: &str) -> Result<()> {
        fs::create_dir_all(safe_join(&self.root, rel.trim_start_matches('/'))?)?;
        Ok(())
    }

    fn remove(&self, rel: &str, meta: &EntryMeta) -> Result<()> {
        let path = safe_join(&self.root, rel)?;
        if meta.kind == EntryKind::Dir && path.exists() {
            fs::remove_dir_all(path)?;
        } else if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

pub fn supported_backends() -> &'static [StorageBackend] {
    &[
        StorageBackend::Daemon,
        StorageBackend::R2,
        StorageBackend::S3,
        StorageBackend::Icloud,
        StorageBackend::Gdrive,
    ]
}

pub fn backend_label(backend: &StorageBackend) -> &'static str {
    match backend {
        StorageBackend::Daemon => "daemon",
        StorageBackend::R2 => "r2",
        StorageBackend::S3 => "s3",
        StorageBackend::Icloud => "icloud",
        StorageBackend::Gdrive => "gdrive",
    }
}

fn copy_file_atomic(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = atomic_temp_path(dst);
    let data = fs::read(src)?;
    fs::File::create(&temp)?.write_all(&data)?;
    fs::rename(temp, dst)?;
    Ok(())
}

fn atomic_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    path.with_file_name(format!(".{name}.mobfs-tmp-{}", std::process::id()))
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in Path::new(rel).components() {
        match component {
            std::path::Component::Normal(value) => path.push(value),
            _ => return Err(MobfsError::InvalidPath(rel.to_string())),
        }
    }
    if !path.starts_with(root) {
        return Err(MobfsError::InvalidPath(rel.to_string()));
    }
    Ok(path)
}

fn modified_secs(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn provider_noise(rel: &str) -> bool {
    rel.ends_with(".icloud")
        || rel.ends_with(".tmp")
        || rel.contains(".tmp.drivedownload")
        || rel.contains(".goutputstream")
        || rel.split('/').any(|part| {
            matches!(
                part,
                ".DS_Store" | ".localized" | "Icon\r" | ".TemporaryItems" | ".Trashes"
            )
        })
}
