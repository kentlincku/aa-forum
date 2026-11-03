use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("migration: {0}")]
    Migration(String),
    #[error("config: {0}")]
    Config(String),
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("feature: {0}")]
    Feature(String),
    #[error("event: {0}")]
    Event(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
