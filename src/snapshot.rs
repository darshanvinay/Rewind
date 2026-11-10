//! Manual captures and validated history records, published after the object graph.
use crate::{ChunkStore, Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;

const MAX_RECORD_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotWire {
    version: u32,
    id: String,
    root_hash: String,
    source_path: String,
    created_unix_seconds: u64,
    created_nanoseconds: u32,
}

/// A committed capture. Source path and timestamp are history metadata, not part
/// of the immutable directory root. Fields cannot be changed through this API.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRecord {
    version: u32,
    id: String,
    root_hash: String,
    source_path: String,
    created_unix_seconds: u64,
    created_nanoseconds: u32,
}

impl SnapshotRecord {
    /// Load one bounded record and validate it against its requested filename ID.
    /// This validates metadata only; restoration verifies the referenced graph.
    pub fn load(store: &ChunkStore, id: &str) -> Result<Self> {
        crate::hashing::validate_hash(id)?;
        let path = store.root().join("snapshots").join(format!("{id}.json"));
        let bytes = crate::storage::read_regular_file(&path, MAX_RECORD_BYTES as u64)?;
        let raw: SnapshotWire = serde_json::from_slice(&bytes)?;
        crate::hashing::validate_hash(&raw.id)?;
        crate::hashing::validate_hash(&raw.root_hash)?;
        if raw.version != 1 || raw.id != id || raw.created_nanoseconds >= 1_000_000_000 {
            return Err(Error::Invalid(format!(
                "invalid or unsupported snapshot record {id}"
            )));
        }
        // Version 1 captures originate on Unix. The path is display metadata,
        // never an instruction to open or create a source/destination directory.
        if !raw.source_path.starts_with('/')
            || raw.source_path.contains('\0')
            || raw
                .source_path
                .split('/')
                .any(|part| part == "." || part == "..")
        {
            return Err(Error::Invalid(
                "snapshot source path must be absolute without dot components or NUL".into(),
            ));
        }
        Ok(Self {
            version: raw.version,
            id: raw.id,
            root_hash: raw.root_hash,
            source_path: raw.source_path,
            created_unix_seconds: raw.created_unix_seconds,
            created_nanoseconds: raw.created_nanoseconds,
        })
    }

    /// Opaque lowercase hexadecimal ID, distinct even for identical tree roots.
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Content identity of the complete captured directory tree.
    pub fn root_hash(&self) -> &str {
        &self.root_hash
    }
    /// Recorded canonical absolute UTF-8 source path, never a restore destination.
    pub fn source_path(&self) -> &str {
        &self.source_path
    }
    /// Completion timestamp as (seconds since Unix epoch, fractional nanoseconds).
    pub fn created_at(&self) -> (u64, u32) {
        (self.created_unix_seconds, self.created_nanoseconds)
    }
}

