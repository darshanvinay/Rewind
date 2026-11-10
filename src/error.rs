//! Errors exposed by the storage engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON metadata error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid data: {0}")]
    Invalid(String),
    #[error("content integrity check failed: {0}")]
    Corrupt(String),
    #[error("source filesystem error: {0}")]
    SourceIo(std::io::Error),
    #[error("source changed during capture: {0}")]
    Unstable(String),
    #[error("capture cancelled")]
    Cancelled,
    #[error("chunking failed: {0}")]
    Chunking(String),
}
pub type Result<T> = std::result::Result<T, Error>;
