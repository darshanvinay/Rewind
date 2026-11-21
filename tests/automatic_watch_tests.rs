#![cfg(unix)]
use rewind::{
    ChunkStore,
    snapshot::{AutomaticCapture, capture_automatic, list_snapshots},
};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::AtomicBool,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

struct Watch {
    child: Child,
    lines: Receiver<Value>,
    seen: Vec<Value>,
}
impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Watch {
    fn start(work: &Path, source: &str) -> Self {
        Self::start_options(work, source, &[])
    }
    fn start_options(work: &Path, source: &str, options: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rewind"))
            .args([
                "watch",
                source,
                "--debounce-ms",
                "50",
                "--max-delay-ms",
                "250",
            ])
            .args(options)
            .current_dir(work)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx
                    .send(serde_json::from_str(&line.unwrap()).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut this = Self {
            child,
            lines,
            seen: vec![],
        };
        this.wait(|v| v["status"] == "watching");
        this
    }
    fn wait(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let value = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| {
                    panic!(
                        "{e}; seen={:?}; child={:?}",
                        self.seen,
                        self.child.try_wait()
                    )
                });
            let matched = predicate(&value);
            self.seen.push(value.clone());
            if matched {
                return value;
            }
        }
    }
    fn baseline(&mut self) -> Value {
        self.wait(|v| v["kind"] == "snapshot_created" || v["kind"] == "skipped_identical")
    }
    fn created(&mut self) -> Value {
        self.wait(|v| v["kind"] == "snapshot_created")
    }
    fn stop(&mut self) {
        assert!(
            Command::new("/bin/kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        self.wait(|v| v["status"] == "stopped");
        assert!(self.child.wait().unwrap().success());
    }
}
fn fixture() -> (tempfile::TempDir, ChunkStore, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join(".rewind")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("nested/empty")).unwrap();
    fs::write(source.join("file"), b"initial").unwrap();
    (temp, store, source)
}
fn id(value: &Value) -> &str {
    value["snapshot_id"].as_str().unwrap()
}

