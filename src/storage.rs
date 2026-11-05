//! Immutable, single-writer storage with no-clobber publication.
use crate::{Error, Result, hashing};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Local immutable objects. Callers must serialize writers and exclude GC.
#[derive(Debug)]
pub struct ChunkStore {
    root: PathBuf,
}

/// Sync directory entries on Unix. Other platforms lack a portable equivalent.
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn ensure_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_dir(parent)?;
    }
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => (),
        Err(e) => return Err(e.into()),
    }
    sync_dir(path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Returns false if an existing object won publication. Never replaces it.
pub(crate) fn publish(path: &Path, bytes: &[u8]) -> Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("missing parent directory".into()))?;
    ensure_dir(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(path) {
        Ok(_) => {
            sync_dir(parent)?;
            Ok(true)
        }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error.into()),
    }
}

impl ChunkStore {
    /// Create the store directories, or reuse an existing store.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        ensure_dir(&root.join("chunks"))?;
        ensure_dir(&root.join("manifests"))?;
        Ok(Self { root })
    }
    /// Open an initialized store without creating directories.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.join("chunks").is_dir() || !root.join("manifests").is_dir() {
            return Err(Error::Invalid(
                "store is not initialized; run rewind init".into(),
            ));
        }
        Ok(Self { root })
    }
    /// Root containing the chunks and manifests directories.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Deterministic object path for a validated identity.
    pub fn chunk_path(&self, id: &str) -> Result<PathBuf> {
        hashing::validate_hash(id)?;
        Ok(self.root.join("chunks").join(&id[..2]).join(&id[2..]))
    }
    /// Persist a chunk without replacement, verifying any existing object.
    pub fn put(&self, data: &[u8]) -> Result<String> {
        if data.is_empty() || data.len() > 16 * 1024 * 1024 {
            return Err(Error::Invalid("chunk must contain 1 byte to 16 MiB".into()));
        }
        let id = hashing::hash(data);
        if !publish(&self.chunk_path(&id)?, data)? {
            self.get(&id)?;
        }
        Ok(id)
    }
    /// Read a bounded object and verify its digest before returning bytes.
    pub fn get(&self, id: &str) -> Result<Vec<u8>> {
        let file = File::open(self.chunk_path(id)?)?;
        let mut data = Vec::new();
        file.take(16 * 1024 * 1024 + 1).read_to_end(&mut data)?;
        if data.is_empty() || data.len() > 16 * 1024 * 1024 || hashing::hash(&data) != id {
            return Err(Error::Corrupt(id.into()));
        }
        Ok(data)
    }
}
