use thiserror::Error;

#[derive(Debug, Error)]
pub enum MobfsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("watch error: {0}")]
    Watch(#[from] notify::Error),

    #[error("toml decode error: {0}")]
    TomlDecode(#[from] toml::de::Error),

    #[error("toml encode error: {0}")]
    TomlEncode(#[from] toml::ser::Error),

    #[error("walkdir error: {0}")]
    Walkdir(#[from] walkdir::Error),

    #[error("invalid remote: {0}")]
    InvalidRemote(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("remote error: {0}")]
    Remote(String),
}

pub type Result<T> = std::result::Result<T, MobfsError>;
