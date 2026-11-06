//! Verified directory reconstruction, separate from capture and history metadata.
use crate::{ChunkStore, Error, Result};
use std::path::Path;

/// Restore a snapshot into a new directory, refusing all existing destinations.
/// Requires an existing destination parent outside the store and the recorded
/// source tree. Builds and syncs a private sibling staging tree before publishing
/// it atomically without replacement. Verifies nodes, manifests and file bytes.
/// Supported on macOS and Linux; returns unsupported elsewhere. A post-publication
/// sync error may leave the completed output visible despite an error result.
pub fn restore_snapshot(store: &ChunkStore, id: &str, output: impl AsRef<Path>) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        supported::restore(store, id, output.as_ref())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (store, id, output);
        Err(Error::Invalid(
            "snapshot restoration currently requires macOS or Linux".into(),
        ))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use super::*;
    use crate::{
        FileManifest, reconstruct,
        snapshot::SnapshotRecord,
        tree::{DirectoryNode, EntryTarget},
    };
    use rustix::fs::{self as rfs, AtFlags, Mode, OFlags, RenameFlags};
    use std::{
        ffi::OsStr,
        fs::{self, File},
        path::PathBuf,
    };

    const MAX_DEPTH: usize = 256;

    fn io(error: rustix::io::Errno) -> Error {
        Error::Io(error.into())
    }

    pub(super) fn restore(store: &ChunkStore, id: &str, output: &Path) -> Result<()> {
        let record = SnapshotRecord::load(store, id)?;
        let output: PathBuf = output.components().collect();
        let name = output
            .file_name()
            .ok_or_else(|| Error::Invalid("output must name a new directory".into()))?;
        let parent_path = fs::canonicalize(
            output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
        let destination = parent_path.join(name);
        let store = ChunkStore::open(fs::canonicalize(store.root())?)?;
        if destination.starts_with(store.root())
            || destination.starts_with(Path::new(record.source_path()))
        {
            return Err(Error::Invalid(
                "snapshot output must be outside the store and recorded source tree".into(),
            ));
        }
        let parent: File = rfs::openat(
            rfs::CWD,
            &parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?
        .into();
        match rfs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("output already exists: {}", destination.display()),
                )
                .into());
            }
            Err(error) if error == rustix::io::Errno::NOENT => (),
            Err(error) => return Err(io(error)),
        }
        let stage = tempfile::Builder::new()
            .prefix(".rewind-restore-")
            .tempdir_in(&parent_path)?;
        let operation = (|| -> Result<()> {
            let stage_name = stage
                .path()
                .file_name()
                .ok_or_else(|| Error::Invalid("missing staging directory name".into()))?;
            let directory: File = rfs::openat(
                &parent,
                stage_name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io)?
            .into();
            populate(&store, record.root_hash(), &directory, 0)?;
            publish_directory(&parent, stage_name, name)
        })();
        match operation {
            Ok(()) => {
                // The stage was renamed; disarm path-based temporary cleanup.
                let _ = stage.keep();
                parent.sync_all()?;
                Ok(())
            }
            Err(error) => {
                // Explicit cleanup reports secondary failures instead of hiding
                // them in Drop. TempDir never follows symlinks during removal.
                match stage.close() {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(Error::Invalid(format!(
                        "restore failed: {error}; staging cleanup also failed: {cleanup}"
                    ))),
                }
            }
        }
    }

    // No check-then-ordinary-rename fallback: a racing empty destination must
    // remain intact too. Unsupported filesystem/kernel operations return errors.
    fn publish_directory(parent: &File, stage: &OsStr, output: &OsStr) -> Result<()> {
        rfs::renameat_with(parent, stage, parent, output, RenameFlags::NOREPLACE).map_err(io)
    }

    fn populate(store: &ChunkStore, hash: &str, output: &File, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::Invalid(
                "snapshot directory depth exceeds 256".into(),
            ));
        }
        let node = DirectoryNode::load(store, hash)?;
        for entry in node.entries() {
            // Node loading validated each name as a single safe component. All
            // output access is still relative to an already opened directory.
            match entry.target() {
                EntryTarget::Directory { tree } => {
                    rfs::mkdirat(output, entry.name(), Mode::RWXU).map_err(io)?;
                    let directory: File = rfs::openat(
                        output,
                        entry.name(),
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(io)?
                    .into();
                    populate(store, tree, &directory, depth + 1)?;
                }
                EntryTarget::File { manifest, length } => {
                    let manifest = FileManifest::load_by_hash(store, manifest)?;
                    if manifest.length != *length {
                        return Err(Error::Corrupt(format!(
                            "file length mismatch for {:?}",
                            entry.name()
                        )));
                    }
                    let mut file: File = rfs::openat(
                        output,
                        entry.name(),
                        OFlags::WRONLY
                            | OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC,
                        Mode::RUSR | Mode::WUSR,
                    )
                    .map_err(io)?
                    .into();
                    reconstruct(store, &manifest, &mut file)?;
                    file.sync_all()?;
                }
            }
        }
        output.sync_all()?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn publication_refuses_a_destination_created_after_preflight() {
            let temp = tempfile::tempdir().unwrap();
            let parent = File::open(temp.path()).unwrap();
            fs::create_dir(temp.path().join("stage")).unwrap();
            fs::write(temp.path().join("stage/file"), b"verified data").unwrap();
            // Simulate another process creating output after the caller checked.
            fs::create_dir(temp.path().join("output")).unwrap();
            assert!(publish_directory(&parent, OsStr::new("stage"), OsStr::new("output")).is_err());
            assert!(temp.path().join("stage/file").is_file());
            assert_eq!(fs::read_dir(temp.path().join("output")).unwrap().count(), 0);
        }
    }
}
