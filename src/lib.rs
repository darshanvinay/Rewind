//! Bounded-memory chunking, immutable storage, and directory Merkle nodes.
pub mod chunking;
pub mod error;
pub mod hashing;
pub mod manifest;
pub mod snapshot;
pub mod storage;
pub mod tree;
pub use error::{Error, Result};
pub use manifest::{FileManifest, reconstruct, restore_file, store_file, verify};
pub use storage::ChunkStore;
