//! Native event logging only. No capture or storage mutation is performed here.
pub use crate::debounce::DebounceConfig;
use crate::{ChunkStore, Error, Result, debounce::Debouncer};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{ModifyKind, RenameMode},
};
use serde::Serialize;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    time::Instant,
};

const QUEUE_CAPACITY: usize = 1024;
const MAX_EVENT_PATHS: usize = 64;
const MAX_EVENT_PATH_BYTES: usize = 64 * 1024;

enum Message {
    Event(notify::Result<Event>),
    Wake,
}

/// A cloneable shutdown request. A wake message interrupts the blocking receiver.
#[derive(Clone)]
pub struct StopHandle {
    sender: SyncSender<Message>,
    stopped: Arc<AtomicBool>,
}
impl StopHandle {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        // A full queue is already sufficient to wake the receiver.
        let _ = self.sender.try_send(Message::Wake);
    }
}

/// Backend-independent event hints, not debounced saves or a replayable journal.
#[derive(Debug, Serialize)]
pub struct NormalizedEvent {
    pub kind: &'static str,
    pub paths: Vec<String>,
    pub tracker: Option<usize>,
    pub reconciliation_reason: Option<&'static str>,
}
impl NormalizedEvent {
    fn reconcile(reason: &'static str) -> Self {
        Self {
            kind: "reconcile",
            paths: vec![],
            tracker: None,
            reconciliation_reason: Some(reason),
        }
    }
}

/// Owns the native watcher for the entire monitoring lifetime.
/// Startup validates the tree without storing, hashing or creating snapshots.
pub struct Monitor {
    _watcher: RecommendedWatcher,
    receiver: Receiver<Message>,
    stop: StopHandle,
    overflow: Arc<AtomicBool>,
    config: DebounceConfig,
    #[cfg(unix)]
    source: unix::Source,
}

fn enqueue(sender: &SyncSender<Message>, overflow: &AtomicBool, event: notify::Result<Event>) {
    if event
        .as_ref()
        .is_ok_and(|e| matches!(e.kind, EventKind::Access(_)) && !e.need_rescan())
    {
        return;
    }
    let paths = match &event {
        Ok(e) => &e.paths,
        Err(e) => &e.paths,
    };
    let oversized = paths.len() > MAX_EVENT_PATHS
        || paths
            .iter()
            .map(|p| p.as_os_str().as_encoded_bytes().len())
            .sum::<usize>()
            > MAX_EVENT_PATH_BYTES;
    if oversized {
        overflow.store(true, Ordering::Relaxed);
        let _ = sender.try_send(Message::Wake);
    } else if let Err(TrySendError::Full(_)) = sender.try_send(Message::Event(event)) {
        overflow.store(true, Ordering::Relaxed);
    }
}

