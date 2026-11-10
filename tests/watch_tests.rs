#![cfg(unix)]
use rewind::{ChunkStore, watch::Monitor};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::{fs::symlink, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

struct Running {
    child: Child,
    lines: Receiver<Value>,
    seen: Vec<Value>,
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Running {
    fn start(work: &Path, source: &str) -> Self {
        Self::options(work, source, &[])
    }
    fn options(work: &Path, source: &str, options: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rewind"))
            .args(["watch", source])
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
                let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut running = Self {
            child,
            lines,
            seen: vec![],
        };
        running.wait_for(|v| v["status"] == "watching");
        running
    }
    fn wait_for(&mut self, predicate: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| {
                    panic!(
                        "no matching watch event: {e}; seen={:?}; child={:?}",
                        self.seen,
                        self.child.try_wait()
                    )
                });
            let matches = predicate(&line);
            self.seen.push(line);
            if matches {
                return;
            }
        }
    }
    fn path(&mut self, path: &str) {
        self.wait_for(|v| has_path(v, path));
    }
    fn exited(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "watch process did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self) {
        assert!(
            Command::new("/bin/kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        self.wait_for(|v| v["status"] == "stopped");
        assert!(self.exited().success());
    }
}
fn fixture() -> (tempfile::TempDir, ChunkStore, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join(".rewind")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("nested")).unwrap();
    (temp, store, source)
}

#[test]
fn native_cli_logs_create_modify_rename_delete_nested_changes_and_ctrl_c() {
    let (temp, store, source) = fixture();
    fs::write(source.join("existing.bin"), b"initial").unwrap();
    let mut run = Running::start(temp.path(), "source");
    fs::write(source.join("nested/new.bin"), [0, 255, 128]).unwrap();
    run.path("nested/new.bin");
    fs::write(source.join("existing.bin"), b"modified").unwrap();
    run.path("existing.bin");
    fs::rename(
        source.join("nested/new.bin"),
        source.join("nested/renamed.bin"),
    )
    .unwrap();
    run.path("nested/renamed.bin");
    fs::remove_file(source.join("nested/renamed.bin")).unwrap();
    run.path("nested/renamed.bin");
    fs::create_dir(source.join("new-dir")).unwrap();
    run.path("new-dir");
    // Give recursive native registration a path event before writing its child.
    fs::write(source.join("new-dir/child"), b"nested").unwrap();
    run.path("new-dir/child");
    fs::rename(source.join("new-dir"), source.join("moved-dir")).unwrap();
    run.path("moved-dir");
    run.stop();
    assert!(rewind::snapshot::list_snapshots(&store).unwrap().is_empty());
    assert!(!store.root().join("capture.pending").exists());
    assert!(
        fs::read_dir(store.root().join("chunks"))
            .unwrap()
            .next()
            .is_none()
    );
    // Native event kinds vary; every emitted data line uses the stable vocabulary.
    for event in &run.seen {
        if let Some(kind) = event["kind"].as_str() {
            assert_eq!(kind, "batch");
        }
    }
}

#[test]
fn actual_nested_store_is_excluded_but_unrelated_dot_rewind_is_logged() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir_all(project.join("ordinary/.rewind")).unwrap();
    // The CLI store is inside the watched root, reached through a parent alias.
    let store = ChunkStore::init(project.join("vault")).unwrap();
    symlink(temp.path(), temp.path().join("alias")).unwrap();
    let monitor = Monitor::start(&store, temp.path().join("alias/project")).unwrap();
    let stop = monitor.stop_handle();
    let (tx, rx) = mpsc::channel();
    struct Lines {
        tx: mpsc::Sender<Value>,
        buffer: Vec<u8>,
    }
    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buffer.extend_from_slice(bytes);
            while let Some(i) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=i).collect();
                self.tx
                    .send(serde_json::from_slice(&line).unwrap())
                    .unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let handle = thread::spawn(move || monitor.run(&mut Lines { tx, buffer: vec![] }));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(12)).unwrap()["status"],
        "watching"
    );
    store.put(b"real storage mutation").unwrap();
    fs::write(project.join("ordinary/.rewind/real-source"), b"source").unwrap();
    let mut seen = vec![];
    loop {
        let v = rx.recv_timeout(Duration::from_secs(12)).unwrap();
        let found = has_path(&v, "ordinary/.rewind/real-source");
        seen.push(v);
        if found {
            break;
        }
    }
    stop.stop();
    handle.join().unwrap().unwrap();
    seen.extend(rx.try_iter());
    for event in seen {
        for path in batch_paths(&event) {
            assert!(!path.starts_with("vault"));
        }
    }
    assert!(rewind::snapshot::list_snapshots(&store).unwrap().is_empty());
}

