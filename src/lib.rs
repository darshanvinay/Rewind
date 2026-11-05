//! Phase 1: bounded-memory chunking and immutable, verified local storage.
pub mod chunking;
pub mod error;
pub mod hashing;
pub mod manifest;
pub mod storage;
pub use error::{Error, Result};
pub use manifest::{FileManifest, reconstruct, restore_file, store_file, verify};
pub use storage::ChunkStore;