#[test]
fn baseline_edits_atomic_rename_deletion_nested_binary_and_historical_restore() {
    let (temp, store, source) = fixture();
    let mut watch = Watch::start(temp.path(), "source");
    let a = watch.baseline();
    assert_eq!(a["kind"], "snapshot_created");
    fs::write(source.join("file"), b"edited").unwrap();
    fs::write(source.join("nested/binary"), [0, 255, 42]).unwrap();
    let b = watch.created();
    fs::write(source.join(".save-temp"), b"replacement").unwrap();
    fs::rename(source.join(".save-temp"), source.join("file")).unwrap();
    let c = watch.created();
    fs::remove_file(source.join("file")).unwrap();
    fs::rename(source.join("nested"), source.join("moved")).unwrap();
    let d = watch.created();
    watch.stop();
    fs::rename(&source, temp.path().join("source-gone")).unwrap();
    for (name, v) in [("a", &a), ("b", &b), ("c", &c), ("d", &d)] {
        rewind::restore_snapshot(&store, id(v), temp.path().join(name)).unwrap();
    }
    assert_eq!(fs::read(temp.path().join("a/file")).unwrap(), b"initial");
    assert!(temp.path().join("a/nested/empty").is_dir());
    assert_eq!(fs::read(temp.path().join("b/file")).unwrap(), b"edited");
    assert_eq!(
        fs::read(temp.path().join("b/nested/binary")).unwrap(),
        [0, 255, 42]
    );
    assert_eq!(
        fs::read(temp.path().join("c/file")).unwrap(),
        b"replacement"
    );
    assert!(!temp.path().join("d/file").exists());
    assert!(temp.path().join("d/moved/empty").is_dir());
}
#[test]
fn restart_and_same_content_suppress_records_without_pending_markers() {
    let (temp, store, source) = fixture();
    let mut a = Watch::start(temp.path(), "source");
    let first = a.baseline();
    a.stop();
    let count = list_snapshots(&store).unwrap().len();
    let mut b = Watch::start(temp.path(), "source");
    let same = b.baseline();
    assert_eq!(same["kind"], "skipped_identical");
    assert_eq!(id(&first), id(&same));
    fs::write(source.join("file"), b"initial").unwrap();
    b.wait(|v| v["kind"] == "skipped_identical");
    b.stop();
    assert_eq!(list_snapshots(&store).unwrap().len(), count);
    assert!(!store.root().join("capture.pending").exists());
}
#[test]
fn storage_corruption_is_fatal_instead_of_skipping_an_identical_root() {
    let (temp, store, source) = fixture();
    capture_automatic(&store, &source, &AtomicBool::new(false)).unwrap();
    let path = store
        .chunk_path(&rewind::hashing::hash(b"initial"))
        .unwrap();
    fs::write(&path, b"corrupt").unwrap();
    let mut watch = Watch::start(temp.path(), "source");
    let error = watch.wait(|v| v["kind"] == "error");
    assert_eq!(error["fatal"], true);
    assert!(!watch.child.wait().unwrap().success());
    assert_eq!(list_snapshots(&store).unwrap().len(), 1);
    assert_eq!(fs::read(path).unwrap(), b"corrupt");
}
#[test]
fn two_watchers_and_manual_writers_serialize_and_restore_the_result() {
    let (temp, store, source) = fixture();
    let mut a = Watch::start(temp.path(), "source");
    a.baseline();
    let mut b = Watch::start(temp.path(), "source");
    assert_eq!(b.baseline()["kind"], "skipped_identical");
    let manual = rewind::snapshot::create_snapshot(&store, &source).unwrap();
    fs::write(source.join("file"), b"shared edit").unwrap();
    let final_record = a.wait(|v| {
        (v["kind"] == "snapshot_created" || v["kind"] == "skipped_identical")
            && v["root_hash"] != manual.root_hash()
    });
    b.wait(|v| {
        (v["kind"] == "snapshot_created" || v["kind"] == "skipped_identical")
            && v["root_hash"] != manual.root_hash()
    });
    a.stop();
    b.stop();
    let root = final_record["root_hash"].as_str().unwrap();
    assert_eq!(
        list_snapshots(&store)
            .unwrap()
            .iter()
            .filter(|r| r.root_hash() == root)
            .count(),
        1
    );
    rewind::restore_snapshot(&store, id(&final_record), temp.path().join("restored")).unwrap();
    assert_eq!(
        fs::read(temp.path().join("restored/file")).unwrap(),
        b"shared edit"
    );
}
#[test]
fn rapid_saves_reconcile_final_bytes_without_recording_every_notification() {
    let (temp, store, source) = fixture();
    let mut watch = Watch::start(temp.path(), "source");
    watch.baseline();
    for i in 0..25 {
        fs::write(source.join("file"), format!("save-{i}")).unwrap();
    }
    let result = watch.created();
    watch.stop();
    rewind::restore_snapshot(&store, id(&result), temp.path().join("restored")).unwrap();
    assert_eq!(
        fs::read(temp.path().join("restored/file")).unwrap(),
        b"save-24"
    );
    assert!(list_snapshots(&store).unwrap().len() < 26);
}
#[test]
fn nested_storage_does_not_cause_a_snapshot_feedback_loop() {
    let (temp, store, source) = fixture();
    let mut watch = Watch::start(temp.path(), ".");
    let baseline = watch.baseline();
    // Native ancestor hints may cause read-only skips, but never more records.
    thread::sleep(Duration::from_millis(400));
    let count = list_snapshots(&store).unwrap().len();
    assert_eq!(count, 1);
    store.put(b"unreferenced manual object").unwrap();
    thread::sleep(Duration::from_millis(400));
    assert_eq!(list_snapshots(&store).unwrap().len(), 1);
    fs::write(source.join("file"), b"nested edit").unwrap();
    let next = watch.created();
    assert_ne!(next["root_hash"], baseline["root_hash"]);
    watch.stop();
    assert!(!store.root().join("capture.pending").exists());
}
#[test]
fn cancellation_while_waiting_for_writer_lock_exits_without_a_snapshot() {
    let (temp, store, _source) = fixture();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.root().join("writer.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut watch = Watch::start(temp.path(), "source");
    watch.wait(|v| v["kind"] == "reconciliation" && v["status"] == "started");
    watch.stop();
    drop(lock);
    assert!(list_snapshots(&store).unwrap().is_empty());
    assert!(!store.root().join("capture.pending").exists());
}
#[test]
fn automatic_capture_scopes_latest_record_to_canonical_source() {
    let (temp, store, source) = fixture();
    let a = capture_automatic(&store, &source, &AtomicBool::new(false)).unwrap();
    assert!(matches!(a, AutomaticCapture::Created(_)));
    let other = temp.path().join("other");
    fs::create_dir(&other).unwrap();
    rewind::snapshot::create_snapshot(&store, &other).unwrap();
    assert!(matches!(
        capture_automatic(&store, &source, &AtomicBool::new(false)).unwrap(),
        AutomaticCapture::Unchanged(_)
    ));
}

#[test]
fn shadow_diagnostics_compare_roots_but_only_c_publishes() {
    let (temp, store, source) = fixture();
    let mut watch = Watch::start(temp.path(), "source");
    watch.baseline();
    fs::write(source.join("file"), b"shadow-only construction").unwrap();
    let created = watch.created();
    let shadow = watch
        .seen
        .iter()
        .rev()
        .find(|v| v["kind"] == "shadow")
        .unwrap();
    assert_eq!(shadow["publisher"], "milestone_c");
    assert_eq!(shadow["status"], "candidate");
    assert_eq!(shadow["matches_authoritative"], true);
    assert_eq!(shadow["root_hash"], created["root_hash"]);
    assert_eq!(shadow["stats"]["files_read"], 1);
    assert!(shadow["cache_entries"].as_u64().unwrap() > 0);
    watch.stop();
    assert_eq!(list_snapshots(&store).unwrap().len(), 2);
}

#[test]
fn warm_shadow_cache_never_replaces_c_archive_integrity_verification() {
    let (temp, store, source) = fixture();
    let mut watch = Watch::start(temp.path(), "source");
    watch.baseline();
    let chunk = store
        .chunk_path(&rewind::hashing::hash(b"initial"))
        .unwrap();
    fs::write(chunk, b"corrupt").unwrap();
    // Source contents are identical; C must still read/verify the archived chunk.
    fs::write(source.join("file"), b"initial").unwrap();
    let error = watch.wait(|v| v["kind"] == "error");
    assert_eq!(error["fatal"], true);
    assert!(!watch.child.wait().unwrap().success());
    assert_eq!(list_snapshots(&store).unwrap().len(), 1);
}

#[test]
fn periodic_strict_reconciliation_skips_identical_without_writing_history() {
    let (temp, store, _) = fixture();
    let mut watch = Watch::start_options(
        temp.path(),
        "source",
        &["--mode", "strict", "--reconcile-interval-secs", "1"],
    );
    let baseline = watch.baseline();
    assert_eq!(baseline["capture_mode"], "strict");
    assert_eq!(baseline["record_source_validation"], "strict_two_pass");
    let periodic = watch.wait(|v| v["kind"] == "skipped_identical" && v["reason"] == "periodic");
    assert_eq!(id(&periodic), id(&baseline));
    watch.stop();
    assert_eq!(list_snapshots(&store).unwrap().len(), 1);
    assert!(!store.root().join("capture.pending").exists());
}

#[test]
fn provenance_is_versioned_and_legacy_records_remain_unknown() {
    use rewind::snapshot::{SnapshotRecord, SourceValidation};
    let (temp, store, source) = fixture();
    let created = capture_automatic(&store, &source, &AtomicBool::new(false)).unwrap();
    let AutomaticCapture::Created(record) = created else {
        panic!("expected baseline")
    };
    assert_eq!(
        record.source_validation(),
        Some(SourceValidation::StrictTwoPass)
    );
    let path = store
        .root()
        .join("snapshots")
        .join(format!("{}.json", record.id()));
    let mut wire: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    wire["source_validation"] = "fast".into();
    fs::write(&path, serde_json::to_vec(&wire).unwrap()).unwrap();
    assert!(SnapshotRecord::load(&store, record.id()).is_err());
    wire["version"] = 1.into();
    wire.as_object_mut().unwrap().remove("source_validation");
    fs::write(&path, serde_json::to_vec(&wire).unwrap()).unwrap();
    assert_eq!(
        SnapshotRecord::load(&store, record.id())
            .unwrap()
            .source_validation(),
        None
    );
    rewind::restore_snapshot(&store, record.id(), temp.path().join("legacy-restore")).unwrap();
    assert_eq!(
        fs::read(temp.path().join("legacy-restore/file")).unwrap(),
        b"initial"
    );
    wire["version"] = 2.into();
    fs::write(&path, serde_json::to_vec(&wire).unwrap()).unwrap();
    assert!(SnapshotRecord::load(&store, record.id()).is_err());
}

#[test]
fn periodic_scan_detects_idle_archive_corruption_and_fails_closed() {
    let (temp, store, _) = fixture();
    let mut watch =
        Watch::start_options(temp.path(), "source", &["--reconcile-interval-secs", "1"]);
    watch.baseline();
    // Archive-only mutations are excluded from source event handling. The timer
    // must independently wake the worker and verify reused archive objects.
    let shard = fs::read_dir(store.root().join("chunks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let chunk = fs::read_dir(shard).unwrap().next().unwrap().unwrap().path();
    fs::write(chunk, b"corrupted").unwrap();
    let error = watch.wait(|v| v["kind"] == "error" && v["fatal"] == true);
    assert_eq!(error["code"], "integrity_storage_or_source_policy");
    assert!(!watch.child.wait().unwrap().success());
    assert_eq!(list_snapshots(&store).unwrap().len(), 1);
    assert!(!store.root().join("capture.pending").exists());
}
