//! Errors exposed by the storage engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid data: {0}")]
    Invalid(String),
    #[error("chunk integrity check failed: {0}")]
    Corrupt(String),
    #[error("chunking failed: {0}")]
    Chunking(String),
}
pub type Result<T> = std::result::Result<T, Error>;
