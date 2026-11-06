//! Versioned file recipes, independently validated before reading chunks.
use crate::{
    Error, Result,
    chunking::ChunkSizes,
    hashing,
    storage::{ChunkStore, publish, publish_if_missing, read_regular_file, sync_dir},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

/// One ordered reference to immutable chunk bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkRef {
    pub hash: String,
    pub length: u32,
}

/// Version 1 recipe with whole-file integrity and chunking provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileManifest {
    pub version: u32,
    pub algorithm: String,
    pub chunking: ChunkSizes,
    pub length: u64,
    pub file_hash: String,
    pub chunks: Vec<ChunkRef>,
}

impl FileManifest {
    /// Validate the supported schema and all reference lengths and identities.
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || self.algorithm != "fastcdc-2020-blake3" {
            return Err(Error::Invalid(
                "unsupported manifest version or algorithm".into(),
            ));
        }
        self.chunking.validate()?;
        hashing::validate_hash(&self.file_hash)?;
        let mut total = 0u64;
        for chunk in &self.chunks {
            hashing::validate_hash(&chunk.hash)?;
            if chunk.length == 0 || chunk.length > self.chunking.max {
                return Err(Error::Invalid("invalid manifest chunk length".into()));
            }
            total = total
                .checked_add(u64::from(chunk.length))
                .ok_or_else(|| Error::Invalid("length overflow".into()))?;
        }
        if total != self.length {
            return Err(Error::Invalid("manifest total length mismatch".into()));
        }
        Ok(())
    }
    /// Read at most 64 MiB of JSON and validate it.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::decode(&read_manifest_bytes(path)?)
    }

    /// Load a store manifest by its BLAKE3 identity, verifying exactly the bytes
    /// that are parsed. Snapshot references must use this instead of path loading.
    pub fn load_by_hash(store: &ChunkStore, id: &str) -> Result<Self> {
        hashing::validate_hash(id)?;
        let bytes = read_manifest_bytes(store.root().join("manifests").join(format!("{id}.json")))?;
        if hashing::hash(&bytes) != id {
            return Err(Error::Corrupt(format!("file manifest {id}")));
        }
        Self::decode(&bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }
    /// Publish serialized JSON under its content identity without replacement.
    pub fn save(&self, store: &ChunkStore) -> Result<PathBuf> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(Error::Invalid("manifest exceeds 64 MiB".into()));
        }
        let path = store
            .root()
            .join("manifests")
            .join(format!("{}.json", hashing::hash(&bytes)));
        publish_if_missing(
            || {
                // Exact equality with these already validated, hashed bytes is
                // stronger than trusting a filename or parsing JSON alone.
                if read_manifest_bytes(&path)? != bytes {
                    return Err(Error::Corrupt(path.display().to_string()));
                }
                Ok(())
            },
            || publish(&path, &bytes),
        )?;
        Ok(path)
    }
}

fn read_manifest_bytes(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    read_regular_file(path.as_ref(), 64 * 1024 * 1024)
}

/// Stream a regular file into the store and return its recipe.
/// Concurrent source modifications are not a point-in-time snapshot.
pub fn store_file(
    store: &ChunkStore,
    path: impl AsRef<Path>,
    sizes: ChunkSizes,
) -> Result<FileManifest> {
    sizes.validate()?;
    store_open_file(store, File::open(path)?, sizes)
}

// Directory capture opens files relative to directory handles with NOFOLLOW.
// Keep the public path-based Phase 1 API unchanged while reusing its pipeline.
pub(crate) fn store_open_file(
    store: &ChunkStore,
    file: File,
    sizes: ChunkSizes,
) -> Result<FileManifest> {
    sizes.validate()?;
    if !file.metadata()?.is_file() {
        return Err(Error::Invalid("input must be a regular file".into()));
    }
    let mut manifest = FileManifest {
        version: 1,
        algorithm: "fastcdc-2020-blake3".into(),
        chunking: sizes,
        length: 0,
        file_hash: String::new(),
        chunks: Vec::new(),
    };
    let mut hasher = blake3::Hasher::new();
    for chunk in fastcdc::v2020::StreamCDC::new(
        file,
        sizes.min as usize,
        sizes.avg as usize,
        sizes.max as usize,
    ) {
        let chunk = chunk.map_err(|e| Error::Chunking(e.to_string()))?;
        hasher.update(&chunk.data);
        let hash = store.put(&chunk.data)?;
        manifest.length += chunk.data.len() as u64;
        manifest.chunks.push(ChunkRef {
            hash,
            length: chunk.data.len() as u32,
        });
    }
    manifest.file_hash = hasher.finalize().to_hex().to_string();
    manifest.validate()?;
    Ok(manifest)
}

/// Verified chunks may already have reached the writer when a later chunk fails.
/// Use restore_file for atomic filesystem output.
pub fn reconstruct(
    store: &ChunkStore,
    manifest: &FileManifest,
    mut output: impl Write,
) -> Result<()> {
    manifest.validate()?;
    let mut hasher = blake3::Hasher::new();
    for chunk in &manifest.chunks {
        let data = store.get(&chunk.hash)?;
        if data.len() != chunk.length as usize {
            return Err(Error::Corrupt(chunk.hash.clone()));
        }
        hasher.update(&data);
        output.write_all(&data)?;
    }
    if hasher.finalize().to_hex().as_str() != manifest.file_hash {
        return Err(Error::Corrupt("whole-file hash mismatch".into()));
    }
    Ok(())
}

/// Verify every referenced chunk, its length, and the complete file digest.
pub fn verify(store: &ChunkStore, manifest: &FileManifest) -> Result<()> {
    reconstruct(store, manifest, std::io::sink())
}

/// Refuses existing output; commits only after every integrity check succeeds.
pub fn restore_file(
    store: &ChunkStore,
    manifest: &FileManifest,
    path: impl AsRef<Path>,
) -> Result<()> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    reconstruct(store, manifest, &mut temp)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|e| Error::Io(e.error))?;
    sync_dir(parent)
}
