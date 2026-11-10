//! Writer coordination and recoverable capture-only directory-sync batching.
//! The OS lock is permanent; the pending marker is a durability journal, not a lock.
use crate::{ChunkStore, Error, Result, storage};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const MARKER: &str = "capture.pending";
const MARKER_BYTES: &[u8] = b"rewind-pending-v1\n";

#[derive(Debug)]
pub(crate) struct Batch {
    _lock: File,
    dirty: Mutex<BTreeSet<PathBuf>>,
    ready: AtomicBool,
    published: AtomicBool,
}

/// A standalone mutation owns its lock until explicit successful completion.
/// Dropping after an error releases the lock but deliberately leaves the marker.
pub(crate) struct Writer<'a> {
    store: &'a ChunkStore,
    lock: Option<File>,
}

impl Writer<'_> {
    pub(crate) fn finish(self) -> Result<()> {
        if self.lock.is_some() {
            clear_marker(self.store.root())?;
        }
        Ok(())
    }
}

fn lock(root: &Path) -> Result<File> {
    #[cfg(unix)]
    let file: File = rustix::fs::openat(
        rustix::fs::CWD,
        root.join("writer.lock"),
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(std::io::Error::from)?
    .into();
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("writer.lock"))?;
    if !file.metadata()?.is_file() {
        return Err(Error::Invalid("writer lock must be a regular file".into()));
    }
    file.lock()?;
    Ok(file)
}

/// Automatic capture opens the existing lock without creating any namespace entry.
/// Initialization already created and synced this permanent lock inode.
pub(crate) fn lock_recovered(root: &Path, cancel: &AtomicBool) -> Result<File> {
    use std::fs::TryLockError;
    #[cfg(unix)]
    let file: File = rustix::fs::openat(
        rustix::fs::CWD,
        root.join("writer.lock"),
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?
    .into();
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("writer.lock"))?;
    if !file.metadata()?.is_file() {
        return Err(Error::Invalid("invalid writer lock".into()));
    }
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(25))
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    recover(root)?; // Failure leaves the marker; no caller may proceed to reuse.
    Ok(file)
}

fn clear_marker(root: &Path) -> Result<()> {
    checkpoint("marker_cleanup")?;
    fs::remove_file(root.join(MARKER))?;
    storage::sync_dir(root)
}

fn prepare(root: &Path) -> Result<()> {
    recover(root)?;
    if !storage::publish(&root.join(MARKER), MARKER_BYTES)? {
        return Err(Error::Invalid("pending marker unexpectedly exists".into()));
    }
    checkpoint("marker_durable")
}

/// Under the exclusive lock, repair surviving namespace links, deepest first.
/// Missing directories after a crash contain no reachable reusable objects; their
/// surviving ancestors are still synced. No objects are deleted or trusted by hash
/// here: ordinary reuse continues to verify their contents separately.
fn recover(root: &Path) -> Result<()> {
    match storage::read_regular_file(&root.join(MARKER), 64) {
        Ok(bytes) if bytes == MARKER_BYTES => (),
        Ok(_) => return Err(Error::Invalid("unsupported pending marker".into())),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    let mut dirs = BTreeSet::from([root.to_owned()]);
    // Also repair an interrupted initialization that linked the store itself.
    dirs.extend(fs::canonicalize(root)?.ancestors().map(Path::to_owned));
    for name in ["chunks", "manifests", "trees", "snapshots"] {
        collect_dirs(&root.join(name), 0, &mut dirs)?;
    }
    barrier(dirs, "recovery_sync")?;
    clear_marker(root)
}

fn collect_dirs(path: &Path, depth: usize, dirs: &mut BTreeSet<PathBuf>) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => (),
        Ok(_) => {
            return Err(Error::Invalid(format!(
                "invalid recovery directory: {}",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    if depth > 2 {
        return Err(Error::Invalid(
            "unexpected nested storage directories".into(),
        ));
    }
    dirs.insert(path.to_owned());
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_dirs(&entry.path(), depth + 1, dirs)?;
        } else if !kind.is_file() {
            return Err(Error::Invalid("non-regular object during recovery".into()));
        }
    }
    Ok(())
}

fn barrier(dirs: BTreeSet<PathBuf>, stage: &str) -> Result<()> {
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then(a.cmp(b))
    });
    for dir in dirs {
        checkpoint(stage)?;
        #[cfg(test)]
        faults::DIRS.with(|trace| trace.borrow_mut().push((stage.to_owned(), dir.clone())));
        storage::sync_dir(&dir)?;
    }
    Ok(())
}