#[test]
fn unsupported_startup_entries_and_invalid_roots_are_rejected_without_writes() {
    let (_temp, store, source) = fixture();
    symlink("missing", source.join("link")).unwrap();
    assert!(Monitor::start(&store, &source).is_err());
    fs::remove_file(source.join("link")).unwrap();
    let socket = UnixListener::bind(source.join("socket")).unwrap();
    assert!(Monitor::start(&store, &source).is_err());
    drop(socket);
    fs::remove_file(source.join("socket")).unwrap();
    assert!(Monitor::start(&store, store.root()).is_err());
    symlink(&source, source.with_file_name("source-link")).unwrap();
    assert!(Monitor::start(&store, source.with_file_name("source-link/")).is_err());
    fs::write(source.join("bad:name"), b"unsupported name").unwrap();
    assert!(Monitor::start(&store, &source).is_err());
    assert!(rewind::snapshot::list_snapshots(&store).unwrap().is_empty());
}

#[test]
fn runtime_symlink_and_root_removal_fail_with_no_snapshots() {
    for replacement in [false, true] {
        let (temp, store, source) = fixture();
        let mut run = Running::start(temp.path(), "source");
        if replacement {
            fs::remove_dir_all(&source).unwrap();
        } else {
            symlink("missing", source.join("new-link")).unwrap();
        }
        assert!(!run.exited().success());
        assert!(rewind::snapshot::list_snapshots(&store).unwrap().is_empty());
    }
}

#[test]
fn cli_watching_dot_excludes_its_store_and_allows_concurrent_manual_capture() {
    let (temp, store, source) = fixture();
    let mut run = Running::start(temp.path(), ".");
    store.put(b"manual stored chunk").unwrap();
    let manual = rewind::snapshot::create_snapshot(&store, &source).unwrap();
    fs::write(source.join("probe"), b"ordinary source event").unwrap();
    run.path("source/probe");
    run.stop();
    for event in &run.seen {
        for path in batch_paths(event) {
            assert!(path != ".rewind" && !path.starts_with(".rewind/"));
        }
    }
    let history = rewind::snapshot::list_snapshots(&store).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id(), manual.id());
}

fn has_path(batch: &Value, path: &str) -> bool {
    batch_paths(batch).contains(&path)
}
fn batch_paths(batch: &Value) -> Vec<&str> {
    batch["changes"]
        .as_array()
        .map(|changes| changes.iter().filter_map(|c| c["path"].as_str()).collect())
        .unwrap_or_default()
}

