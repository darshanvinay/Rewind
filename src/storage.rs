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
    ensure_dir_with(path, &mut sync_dir)
}

// Keep directory-sync failures observable and testable without simulating a
// power loss. The production call always uses the real directory sync above.
fn ensure_dir_with(path: &Path, sync: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(Error::Invalid(format!(
                "expected a non-symlink directory: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_dir_with(parent, sync)?;
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(e)
            if e.kind() == std::io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(path)?.is_dir() => {}
        Err(e) => return Err(e.into()),
    }
    sync(path)?;
    sync(parent)
}

/// Bounded stored-object read. Reject non-regular objects before opening, then
/// check the opened descriptor too. Unix NOFOLLOW/NONBLOCK prevent a replacement
/// symlink or FIFO from redirecting or indefinitely blocking the final open.
pub(crate) fn read_regular_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(Error::Invalid(format!(
            "expected a regular non-symlink file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    let file: File = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?
    .into();
    #[cfg(not(unix))]
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error::Invalid(format!(
            "object is not a regular file within the {limit}-byte limit: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Invalid(format!(
            "object exceeds {limit}-byte limit: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Reuse only a successfully verified object. Only NotFound permits a new write;
/// all other verification failures propagate. A no-clobber race loser must verify
/// the object that appeared before it can report success. No sync policy changes.
pub(crate) fn publish_if_missing(
    mut verify_existing: impl FnMut() -> Result<()>,
    persist_new: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    match verify_existing() {
        Ok(()) => return Ok(()),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    if !persist_new()? {
        verify_existing()?;
    }
    Ok(())
}

/// Returns false if an existing object won publication. Never replaces it.
pub(crate) fn publish(path: &Path, bytes: &[u8]) -> Result<bool> {
    publish_with(path, bytes, || Ok(()))
}

// Private injection point for a failure after file sync but before publication.
fn publish_with(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("missing parent directory".into()))?;
    ensure_dir(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    before_publish()?;
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
        let path = self.chunk_path(&id)?;
        publish_if_missing(|| self.get(&id).map(|_| ()), || publish(&path, data))?;
        Ok(id)
    }
    /// Read a bounded object and verify its digest before returning bytes.
    pub fn get(&self, id: &str) -> Result<Vec<u8>> {
        let data = read_regular_file(&self.chunk_path(id)?, 16 * 1024 * 1024)?;
        if data.is_empty() || data.len() > 16 * 1024 * 1024 || hashing::hash(&data) != id {
            return Err(Error::Corrupt(id.into()));
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_hit_never_invokes_publication() {
        let mut checks = 0;
        publish_if_missing(
            || {
                checks += 1;
                Ok(())
            },
            || panic!("a verified hit must not create or sync a temporary"),
        )
        .unwrap();
        assert_eq!(checks, 1);
    }

    #[test]
    fn corrupt_invalid_and_unreadable_objects_never_invoke_publication() {
        for error in [
            Error::Corrupt("hash mismatch".into()),
            Error::Invalid("not a regular file".into()),
            std::io::Error::from(std::io::ErrorKind::PermissionDenied).into(),
        ] {
            let mut error = Some(error);
            let result = publish_if_missing(
                || Err(error.take().unwrap()),
                || panic!("only NotFound may trigger a new write"),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn missing_object_publishes_once_and_propagates_publication_errors() {
        for fail in [false, true] {
            let mut checks = 0;
            let mut publications = 0;
            let result = publish_if_missing(
                || {
                    checks += 1;
                    Err(std::io::Error::from(std::io::ErrorKind::NotFound).into())
                },
                || {
                    publications += 1;
                    if fail {
                        Err(std::io::Error::other("injected publication failure").into())
                    } else {
                        Ok(true)
                    }
                },
            );
            assert_eq!(result.is_err(), fail);
            assert_eq!((checks, publications), (1, 1));
        }
    }

    #[test]
    fn no_clobber_race_winner_is_verified_and_never_overwritten() {
        for winner in [b"expected".as_slice(), b"corrupt"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("object");
            let expected = b"expected";
            let id = hashing::hash(expected);
            let mut checks = 0;
            let result = publish_if_missing(
                || {
                    checks += 1;
                    let data = read_regular_file(&path, 1024)?;
                    if hashing::hash(&data) != id {
                        return Err(Error::Corrupt(id.clone()));
                    }
                    Ok(())
                },
                || {
                    publish_with(&path, expected, || {
                        // Simulate a competing writer after our verified miss and
                        // file sync but before atomic publication.
                        let mut file = File::create_new(&path)?;
                        file.write_all(winner)?;
                        file.sync_all()?;
                        sync_dir(temp.path())
                    })
                },
            );
            assert_eq!(checks, 2);
            if winner == expected {
                result.unwrap();
            } else {
                assert!(matches!(result, Err(Error::Corrupt(_))));
            }
            assert_eq!(fs::read(&path).unwrap(), winner);
            assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn disappeared_race_winner_is_an_error_without_an_unbounded_retry() {
        let mut checks = 0;
        let result = publish_if_missing(
            || {
                checks += 1;
                Err(std::io::Error::from(std::io::ErrorKind::NotFound).into())
            },
            || Ok(false),
        );
        assert_eq!(checks, 2);
        assert!(
            matches!(result, Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn relative_directory_creation_syncs_dot_and_propagates_failure() {
        // No global cwd change: the unique temporary basename is relative to it.
        let temp = tempfile::Builder::new()
            .prefix(".rewind-sync-test-")
            .tempdir_in(".")
            .unwrap();
        let name = PathBuf::from(temp.path().file_name().unwrap());
        fs::remove_dir(temp.path()).unwrap();
        let mut calls = Vec::new();
        ensure_dir_with(&name, &mut |path| {
            calls.push(path.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [name.clone(), PathBuf::from(".")]);
        fs::remove_dir(&name).unwrap();
        let result = ensure_dir_with(&name, &mut |path| {
            if path == Path::new(".") {
                return Err(std::io::Error::other("injected parent-sync failure").into());
            }
            Ok(())
        });
        assert!(result.is_err());
        temp.close().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn failure_after_tempfile_sync_does_not_publish_snapshot_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        fs::create_dir(temp.path().join("source")).unwrap();
        let first = crate::snapshot::create_snapshot(&store, temp.path().join("source")).unwrap();
        let new_id = hashing::hash(b"injected unpublished record");
        let mut record = serde_json::to_value(&first).unwrap();
        record["id"] = new_id.clone().into();
        let path = store
            .root()
            .join("snapshots")
            .join(format!("{new_id}.json"));
        let result = publish_with(&path, &serde_json::to_vec(&record).unwrap(), || {
            Err(std::io::Error::other("injected failure after file sync").into())
        });
        assert!(result.is_err());
        assert!(!path.exists());
        let records = crate::snapshot::list_snapshots(&store).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id(), first.id());
        assert_eq!(
            fs::read_dir(store.root().join("snapshots"))
                .unwrap()
                .count(),
            1
        );
    }
}
