//! Full-scan modes and two-pass validation, sharing the manual traversal.
#[cfg(test)]
use super::unix::create;
use super::{
    unix::{capture_directory, io_at, publish_record},
    *,
};
use rustix::fs::{self as rfs, Mode, OFlags};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanMode {
    Manual,
    Probe,
    Verify,
    Persist,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mode: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Stamp {
    fn new(m: &fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mode: m.mode(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}
#[derive(Default)]
struct Observations {
    entries: BTreeMap<PathBuf, Stamp>,
    bytes: usize,
}
pub(super) struct Scan<'a> {
    pub(super) mode: ScanMode,
    stop: Option<&'a AtomicBool>,
    expected: Option<&'a Observations>,
    observed: Observations,
}
impl Scan<'_> {
    pub(super) fn manual() -> Self {
        Self {
            mode: ScanMode::Manual,
            stop: None,
            expected: None,
            observed: Observations::default(),
        }
    }
    pub(super) fn cancel(&self) -> Result<()> {
        if self.stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    pub(super) fn io(&self, path: &Path, e: impl Into<std::io::Error>) -> Error {
        if self.mode == ScanMode::Manual {
            io_at(path, e)
        } else {
            let e = e.into();
            Error::SourceIo(std::io::Error::new(
                e.kind(),
                format!("{}: {e}", path.display()),
            ))
        }
    }
    pub(super) fn metadata(&self, file: &File) -> Result<fs::Metadata> {
        file.metadata().map_err(|e| {
            if self.mode == ScanMode::Manual {
                Error::Io(e)
            } else {
                Error::SourceIo(e)
            }
        })
    }
    pub(super) fn observe(&mut self, path: &Path, m: &fs::Metadata) -> Result<()> {
        if self.mode == ScanMode::Manual {
            return Ok(());
        }
        let stamp = Stamp::new(m);
        if let Some(expected) = self.expected {
            if expected.entries.get(path) != Some(&stamp) {
                return Err(Error::Unstable(path.display().to_string()));
            }
        } else {
            if self.observed.entries.len() >= 100_000
                || self.observed.bytes + path.as_os_str().len() > 16 * 1024 * 1024
            {
                return Err(Error::Invalid(
                    "automatic capture observation limit: 100000 entries / 16 MiB paths".into(),
                ));
            }
            self.observed.bytes += path.as_os_str().len();
            self.observed.entries.insert(path.to_owned(), stamp);
        }
        Ok(())
    }
    pub(super) fn unchanged(
        &self,
        path: &Path,
        before: &fs::Metadata,
        after: &fs::Metadata,
    ) -> Result<()> {
        if self.mode != ScanMode::Manual && Stamp::new(before) != Stamp::new(after) {
            return Err(Error::Unstable(path.display().to_string()));
        }
        Ok(())
    }
}

fn validate_observations(
    root: &File,
    source: &Path,
    observed: &Observations,
    stop: &AtomicBool,
) -> Result<()> {
    for (path, expected) in &observed.entries {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let relative = path
            .strip_prefix(source)
            .map_err(|_| Error::Invalid("observation outside source".into()))?;
        // Reopen every ancestor relative to the anchored descriptor; no following
        // source symlinks, even when an ancestor was replaced during capture.
        let mut parent = root.try_clone().map_err(Error::SourceIo)?;
        let parts: Vec<_> = relative.components().collect();
        for (i, part) in parts.iter().enumerate() {
            let flags = OFlags::RDONLY
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC
                | if i + 1 < parts.len() || expected.mode & 0o170000 == 0o040000 {
                    OFlags::DIRECTORY
                } else {
                    OFlags::empty()
                };
            parent = rfs::openat(&parent, part.as_os_str(), flags, Mode::empty())
                .map_err(|e| Error::SourceIo(e.into()))?
                .into();
        }
        if Stamp::new(&parent.metadata().map_err(Error::SourceIo)?) != *expected {
            return Err(Error::Unstable(path.display().to_string()));
        }
    }
    let current: File = rfs::openat(
        rfs::CWD,
        source,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::SourceIo(e.into()))?
    .into();
    if Stamp::new(&current.metadata().map_err(Error::SourceIo)?) != observed.entries[source] {
        return Err(Error::Unstable("source root replaced".into()));
    }
    Ok(())
}

pub(crate) fn automatic(
    store: &ChunkStore,
    source: &Path,
    stop: &AtomicBool,
    expected_identity: Option<(u64, u64)>,
) -> Result<super::AutomaticCapture> {
    let source: PathBuf = source.components().collect();
    let root: File = rfs::openat(
        rfs::CWD,
        &source,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::SourceIo(e.into()))?
    .into();
    let source = fs::canonicalize(source).map_err(Error::SourceIo)?;
    let m = root.metadata().map_err(Error::SourceIo)?;
    let current = fs::symlink_metadata(&source).map_err(Error::SourceIo)?;
    if (m.dev(), m.ino()) != (current.dev(), current.ino())
        || expected_identity.is_some_and(|id| id != (m.dev(), m.ino()))
    {
        return Err(Error::Invalid(
            "watched source root replaced; restart required".into(),
        ));
    }
    let source_path = source
        .to_str()
        .ok_or_else(|| Error::Invalid("source path must be UTF-8".into()))?
        .to_owned();
    let store = ChunkStore::open(fs::canonicalize(store.root())?)?;
    if source.starts_with(store.root()) {
        return Err(Error::Invalid("cannot capture store".into()));
    }
    let guard = crate::transaction::lock_recovered(store.root(), stop)?;
    let m = fs::metadata(store.root())?;
    let identity = (m.dev(), m.ino());
    let latest = super::list_snapshots(&store)?
        .into_iter()
        .rev()
        .find(|r| r.source_path() == source_path);
    let mut probe = Scan {
        mode: ScanMode::Probe,
        stop: Some(stop),
        expected: None,
        observed: Observations::default(),
    };
    let first = capture_directory(&store, &root, &source, identity, 0, &mut probe)?;
    #[cfg(test)]
    auto_tests::after_probe(&source);
    let unchanged = latest.as_ref().is_some_and(|r| r.root_hash() == first);
    if unchanged {
        let mut verification = Scan {
            mode: ScanMode::Verify,
            stop: Some(stop),
            expected: Some(&probe.observed),
            observed: Observations::default(),
        };
        let second = capture_directory(&store, &root, &source, identity, 0, &mut verification)?;
        if first != second {
            return Err(Error::Unstable("read-only roots differ".into()));
        }
        validate_observations(&root, &source, &probe.observed, stop)?;
        drop(guard);
        return Ok(super::AutomaticCapture::Unchanged(latest.unwrap()));
    }
    let store = store.begin_capture_locked(guard)?;
    let mut persistence = Scan {
        mode: ScanMode::Persist,
        stop: Some(stop),
        expected: Some(&probe.observed),
        observed: Observations::default(),
    };
    let second = capture_directory(&store, &root, &source, identity, 0, &mut persistence)?;
    if first != second {
        return Err(Error::Unstable("capture roots differ".into()));
    }
    store.object_barrier()?;
    #[cfg(test)]
    auto_tests::before_publish();
    validate_observations(&root, &source, &probe.observed, stop)?;
    if stop.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    #[cfg(test)]
    auto_tests::after_validation(&source);
    let record = publish_record(&store, second, source_path)?;
    store.finish_capture()?;
    Ok(super::AutomaticCapture::Created(record))
}

#[cfg(test)]
pub(crate) mod auto_tests {
    use super::*;
    use std::{cell::RefCell, sync::Arc};
    type Hook = Box<dyn FnOnce(&Path)>;
    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static BEFORE_PUBLISH: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        static COMMIT_HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }
    pub(crate) fn set_commit_hook(hook: Hook) {
        COMMIT_HOOK.with(|h| *h.borrow_mut() = Some(hook));
    }
    pub(super) fn after_validation(path: &Path) {
        COMMIT_HOOK.with(|h| {
            if let Some(hook) = h.borrow_mut().take() {
                hook(path);
            }
        });
    }
    pub(super) fn before_publish() {
        BEFORE_PUBLISH.with(|h| {
            if let Some(hook) = h.borrow_mut().take() {
                hook();
            }
        });
    }
    pub(crate) fn set_probe_hook(hook: Box<dyn FnOnce(&Path)>) {
        HOOK.with(|h| *h.borrow_mut() = Some(hook));
    }
    pub(super) fn after_probe(path: &Path) {
        HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook(path);
            }
        });
    }
    fn fixture() -> (tempfile::TempDir, ChunkStore, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested/empty")).unwrap();
        fs::write(source.join("nested/file"), [0, 255, 42]).unwrap();
        (temp, store, source)
    }
    fn state(root: &Path) -> Vec<(PathBuf, Stamp, Option<Vec<u8>>)> {
        fn visit(p: &Path, out: &mut Vec<(PathBuf, Stamp, Option<Vec<u8>>)>) {
            let m = fs::symlink_metadata(p).unwrap();
            out.push((
                p.to_owned(),
                Stamp::new(&m),
                if m.is_file() {
                    Some(fs::read(p).unwrap())
                } else {
                    None
                },
            ));
            if m.is_dir() {
                for e in fs::read_dir(p).unwrap() {
                    visit(&e.unwrap().path(), out);
                }
            }
        }
        let mut out = vec![];
        visit(root, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
    #[test]
    fn probe_and_persistent_roots_match_and_unchanged_does_not_mutate_store() {
        let (_temp, store, source) = fixture();
        let stop = AtomicBool::new(false);
        let first = automatic(&store, &source, &stop, None).unwrap();
        let super::super::AutomaticCapture::Created(first) = first else {
            panic!()
        };
        let before = state(store.root());
        let second = automatic(&store, &source, &stop, None).unwrap();
        let super::super::AutomaticCapture::Unchanged(second) = second else {
            panic!()
        };
        assert_eq!(first.id(), second.id());
        assert_eq!(state(store.root()), before);
        let manual = super::create(&store, &source).unwrap();
        assert_eq!(manual.root_hash(), first.root_hash());
    }
    #[test]
    fn observable_source_change_between_passes_never_publishes() {
        let (_temp, store, source) = fixture();
        let stop = AtomicBool::new(false);
        HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(|path| {
                fs::write(path.join("nested/file"), b"changed").unwrap()
            }))
        });
        assert!(matches!(
            automatic(&store, &source, &stop, None),
            Err(Error::Unstable(_))
        ));
        assert!(super::super::list_snapshots(&store).unwrap().is_empty());
        assert!(store.root().join("capture.pending").exists());
        automatic(&store, &source, &stop, None).unwrap();
        assert!(!store.root().join("capture.pending").exists());
    }
    #[test]
    fn cancelled_attempt_leaves_no_record_and_recovers_pending_state() {
        let (_temp, store, source) = fixture();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |_| flag.store(true, Ordering::Relaxed)))
        });
        assert!(matches!(
            automatic(&store, &source, &stop, None),
            Err(Error::Cancelled)
        ));
        assert!(super::super::list_snapshots(&store).unwrap().is_empty());
        stop.store(false, Ordering::Relaxed);
        automatic(&store, &source, &stop, None).unwrap();
        assert!(!store.root().join("capture.pending").exists());
    }
    #[test]
    fn identical_sources_verify_chunks_and_missing_metadata_instead_of_trusting_roots() {
        for corrupt in [false, true] {
            let (_temp, store, source) = fixture();
            let stop = AtomicBool::new(false);
            automatic(&store, &source, &stop, None).unwrap();
            if corrupt {
                let path = store
                    .chunk_path(&crate::hashing::hash(&[0, 255, 42]))
                    .unwrap();
                fs::write(path, b"bad").unwrap();
            } else {
                let path = fs::read_dir(store.root().join("manifests"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                fs::remove_file(path).unwrap();
            }
            assert!(automatic(&store, &source, &stop, None).is_err());
            assert_eq!(super::super::list_snapshots(&store).unwrap().len(), 1);
            assert!(!store.root().join("capture.pending").exists());
        }
    }
    #[test]
    fn concurrent_automatic_writers_create_one_baseline() {
        let (_temp, store, source) = fixture();
        std::thread::scope(|scope| {
            let a =
                scope.spawn(|| automatic(&store, &source, &AtomicBool::new(false), None).unwrap());
            let b =
                scope.spawn(|| automatic(&store, &source, &AtomicBool::new(false), None).unwrap());
            let count = [a.join().unwrap(), b.join().unwrap()]
                .into_iter()
                .filter(|r| matches!(r, super::super::AutomaticCapture::Created(_)))
                .count();
            assert_eq!(count, 1);
        });
        assert_eq!(super::super::list_snapshots(&store).unwrap().len(), 1);
    }
    #[test]
    fn cancellation_after_barrier_keeps_recovery_marker_without_history() {
        let (_temp, store, source) = fixture();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        BEFORE_PUBLISH.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || flag.store(true, Ordering::Relaxed)))
        });
        assert!(matches!(
            automatic(&store, &source, &stop, None),
            Err(Error::Cancelled)
        ));
        assert!(super::super::list_snapshots(&store).unwrap().is_empty());
        assert!(store.root().join("capture.pending").exists());
        stop.store(false, Ordering::Relaxed);
        automatic(&store, &source, &stop, None).unwrap();
        assert!(!store.root().join("capture.pending").exists());
    }
    #[test]
    fn automatic_object_and_recovery_barrier_failures_remain_fatal_and_marked() {
        let (_temp, store, source) = fixture();
        let stop = AtomicBool::new(false);
        crate::transaction::set_test_fault(Some("object_directory_sync"));
        assert!(automatic(&store, &source, &stop, None).is_err());
        crate::transaction::set_test_fault(Some("recovery_sync"));
        assert!(automatic(&store, &source, &stop, None).is_err());
        crate::transaction::set_test_fault(None);
        assert!(store.root().join("capture.pending").exists());
        assert!(super::super::list_snapshots(&store).unwrap().is_empty());
        automatic(&store, &source, &stop, None).unwrap();
        assert!(!store.root().join("capture.pending").exists());
    }
}