impl ChunkStore {
    pub(crate) fn writer(&self) -> Result<Writer<'_>> {
        let lock = if self.batch.is_none() {
            let guard = lock(self.root())?;
            prepare(self.root())?;
            Some(guard)
        } else {
            None // This private store already owns the entire capture's lock.
        };
        Ok(Writer { store: self, lock })
    }

    pub(crate) fn begin_capture(mut self) -> Result<Self> {
        let guard = lock(self.root())?;
        prepare(self.root())?;
        self.batch = Some(Batch {
            _lock: guard,
            dirty: Mutex::new(BTreeSet::new()),
            ready: AtomicBool::new(false),
            published: AtomicBool::new(false),
        });
        Ok(self)
    }

    pub(crate) fn begin_capture_locked(mut self, guard: File) -> Result<Self> {
        // The caller owns the recovered exclusive lock; start mutation only now.
        if !storage::publish(&self.root().join(MARKER), MARKER_BYTES)? {
            return Err(Error::Invalid("pending marker unexpectedly exists".into()));
        }
        checkpoint("marker_durable")?;
        self.batch = Some(Batch {
            _lock: guard,
            dirty: Mutex::new(BTreeSet::new()),
            ready: AtomicBool::new(false),
            published: AtomicBool::new(false),
        });
        Ok(self)
    }

    pub(crate) fn publish_object(&self, path: &Path, bytes: &[u8]) -> Result<bool> {
        let Some(batch) = &self.batch else {
            return storage::publish(path, bytes);
        };
        if batch.ready.load(Ordering::Relaxed) {
            return Err(Error::Invalid("capture object phase already sealed".into()));
        }
        let remember = |dir: &Path| {
            // Include every surviving ancestor even if a prior interrupted mkdir
            // made it visible without durably linking it into its parent.
            let mut dirty = batch
                .dirty
                .lock()
                .map_err(|_| Error::Invalid("capture state poisoned".into()))?;
            let mut current = dir;
            loop {
                dirty.insert(current.to_owned());
                if current == self.root() {
                    break;
                }
                current = current
                    .parent()
                    .ok_or_else(|| Error::Invalid("object outside store".into()))?;
            }
            Ok(())
        };
        let published = storage::publish_using(
            path,
            bytes,
            |dir| {
                storage::ensure_dir_with(dir, &mut |created| {
                    checkpoint("directory_created")?;
                    remember(created)
                })
            },
            remember,
            || checkpoint("file_synced"),
        )?;
        checkpoint("object_published")?;
        Ok(published)
    }

    pub(crate) fn object_barrier(&self) -> Result<()> {
        let batch = self
            .batch
            .as_ref()
            .ok_or_else(|| Error::Invalid("no capture session".into()))?;
        let dirty = batch
            .dirty
            .lock()
            .map_err(|_| Error::Invalid("capture state poisoned".into()))?
            .clone();
        barrier(dirty, "object_directory_sync")?;
        checkpoint("object_barrier_complete")?;
        batch.ready.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn publish_snapshot(&self, path: &Path, bytes: &[u8]) -> Result<bool> {
        let batch = self
            .batch
            .as_ref()
            .ok_or_else(|| Error::Invalid("no capture session".into()))?;
        if !batch.ready.load(Ordering::Relaxed) {
            return Err(Error::Invalid("object barrier has not completed".into()));
        }
        checkpoint("snapshot_before_publish")?;
        let published = storage::publish_using(
            path,
            bytes,
            storage::ensure_dir,
            |dir| {
                checkpoint("snapshot_directory_sync")?;
                storage::sync_dir(dir)
            },
            || checkpoint("snapshot_file_synced"),
        )?;
        batch.published.store(published, Ordering::Relaxed);
        Ok(published)
    }

    pub(crate) fn finish_capture(self) -> Result<()> {
        if !self
            .batch
            .as_ref()
            .is_some_and(|b| b.published.load(Ordering::Relaxed))
        {
            return Err(Error::Invalid(
                "snapshot has not been durably published".into(),
            ));
        }
        checkpoint("snapshot_published")?;
        clear_marker(self.root())
    }
}

// Test-only faults cannot be enabled in the production executable.
#[cfg(not(test))]
pub(crate) fn checkpoint(_stage: &str) -> Result<()> {
    Ok(())
}
#[cfg(test)]
pub(crate) fn checkpoint(stage: &str) -> Result<()> {
    faults::checkpoint(stage)
}

#[cfg(test)]
pub(crate) fn set_test_fault(stage: Option<&'static str>) {
    faults::FAIL.with(|f| *f.borrow_mut() = stage.map(|s| (s, 0, false)));
}