impl Monitor {
    pub fn start(store: &ChunkStore, directory: impl AsRef<Path>) -> Result<Self> {
        Self::start_with_config(store, directory, DebounceConfig::default())
    }
    pub fn start_with_config(
        store: &ChunkStore,
        directory: impl AsRef<Path>,
        config: DebounceConfig,
    ) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (store, directory);
            return Err(Error::Invalid(
                "watch is currently supported only on Unix".into(),
            ));
        }
        #[cfg(unix)]
        {
            let source = unix::Source::open(store, directory.as_ref())?;
            let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
            let overflow = Arc::new(AtomicBool::new(false));
            let exclusion = source.storage_filter();
            let callback_sender = sender.clone();
            let callback_overflow = overflow.clone();
            let mut watcher = RecommendedWatcher::new(
                move |event: notify::Result<Event>| {
                    if event.as_ref().is_ok_and(|e| exclusion.matches(e)) {
                        return;
                    }
                    enqueue(&callback_sender, &callback_overflow, event);
                },
                notify::Config::default().with_follow_symlinks(false),
            )
            .map_err(watch_error)?;
            // Register first, validate second: changes during validation are queued.
            watcher
                .watch(&source.root, RecursiveMode::Recursive)
                .map_err(watch_error)?;
            source.validate()?;
            let stop = StopHandle {
                sender,
                stopped: Arc::new(AtomicBool::new(false)),
            };
            Ok(Self {
                _watcher: watcher,
                receiver,
                stop,
                overflow,
                config,
                source,
            })
        }
    }

    pub fn stop_handle(&self) -> StopHandle {
        self.stop.clone()
    }

    /// Block while idle. The caller registers its own process signal handler.
    pub fn run(self, output: &mut impl Write) -> Result<()> {
        #[cfg(unix)]
        {
            writeln!(
                output,
                "{}",
                serde_json::json!({"status":"watching", "root":self.source.root, "backend":format!("{:?}", RecommendedWatcher::kind()), "debounce_ms":self.config.debounce_ms(), "max_delay_ms":self.config.max_delay_ms()})
            )?;
            output.flush()?;
            let mut debounce = Debouncer::new(self.config);
            loop {
                let now = Instant::now();
                if self.overflow.swap(false, Ordering::AcqRel) {
                    debounce.reconcile("queue_overflow_or_oversized_event", now);
                }
                if self.stop.stopped.load(Ordering::Relaxed) {
                    // Do not claim queued hints were processed. Flush our bounded
                    // accumulator, mark any remaining queue for future reconciliation.
                    for _ in 0..QUEUE_CAPACITY {
                        match self.receiver.try_recv() {
                            Ok(Message::Event(_)) => {
                                debounce.reconcile("shutdown_pending_events", now);
                                break;
                            }
                            Ok(Message::Wake) => (),
                            Err(_) => break,
                        }
                    }
                    emit(&mut debounce, output)?;
                    break;
                }
                if debounce.deadline().is_some_and(|deadline| now >= deadline) {
                    emit(&mut debounce, output)?;
                    continue;
                }
                let message = match debounce.deadline() {
                    Some(deadline) => match self
                        .receiver
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(message) => message,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err(Error::Invalid("watch event channel disconnected".into()));
                        }
                    },
                    None => self
                        .receiver
                        .recv()
                        .map_err(|_| Error::Invalid("watch event channel disconnected".into()))?,
                };
                if let Message::Event(event) = message {
                    match event {
                        Ok(event) => match self.source.normalize(event) {
                            Ok(Some(event)) => debounce.push(event, Instant::now()),
                            Ok(None) => (),
                            Err(error) => {
                                debounce.reconcile("validation_error", Instant::now());
                                emit(&mut debounce, output)?;
                                return Err(error);
                            }
                        },
                        Err(error) => {
                            debounce.reconcile("backend_error", Instant::now());
                            emit(&mut debounce, output)?;
                            return Err(watch_error(error));
                        }
                    }
                }
            }
            writeln!(output, "{}", serde_json::json!({"status":"stopped"}))?;
            output.flush()?;
        }
        Ok(())
    }
}

fn emit(debounce: &mut Debouncer, output: &mut impl Write) -> Result<()> {
    if let Some(batch) = debounce.take() {
        serde_json::to_writer(&mut *output, &batch)?;
        writeln!(output)?;
        output.flush()?;
    }
    Ok(())
}

fn watch_error(error: notify::Error) -> Error {
    Error::Invalid(format!(
        "watch backend error; event history may be incomplete; restart required: {error}"
    ))
}

fn kind(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Create(_) => "create",
        EventKind::Remove(_) => "remove",
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => "rename-from",
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => "rename-to",
        EventKind::Modify(ModifyKind::Name(_)) => "rename",
        EventKind::Modify(_) => "modify",
        EventKind::Any => "any",
        _ => "other",
    }
}