#[test]
fn rapid_writes_atomic_save_and_directory_rename_emit_bounded_change_batches() {
    let (temp, store, source) = fixture();
    fs::write(source.join("target"), b"old").unwrap();
    let mut run = Running::options(
        temp.path(),
        "source",
        &["--debounce-ms", "250", "--max-delay-ms", "1500"],
    );
    assert_eq!(run.seen[0]["debounce_ms"], 250);
    for i in 0..12 {
        fs::write(source.join("target"), [i]).unwrap();
    }
    run.path("target");
    let batch = run.seen.last().unwrap();
    assert_eq!(
        batch["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["path"] == "target")
            .count(),
        1
    );
    fs::write(source.join(".save-temp"), b"replacement").unwrap();
    fs::rename(source.join(".save-temp"), source.join("target")).unwrap();
    run.path("target");
    assert!(
        !run.seen.last().unwrap()["renames"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fs::rename(source.join("nested"), source.join("renamed-dir")).unwrap();
    run.path("renamed-dir");
    assert_eq!(run.seen.last().unwrap()["reconciliation_required"], true);
    fs::write(source.join("ephemeral"), b"x").unwrap();
    fs::remove_file(source.join("ephemeral")).unwrap();
    run.path("ephemeral");
    run.stop();
    assert!(rewind::snapshot::list_snapshots(&store).unwrap().is_empty());
}

#[test]
fn continuous_native_writes_emit_before_the_writer_becomes_quiet() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (temp, store, source) = fixture();
    let mut run = Running::options(
        temp.path(),
        "source",
        &["--debounce-ms", "300", "--max-delay-ms", "600"],
    );
    let finished = Arc::new(AtomicBool::new(false));
    let done = finished.clone();
    let writer = thread::spawn(move || {
        for i in 0..70 {
            fs::write(source.join("continuous"), [i]).unwrap();
            thread::sleep(Duration::from_millis(30));
        }
        done.store(true, Ordering::SeqCst);
    });
    run.path("continuous");
    assert!(
        !finished.load(Ordering::SeqCst),
        "continuous activity starved batch emission"
    );
    writer.join().unwrap();
    run.stop();
    assert!(rewind::snapshot::list_snapshots(&store).unwrap().is_empty());
}

#[test]
fn native_events_arriving_during_output_processing_form_a_later_batch() {
    let (_temp, store, source) = fixture();
    let monitor = Monitor::start_with_config(
        &store,
        &source,
        rewind::watch::DebounceConfig::from_millis(50, 250).unwrap(),
    )
    .unwrap();
    let stop = monitor.stop_handle();
    let (lines_tx, lines_rx) = mpsc::channel();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    struct BlockingLines {
        lines: mpsc::Sender<Value>,
        blocked: mpsc::Sender<()>,
        release: Receiver<()>,
        buffer: Vec<u8>,
        once: bool,
    }
    impl std::io::Write for BlockingLines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buffer.extend_from_slice(bytes);
            while let Some(i) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=i).collect();
                let value: Value = serde_json::from_slice(&line).unwrap();
                let batch = value["kind"] == "batch";
                self.lines.send(value).unwrap();
                if batch && !self.once {
                    self.once = true;
                    self.blocked.send(()).unwrap();
                    self.release.recv_timeout(Duration::from_secs(10)).unwrap();
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let handle = thread::spawn(move || {
        monitor.run(&mut BlockingLines {
            lines: lines_tx,
            blocked: blocked_tx,
            release: release_rx,
            buffer: vec![],
            once: false,
        })
    });
    assert_eq!(
        lines_rx.recv_timeout(Duration::from_secs(10)).unwrap()["status"],
        "watching"
    );
    fs::write(source.join("first"), b"first batch").unwrap();
    blocked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let first = lines_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(has_path(&first, "first"));
    fs::write(source.join("during-processing"), b"second batch").unwrap();
    // Output remains blocked while the independent native callback queues hints.
    thread::sleep(Duration::from_millis(150));
    release_tx.send(()).unwrap();
    loop {
        let batch = lines_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if has_path(&batch, "during-processing") {
            break;
        }
    }
    stop.stop();
    handle.join().unwrap().unwrap();
    assert!(rewind::snapshot::list_snapshots(&store).unwrap().is_empty());
}

#[test]
fn invalid_timing_options_are_rejected_before_startup() {
    let (temp, _store, _source) = fixture();
    for options in [
        vec!["--debounce-ms", "0"],
        vec!["--debounce-ms", "-1"],
        vec!["--max-delay-ms", "299"],
        vec!["--debounce-ms", "abc"],
        vec!["--unknown", "300"],
        vec!["--debounce-ms"],
        vec!["--debounce-ms", "10", "--debounce-ms", "20"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_rewind"))
            .args(["watch", "source"])
            .args(options)
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
    }
}