/// List validated committed records oldest first (timestamp, then ID).
/// An initialized store without a snapshots directory has an empty history.
/// Temporary/non-record filenames are ignored; invalid recognized records fail.
/// This is not a full content-integrity scan.
pub fn list_snapshots(store: &ChunkStore) -> Result<Vec<SnapshotRecord>> {
    let directory = store.root().join("snapshots");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => (),
        Ok(_) => {
            return Err(Error::Invalid(
                "snapshot history is not a regular directory".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    let entries = fs::read_dir(directory)?;
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        if crate::hashing::validate_hash(id).is_err() {
            continue;
        }
        records.push(SnapshotRecord::load(store, id)?);
    }
    records.sort_by(|a, b| (a.created_at(), a.id()).cmp(&(b.created_at(), b.id())));
    Ok(records)
}

/// Capture regular files and directories with a full traversal. Reject symlinks,
/// special files, unsupported names, store roots, and depth beyond 256 levels.
/// The actual store is excluded by filesystem identity if nested in the source.
/// Writers are serialized by an advisory lock; a trusted store is required. No point-in-time consistency
/// with concurrent source edits is provided. Unix uses descriptor-relative reads;
/// other platforms return an explicit unsupported error without writing anything.
pub fn create_snapshot(
    store: &ChunkStore,
    source: impl AsRef<std::path::Path>,
) -> Result<SnapshotRecord> {
    #[cfg(unix)]
    {
        unix::create(store, source.as_ref())
    }
    #[cfg(not(unix))]
    {
        let _ = (store, source);
        Err(Error::Invalid(
            "snapshot creation is currently supported only on Unix".into(),
        ))
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::{
        chunking::ChunkSizes,
        hashing,
        manifest::store_open_file,
        storage::ensure_dir,
        tree::{DirectoryEntry, DirectoryNode, EntryTarget, validate_name},
    };
    use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags};
    use std::{
        ffi::CStr,
        fs::{self, File},
        os::unix::fs::MetadataExt,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    // Bounds recursion, open directory descriptors, and the stack. Root is level 0.
    const MAX_DEPTH: usize = 256;

    fn io_at(path: &Path, error: impl Into<std::io::Error>) -> Error {
        let error = error.into();
        Error::Io(std::io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        ))
    }

    pub(super) fn create(store: &ChunkStore, source: &Path) -> Result<SnapshotRecord> {
        // Components strip trailing '/' and '/.' so NOFOLLOW checks the root
        // itself rather than an implicit lookup through a symlink root.
        let source: PathBuf = source.components().collect();
        let root: File = rfs::openat(
            rfs::CWD,
            &source,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io_at(&source, e))?
        .into();
        let source = fs::canonicalize(&source).map_err(|e| io_at(&source, e))?;
        let root_meta = root.metadata()?;
        let path_meta = fs::metadata(&source)?;
        if (root_meta.dev(), root_meta.ino()) != (path_meta.dev(), path_meta.ino()) {
            return Err(Error::Invalid(
                "source directory changed while opening it".into(),
            ));
        }
        let source_path = source
            .to_str()
            .ok_or_else(|| Error::Invalid("snapshot source path must be UTF-8".into()))?
            .to_owned();
        let store_path = fs::canonicalize(store.root())?;
        if source.starts_with(&store_path) {
            return Err(Error::Invalid(
                "cannot snapshot the store or a directory inside it".into(),
            ));
        }
        let store = ChunkStore::open(store_path)?.begin_capture()?;
        let store_meta = fs::metadata(store.root())?;
        let identity = (store_meta.dev(), store_meta.ino());
        let root_hash = capture_directory(&store, &root, &source, identity, 0)?;
        store.object_barrier()?;

        // Only now create/publish history metadata. Failed captures may leave
        // unreferenced objects, but no committed record pointing to partial work.
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Invalid("system clock is before the Unix epoch".into()))?;
        let records = store.root().join("snapshots");
        ensure_dir(&records)?;
        // An atomically allocated random tempfile name provides independent
        // entropy without another RNG dependency. The file is never a record.
        let nonce = tempfile::Builder::new()
            .prefix(".pending-")
            .rand_bytes(32)
            .tempfile_in(&records)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rewind-snapshot-id-v1\0");
        hasher.update(
            nonce
                .path()
                .file_name()
                .ok_or_else(|| Error::Invalid("missing nonce filename".into()))?
                .as_encoded_bytes(),
        );
        hasher.update(root_hash.as_bytes());
        hasher.update(&timestamp.as_secs().to_le_bytes());
        hasher.update(&timestamp.subsec_nanos().to_le_bytes());
        let record = SnapshotRecord {
            version: 1,
            id: hasher.finalize().to_hex().to_string(),
            root_hash,
            source_path,
            created_unix_seconds: timestamp.as_secs(),
            created_nanoseconds: timestamp.subsec_nanos(),
        };
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(Error::Invalid("snapshot record exceeds 64 KiB".into()));
        }
        if !store.publish_snapshot(&records.join(format!("{}.json", record.id)), &bytes)? {
            return Err(Error::Invalid(
                "snapshot ID collision; retry capture".into(),
            ));
        }
        // Report cleanup failure accurately; the record may already be visible.
        nonce.close()?;
        store.finish_capture()?;
        Ok(record)
    }

    fn entry_name<'a>(name: &'a CStr, display: &Path) -> Result<&'a str> {
        name.to_str()
            .map_err(|_| Error::Invalid(format!("non-UTF-8 entry in {}", display.display())))
    }

    fn capture_directory(
        store: &ChunkStore,
        directory: &File,
        display: &Path,
        store_identity: (u64, u64),
        depth: usize,
    ) -> Result<String> {
        if depth > MAX_DEPTH {
            return Err(Error::Invalid(format!(
                "directory depth exceeds {MAX_DEPTH}: {}",
                display.display()
            )));
        }
        let mut names = Vec::new();
        for entry in Dir::read_from(directory).map_err(|e| io_at(display, e))? {
            let entry = entry.map_err(|e| io_at(display, e))?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let name = entry_name(name, display)?;
            names.push(name.to_owned());
        }
        names.sort();
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let path = display.join(&name);
            let stat = rfs::statat(directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|e| io_at(&path, e))?;
            let kind = FileType::from_raw_mode(stat.st_mode);
            let target = match kind {
                FileType::Directory => {
                    let child: File = rfs::openat(
                        directory,
                        name.as_str(),
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|e| io_at(&path, e))?
                    .into();
                    let meta = child.metadata()?;
                    if (meta.dev(), meta.ino()) == store_identity {
                        continue;
                    }
                    validate_name(&name)?;
                    EntryTarget::Directory {
                        tree: capture_directory(store, &child, &path, store_identity, depth + 1)?,
                    }
                }
                FileType::RegularFile => {
                    validate_name(&name)?;
                    let file: File = rfs::openat(
                        directory,
                        name.as_str(),
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|e| io_at(&path, e))?
                    .into();
                    let manifest = store_open_file(store, file, ChunkSizes::default())?;
                    let manifest_path = manifest.save(store)?;
                    let hash = manifest_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .ok_or_else(|| Error::Invalid("invalid stored manifest filename".into()))?
                        .to_owned();
                    hashing::validate_hash(&hash)?;
                    EntryTarget::File {
                        manifest: hash,
                        length: manifest.length,
                    }
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "symlinks and special files are unsupported: {}",
                        path.display()
                    )));
                }
            };
            entries.push(DirectoryEntry::new(name, target)?);
        }
        DirectoryNode::new(entries)?.save(store)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn non_utf8_entry_names_are_rejected_without_a_filesystem_fixture() {
            // macOS filesystems can reject these bytes before capture sees them.
            let invalid = c"\xff";
            assert!(entry_name(invalid, Path::new("source")).is_err());
            assert_eq!(
                entry_name(c"ordinary", Path::new("source")).unwrap(),
                "ordinary"
            );
        }
    }
}