#[cfg(unix)]
#[path = "watch_auto.rs"]
mod automatic;

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::tree::validate_name;
    use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags};
    use std::{
        fs::{self, File},
        os::unix::fs::MetadataExt,
        path::Component,
    };

    pub(super) struct Source {
        pub root: PathBuf,
        handle: File,
        identity: (u64, u64),
        store: PathBuf,
        store_identity: (u64, u64),
    }
    pub(super) struct StorageFilter {
        path: PathBuf,
        identity: (u64, u64),
    }
    impl StorageFilter {
        pub fn matches(&self, event: &Event) -> bool {
            !event.need_rescan()
                && !event.paths.is_empty()
                && event.paths.iter().all(|p| {
                    !p.components().any(|c| matches!(c, Component::ParentDir))
                        && p.starts_with(&self.path)
                })
                && fs::symlink_metadata(&self.path)
                    .is_ok_and(|m| m.is_dir() && (m.dev(), m.ino()) == self.identity)
        }
    }
    fn identity(file: &File) -> Result<(u64, u64)> {
        let m = file.metadata()?;
        Ok((m.dev(), m.ino()))
    }
    fn open_dir(parent: impl std::os::fd::AsFd, path: &Path) -> Result<File> {
        Ok(rfs::openat(
            parent,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?
        .into())
    }
    impl Source {
        pub fn open(store: &ChunkStore, directory: &Path) -> Result<Self> {
            let directory: PathBuf = directory.components().collect();
            let handle = open_dir(rfs::CWD, &directory)?;
            let root = fs::canonicalize(directory)?;
            if root.to_str().is_none() {
                return Err(Error::Invalid("watch source path must be UTF-8".into()));
            }
            let id = identity(&handle)?;
            let m = fs::metadata(&root)?;
            if id != (m.dev(), m.ino()) {
                return Err(Error::Invalid("watch source changed while opening".into()));
            }
            let store = fs::canonicalize(store.root())?;
            if root.starts_with(&store) {
                return Err(Error::Invalid(
                    "cannot watch the store or its descendants".into(),
                ));
            }
            let m = fs::metadata(&store)?;
            Ok(Self {
                root,
                handle,
                identity: id,
                store,
                store_identity: (m.dev(), m.ino()),
            })
        }
        pub(super) fn check_roots(&self) -> Result<()> {
            for (path, expected) in [
                (&self.root, self.identity),
                (&self.store, self.store_identity),
            ] {
                let m = fs::symlink_metadata(path)?;
                if !m.is_dir() || (m.dev(), m.ino()) != expected {
                    return Err(Error::Invalid(
                        "watched root or storage directory moved/replaced; restart required".into(),
                    ));
                }
            }
            Ok(())
        }
        pub(super) fn root_identity(&self) -> (u64, u64) {
            self.identity
        }
        pub(super) fn store_root(&self) -> PathBuf {
            self.store.clone()
        }
        pub fn storage_filter(&self) -> StorageFilter {
            StorageFilter {
                path: self.store.clone(),
                identity: self.store_identity,
            }
        }
        pub fn validate(&self) -> Result<()> {
            self.check_roots()?;
            self.scan(&self.handle, 0)
        }
        fn scan(&self, directory: &File, depth: usize) -> Result<()> {
            if depth > 256 {
                return Err(Error::Invalid("directory depth exceeds 256".into()));
            }
            // No whole-tree index: retain one directory iterator per ancestor.
            for entry in Dir::read_from(directory).map_err(std::io::Error::from)? {
                let entry = entry.map_err(std::io::Error::from)?;
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                let name = name
                    .to_str()
                    .map_err(|_| Error::Invalid("non-UTF-8 watch entry".into()))?;
                let stat = rfs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(std::io::Error::from)?;
                match FileType::from_raw_mode(stat.st_mode) {
                    FileType::Directory => {
                        let child = open_dir(directory, Path::new(name))?;
                        if identity(&child)? == self.store_identity {
                            continue;
                        }
                        validate_name(name)?;
                        self.scan(&child, depth + 1)?;
                    }
                    FileType::RegularFile => validate_name(name)?,
                    _ => {
                        return Err(Error::Invalid(format!(
                            "symlinks and special files are unsupported: {name:?}"
                        )));
                    }
                }
            }
            Ok(())
        }
        // Walk existing ancestors with NOFOLLOW. A missing suffix is normal for
        // deletion/rename events; never canonicalize a source symlink into a target.
        fn validate_path(&self, relative: &Path) -> Result<bool> {
            if relative.as_os_str().is_empty() {
                self.scan(&self.handle, 0)?;
            }
            let mut directory = self.handle.try_clone()?;
            let parts: Vec<_> = relative.components().collect();
            for (i, part) in parts.iter().enumerate() {
                let Component::Normal(name) = part else {
                    return Err(Error::Invalid("unexpected event path component".into()));
                };
                let name = name
                    .to_str()
                    .ok_or_else(|| Error::Invalid("non-UTF-8 watch entry".into()))?;
                let stat =
                    match rfs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(stat) => stat,
                        Err(rustix::io::Errno::NOENT) => {
                            for part in &parts[i..] {
                                validate_name(part.as_os_str().to_str().ok_or_else(|| {
                                    Error::Invalid("non-UTF-8 watch entry".into())
                                })?)?;
                            }
                            return Ok(true);
                        }
                        Err(e) => return Err(std::io::Error::from(e).into()),
                    };
                match FileType::from_raw_mode(stat.st_mode) {
                    FileType::Directory => {
                        directory = open_dir(&directory, Path::new(name))?;
                        if identity(&directory)? == self.store_identity {
                            return Ok(false);
                        }
                        validate_name(name)?;
                        if i == parts.len() - 1 {
                            self.scan(&directory, parts.len())?;
                        }
                    }
                    FileType::RegularFile => {
                        validate_name(name)?;
                        // Old children can be reported after a directory became a file.
                        if i + 1 != parts.len() {
                            return Ok(true);
                        }
                    }
                    _ => {
                        return Err(Error::Invalid(format!(
                            "symlinks and special files are unsupported: {relative:?}"
                        )));
                    }
                }
            }
            Ok(true)
        }
        pub fn normalize(&self, event: Event) -> Result<Option<NormalizedEvent>> {
            self.check_roots()?;
            if event.need_rescan() {
                return Ok(Some(NormalizedEvent::reconcile("backend_rescan")));
            }
            // Access/read notifications are not content-change hints.
            if matches!(event.kind, EventKind::Access(_)) {
                return Ok(None);
            }
            if event.paths.is_empty() {
                return Ok(Some(NormalizedEvent::reconcile("missing_paths")));
            }
            let tracker = event.tracker();
            let paired = matches!(
                event.kind,
                EventKind::Modify(ModifyKind::Name(RenameMode::Both))
            ) && event.paths.len() == 2;
            let mut paths = Vec::new();
            let mut endpoints = Vec::new();
            for (index, path) in event.paths.into_iter().enumerate() {
                if !path.is_absolute()
                    || path.components().any(|c| matches!(c, Component::ParentDir))
                {
                    return Ok(Some(NormalizedEvent::reconcile("unresolvable_path")));
                }
                // The registered root is canonical; deleted paths cannot themselves
                // be canonicalized. Canonical prefixes anchor their lexical suffixes.
                let Ok(relative) = path.strip_prefix(&self.root) else {
                    return Ok(Some(NormalizedEvent::reconcile("unresolvable_path")));
                };
                if path.starts_with(&self.store) {
                    continue;
                }
                if relative.components().count() > 257 {
                    return Err(Error::Invalid("directory depth exceeds 256".into()));
                }
                if !self.validate_path(relative)? {
                    continue;
                }
                let relative = if relative.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    relative
                        .to_str()
                        .ok_or_else(|| Error::Invalid("non-UTF-8 watch path".into()))?
                        .to_owned()
                };
                if !paths.contains(&relative) {
                    paths.push(relative);
                    endpoints.push(index);
                }
            }
            if paths.is_empty() {
                return Ok(None);
            }
            Ok(Some(NormalizedEvent {
                kind: if paired && endpoints == [0] {
                    "rename-from"
                } else if paired && endpoints == [1] {
                    "rename-to"
                } else {
                    kind(event.kind)
                },
                paths,
                tracker,
                reconciliation_reason: None,
            }))
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        fn fixture() -> (tempfile::TempDir, Source) {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            fs::create_dir(&project).unwrap();
            let store = ChunkStore::init(project.join("storage")).unwrap();
            let source = Source::open(&store, &project).unwrap();
            (temp, source)
        }
        #[test]
        fn normalized_renames_preserve_order_and_store_boundary_crossings() {
            let (_temp, source) = fixture();
            let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(source.root.join("before"))
                .add_path(source.root.join("after"));
            let event = source.normalize(event).unwrap().unwrap();
            assert_eq!(event.kind, "rename");
            assert_eq!(event.paths, ["before", "after"]);
            let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(source.store.join("before"))
                .add_path(source.root.join("after"));
            let filtered = source.normalize(event).unwrap().unwrap();
            assert_eq!(filtered.paths, ["after"]);
            assert_eq!(filtered.kind, "rename-to");
            let filter = source.storage_filter();
            assert!(
                filter.matches(&Event::new(EventKind::Any).add_path(source.store.join("chunk")))
            );
            assert!(
                !filter.matches(
                    &Event::new(EventKind::Any)
                        .set_flag(notify::event::Flag::Rescan)
                        .add_path(source.store.join("chunk"))
                )
            );
            assert!(
                source
                    .normalize(Event::new(EventKind::Any).add_path(source.store.join("deleted")))
                    .unwrap()
                    .is_none()
            );
            fs::create_dir_all(source.root.join("unrelated/.rewind")).unwrap();
            let event =
                Event::new(EventKind::Any).add_path(source.root.join("unrelated/.rewind/deleted"));
            assert_eq!(
                source.normalize(event).unwrap().unwrap().paths,
                ["unrelated/.rewind/deleted"]
            );
        }
        #[test]
        fn rescan_unknown_paths_and_unsupported_runtime_entries_fail_closed() {
            use notify::event::Flag;
            let (_temp, source) = fixture();
            assert!(
                source
                    .normalize(Event::new(EventKind::Any).set_flag(Flag::Rescan))
                    .unwrap()
                    .unwrap()
                    .reconciliation_reason
                    .is_some()
            );
            assert!(
                source
                    .normalize(Event::new(EventKind::Any))
                    .unwrap()
                    .unwrap()
                    .reconciliation_reason
                    .is_some()
            );
            for path in [
                PathBuf::from("relative"),
                source.root.join("../escape"),
                PathBuf::from("/outside"),
            ] {
                assert!(
                    source
                        .normalize(Event::new(EventKind::Any).add_path(path))
                        .unwrap()
                        .unwrap()
                        .reconciliation_reason
                        .is_some()
                );
            }
            std::os::unix::fs::symlink("missing", source.root.join("link")).unwrap();
            assert!(
                source
                    .normalize(Event::new(EventKind::Any).add_path(source.root.join("link")))
                    .is_err()
            );
            // Coarse root-directory hints still validate newly added entries.
            assert!(
                source
                    .normalize(Event::new(EventKind::Any).add_path(source.root.clone()))
                    .is_err()
            );
        }
        #[test]
        fn moved_or_replaced_store_identity_requires_restart() {
            let (_temp, source) = fixture();
            fs::rename(&source.store, source.root.join("moved-storage")).unwrap();
            fs::create_dir(&source.store).unwrap();
            assert!(
                source
                    .normalize(Event::new(EventKind::Any).add_path(source.root.join("file")))
                    .is_err()
            );
        }
    }
}