#[cfg(test)]
mod faults {
    use super::*;
    use std::cell::RefCell;
    thread_local! { pub static FAIL: RefCell<Option<(&'static str, usize, bool)>> = const { RefCell::new(None) }; }
    thread_local! {
        pub static EVENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        pub static DIRS: RefCell<Vec<(String, PathBuf)>> = const { RefCell::new(Vec::new()) };
    }
    pub fn checkpoint(stage: &str) -> Result<()> {
        EVENTS.with(|trace| trace.borrow_mut().push(stage.to_owned()));
        FAIL.with(|fault| {
            let mut fault = fault.borrow_mut();
            if let Some((target, remaining, exit)) = fault.as_mut()
                && *target == stage
            {
                if *remaining > 0 {
                    *remaining -= 1;
                    return Ok(());
                }
                if *exit {
                    if std::env::var("REWIND_TEST_MODE").as_deref() == Ok("kill") {
                        let root = PathBuf::from(std::env::var("REWIND_TEST_ROOT").unwrap());
                        fs::write(root.join("kill-ready"), stage).unwrap();
                        loop {
                            std::thread::park();
                        }
                    }
                    std::process::exit(73);
                }
                return Err(std::io::Error::other(format!("injected {stage}")).into());
            }
            Ok(())
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::snapshot::{create_snapshot, list_snapshots};
    use std::{
        process::{Child, Command},
        thread,
        time::{Duration, Instant},
    };

    fn fixture() -> (tempfile::TempDir, ChunkStore, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let store = ChunkStore::init(fs::canonicalize(temp.path()).unwrap().join("store")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"original").unwrap();
        (temp, store, source)
    }
    fn fault(stage: &'static str, skip: usize, exit: bool) {
        faults::FAIL.with(|f| *f.borrow_mut() = Some((stage, skip, exit)));
    }
    fn reset() {
        faults::FAIL.with(|f| *f.borrow_mut() = None);
    }

    #[test]
    fn capture_faults_never_publish_before_the_barrier_and_recover() {
        for (stage, skip, visible) in [
            ("marker_durable", 0, false),
            ("directory_created", 0, false),
            ("file_synced", 0, false),
            ("object_published", 0, false),
            ("object_published", 2, false),
            ("object_directory_sync", 0, false),
            ("object_directory_sync", 2, false),
            ("object_barrier_complete", 0, false),
            ("snapshot_before_publish", 0, false),
            ("snapshot_file_synced", 0, false),
            ("snapshot_directory_sync", 0, true),
            ("snapshot_published", 0, true),
            ("marker_cleanup", 0, true),
        ] {
            let (temp, store, source) = fixture();
            let old = create_snapshot(&store, &source).unwrap();
            fs::write(source.join("file"), b"new contents").unwrap();
            // Force creation of a new directory namespace even after the old capture.
            fs::create_dir(source.join("nested")).unwrap();
            fs::write(source.join("nested/child"), b"child").unwrap();
            fault(stage, skip, false);
            assert!(create_snapshot(&store, &source).is_err(), "{stage}");
            reset();
            assert!(store.root().join(MARKER).is_file(), "{stage}");
            let history = list_snapshots(&store).unwrap();
            assert_eq!(history.len(), if visible { 2 } else { 1 }, "{stage}");
            // Previous records and any record visible after rename are restorable.
            for (i, record) in history.iter().enumerate() {
                crate::restore_snapshot(
                    &store,
                    record.id(),
                    temp.path().join(format!("restored-{i}")),
                )
                .unwrap();
            }
            let next = create_snapshot(&store, &source).unwrap();
            assert_ne!(old.root_hash(), next.root_hash());
            assert!(!store.root().join(MARKER).exists());
        }
    }

    #[test]
    fn recovery_barrier_failures_and_invalid_markers_fail_closed() {
        for (stage, skip) in [
            ("recovery_sync", 0),
            ("recovery_sync", 1),
            ("recovery_sync", 2),
            ("recovery_sync", 3),
            ("marker_cleanup", 0),
        ] {
            let (_temp, store, source) = fixture();
            fault("object_barrier_complete", 0, false);
            assert!(create_snapshot(&store, &source).is_err());
            reset();
            fault(stage, skip, false);
            // Existing handles and standalone reuse must also pass recovery.
            assert!(store.put(b"original").is_err());
            reset();
            assert_eq!(fs::read(store.root().join(MARKER)).unwrap(), MARKER_BYTES);
            assert!(list_snapshots(&store).unwrap().is_empty());
            store.put(b"original").unwrap();
            assert!(!store.root().join(MARKER).exists());
        }
        let (_temp, store, _source) = fixture();
        fs::write(store.root().join(MARKER), b"unknown version").unwrap();
        assert!(store.put(b"x").is_err());
        assert_eq!(
            fs::read(store.root().join(MARKER)).unwrap(),
            b"unknown version"
        );
        fs::remove_file(store.root().join(MARKER)).unwrap();
        std::os::unix::fs::symlink("missing", store.root().join(MARKER)).unwrap();
        assert!(store.put(b"x").is_err());
        assert!(
            fs::symlink_metadata(store.root().join(MARKER))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn recovery_handles_surviving_ancestors_and_lost_shards_without_trusting_data() {
        let (_temp, store, source) = fixture();
        fault("object_barrier_complete", 0, false);
        assert!(create_snapshot(&store, &source).is_err());
        reset();
        // Model namespace entries lost across reboot, not a real power-loss test.
        fs::remove_dir_all(store.root().join("trees")).unwrap();
        let chunk = store
            .chunk_path(&crate::hashing::hash(b"original"))
            .unwrap();
        fs::remove_dir_all(chunk.parent().unwrap()).unwrap();
        faults::DIRS.with(|t| t.borrow_mut().clear());
        create_snapshot(&store, &source).unwrap();
        faults::DIRS.with(|t| {
            let dirs = t.borrow();
            assert!(
                dirs.iter()
                    .any(|(stage, path)| stage == "recovery_sync" && path == store.root())
            );
            assert!(
                dirs.iter()
                    .any(|(stage, path)| stage == "object_directory_sync"
                        && path == chunk.parent().unwrap())
            );
        });
        fault("object_barrier_complete", 0, false);
        assert!(create_snapshot(&store, &source).is_err());
        reset();
        fs::write(&chunk, b"corrupt!").unwrap();
        assert!(store.put(b"original").is_err());
        assert_eq!(fs::read(chunk).unwrap(), b"corrupt!");
    }

    #[test]
    fn barrier_orders_children_before_parents_and_snapshot_after_objects() {
        let (_temp, store, source) = fixture();
        faults::DIRS.with(|t| t.borrow_mut().clear());
        faults::EVENTS.with(|t| t.borrow_mut().clear());
        create_snapshot(&store, source).unwrap();
        faults::DIRS.with(|t| {
            let trace = t.borrow();
            let dirs: Vec<_> = trace
                .iter()
                .filter(|(stage, _)| stage == "object_directory_sync")
                .map(|(_, p)| p)
                .collect();
            assert!(!dirs.is_empty());
            assert_eq!(dirs.last().unwrap().as_path(), store.root());
            assert_eq!(dirs.len(), dirs.iter().collect::<BTreeSet<_>>().len());
            for pair in dirs.windows(2) {
                assert!(pair[0].components().count() >= pair[1].components().count());
            }
        });
        faults::EVENTS.with(|t| {
            let trace = t.borrow();
            let pos = |s| trace.iter().position(|x| x == s).unwrap();
            assert!(pos("marker_durable") < pos("file_synced"));
            assert!(pos("file_synced") < pos("object_published"));
            assert!(pos("object_barrier_complete") < pos("snapshot_before_publish"));
            assert!(pos("snapshot_file_synced") < pos("snapshot_directory_sync"));
            assert!(pos("snapshot_directory_sync") < pos("marker_cleanup"));
        });
    }

    fn spawn(temp: &Path, mode: &str, stage: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transaction::tests::subprocess_helper",
                "--nocapture",
            ])
            .env("REWIND_TEST_ROOT", temp)
            .env("REWIND_TEST_MODE", mode)
            .env("REWIND_TEST_STAGE", stage)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }
    fn wait_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < deadline, "timeout: {}", path.display());
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn wait_child(child: &mut Child) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("child timeout");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn subprocess_interruption_recovers_and_releases_the_writer_lock() {
        for stage in [
            "marker_durable",
            "directory_created",
            "file_synced",
            "object_published",
            "object_directory_sync",
            "object_barrier_complete",
            "snapshot_file_synced",
            "snapshot_directory_sync",
            "snapshot_published",
        ] {
            let (temp, store, source) = fixture();
            let mut child = spawn(temp.path(), "crash", stage);
            assert_eq!(wait_child(&mut child).code(), Some(73), "{stage}");
            assert!(store.root().join(MARKER).exists());
            // Read-only commands do not remove a live or abandoned marker.
            list_snapshots(&store).unwrap();
            assert!(store.root().join(MARKER).exists());
            let record = create_snapshot(&store, &source).unwrap();
            crate::restore_snapshot(&store, record.id(), temp.path().join("restore")).unwrap();
            assert_eq!(
                fs::read(temp.path().join("restore/file")).unwrap(),
                b"original"
            );
        }
    }

    #[test]
    fn exclusive_lock_covers_capture_recovery_and_all_standalone_mutations() {
        for mode in ["capture", "chunk", "manifest", "tree", "init"] {
            let (temp, store, _source) = fixture();
            let held = ChunkStore::open(store.root())
                .unwrap()
                .begin_capture()
                .unwrap();
            let mut child = spawn(temp.path(), mode, "");
            wait_file(&temp.path().join("started"));
            thread::sleep(Duration::from_millis(60));
            assert!(
                child.try_wait().unwrap().is_none(),
                "{mode} bypassed writer lock"
            );
            assert!(store.root().join(MARKER).exists());
            drop(held); // No commit: next writer must recover, then write.
            assert!(wait_child(&mut child).success(), "{mode}");
            assert!(!store.root().join(MARKER).exists());
        }
    }

    #[test]
    fn sigkill_during_capture_leaves_recoverable_objects_and_releases_lock() {
        let (temp, store, source) = fixture();
        let mut child = spawn(temp.path(), "kill", "object_published");
        wait_file(&temp.path().join("kill-ready"));
        child.kill().unwrap();
        assert!(!wait_child(&mut child).success());
        assert!(list_snapshots(&store).unwrap().is_empty());
        assert!(store.root().join(MARKER).exists());
        let record = create_snapshot(&store, source).unwrap();
        crate::restore_snapshot(&store, record.id(), temp.path().join("restore")).unwrap();
        assert_eq!(
            fs::read(temp.path().join("restore/file")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn file_sync_and_marker_sync_failures_prevent_history_publication() {
        for (stage, skip, marker) in [
            ("file_sync", 0, false),
            ("file_sync", 1, true),
            ("directory_sync", 0, true),
        ] {
            let (_temp, store, source) = fixture();
            fault(stage, skip, false);
            assert!(create_snapshot(&store, &source).is_err());
            reset();
            assert!(list_snapshots(&store).unwrap().is_empty());
            assert_eq!(store.root().join(MARKER).exists(), marker);
            create_snapshot(&store, &source).unwrap();
        }
    }

    #[test]
    fn private_commit_api_requires_barrier_and_disallows_new_objects_after_it() {
        let (_temp, store, _source) = fixture();
        let capture = ChunkStore::open(store.root())
            .unwrap()
            .begin_capture()
            .unwrap();
        let path = store.root().join("snapshots/invalid.json");
        assert!(capture.publish_snapshot(&path, b"invalid").is_err());
        assert!(!path.exists());
        capture.put(b"x").unwrap();
        capture.object_barrier().unwrap();
        assert!(capture.put(b"new object after sealing").is_err());
        assert!(capture.finish_capture().is_err());
        assert!(store.root().join(MARKER).exists());
        store.put(b"x").unwrap();
        assert!(!store.root().join(MARKER).exists());
    }

    #[test]
    fn subprocess_helper() {
        let Ok(root) = std::env::var("REWIND_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let store = ChunkStore::open(root.join("store")).unwrap();
        fs::write(root.join("started"), b"ready").unwrap();
        match std::env::var("REWIND_TEST_MODE").unwrap().as_str() {
            "crash" | "kill" => {
                let stage = std::env::var("REWIND_TEST_STAGE").unwrap();
                let stage: &'static str = Box::leak(stage.into_boxed_str());
                fault(stage, 0, true);
                create_snapshot(&store, root.join("source")).unwrap();
                panic!("crash point was not reached");
            }
            "capture" => {
                create_snapshot(&store, root.join("source")).unwrap();
            }
            "chunk" => {
                store.put(b"original").unwrap();
            }
            "manifest" => {
                crate::FileManifest {
                    version: 1,
                    algorithm: "fastcdc-2020-blake3".into(),
                    chunking: crate::chunking::ChunkSizes::default(),
                    length: 0,
                    file_hash: crate::hashing::hash(b""),
                    chunks: vec![],
                }
                .save(&store)
                .unwrap();
            }
            "tree" => {
                crate::tree::DirectoryNode::new(vec![])
                    .unwrap()
                    .save(&store)
                    .unwrap();
            }
            "init" => {
                ChunkStore::init(store.root()).unwrap();
            }
            mode => panic!("unknown child mode {mode}"),
        }
    }
}