#[cfg(not(unix))]
impl Monitor {
    pub fn run_automatic_with_reconciliation(
        self,
        _output: &mut impl Write,
        _interval: std::time::Duration,
    ) -> Result<()> {
        Err(Error::Invalid("automatic capture requires Unix".into()))
    }
    pub fn run_automatic(self, _output: &mut impl Write) -> Result<()> {
        Err(Error::Invalid(
            "automatic watching is supported only on Unix".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_queue_and_oversized_messages_report_overflow() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let overflow = AtomicBool::new(false);
        enqueue(&sender, &overflow, Ok(Event::new(EventKind::Any)));
        enqueue(&sender, &overflow, Ok(Event::new(EventKind::Any)));
        assert!(overflow.load(Ordering::Relaxed));
        assert!(matches!(receiver.recv().unwrap(), Message::Event(_)));
        overflow.store(false, Ordering::Relaxed);
        let mut event = Event::new(EventKind::Any);
        event.paths = vec![PathBuf::from("/file"); MAX_EVENT_PATHS + 1];
        enqueue(&sender, &overflow, Ok(event));
        assert!(overflow.load(Ordering::Relaxed));
        assert!(matches!(receiver.recv().unwrap(), Message::Wake));
        overflow.store(false, Ordering::Relaxed);
        let event = Event::new(EventKind::Any)
            .add_path(PathBuf::from("x".repeat(MAX_EVENT_PATH_BYTES + 1)));
        enqueue(&sender, &overflow, Ok(event));
        assert!(overflow.load(Ordering::Relaxed));
    }
    #[cfg(unix)]
    #[test]
    fn loss_emits_reconciliation_and_later_native_batches_keep_the_warning() {
        use std::{fs, thread, time::Duration};
        let temp = tempfile::tempdir().unwrap();
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let monitor = Monitor::start_with_config(
            &store,
            &source,
            DebounceConfig::from_millis(30, 100).unwrap(),
        )
        .unwrap();
        monitor.overflow.store(true, Ordering::Relaxed);
        monitor
            .stop
            .sender
            .try_send(Message::Event(Ok(
                Event::new(EventKind::Any).set_flag(notify::event::Flag::Rescan)
            )))
            .unwrap();
        let stop = monitor.stop_handle();
        let (tx, rx) = mpsc::channel();
        struct Reporter {
            buffer: Vec<u8>,
            tx: mpsc::Sender<serde_json::Value>,
        }
        impl Write for Reporter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.buffer.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                for line in std::mem::take(&mut self.buffer)
                    .split(|b| *b == b'\n')
                    .filter(|line| !line.is_empty())
                {
                    self.tx.send(serde_json::from_slice(line).unwrap()).unwrap();
                }
                Ok(())
            }
        }
        let handle = thread::spawn(move || monitor.run(&mut Reporter { buffer: vec![], tx }));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap()["status"],
            "watching"
        );
        let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(first["reconciliation_required"], true);
        let reasons = first["reconciliation_reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|r| r == "backend_rescan"));
        assert!(
            reasons
                .iter()
                .any(|r| r == "queue_overflow_or_oversized_event")
        );
        fs::write(source.join("later"), b"later native event").unwrap();
        loop {
            let batch = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(batch["reconciliation_required"], true);
            if batch["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == "later")
            {
                break;
            }
        }
        stop.stop();
        handle.join().unwrap().unwrap();
        assert!(crate::snapshot::list_snapshots(&store).unwrap().is_empty());
    }

    #[test]
    fn stop_wakes_idle_receiver_and_access_events_do_not_fill_queue() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let overflow = AtomicBool::new(false);
        enqueue(
            &sender,
            &overflow,
            Ok(Event::new(EventKind::Access(
                notify::event::AccessKind::Any,
            ))),
        );
        assert!(receiver.try_recv().is_err());
        assert!(!overflow.load(Ordering::Relaxed));
        let stop = StopHandle {
            sender,
            stopped: Arc::new(AtomicBool::new(false)),
        };
        stop.stop();
        assert!(matches!(receiver.recv().unwrap(), Message::Wake));
        assert!(stop.stopped.load(Ordering::Relaxed));
        stop.stop();
        stop.stop(); // Full queue never blocks the signal-handler thread.
    }
}
