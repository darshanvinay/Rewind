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
        let mut child = Command::new(env!("CARGO_BIN_EXE_rewind"))
            .args(["watch", source])
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
        self.wait_for(|v| {
            v["paths"]
                .as_array()
                .is_some_and(|ps| ps.iter().any(|p| p == path))
        });
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
            assert!(
                [
                    "create",
                    "modify",
                    "remove",
                    "rename",
                    "rename-from",
                    "rename-to",
                    "any",
                    "other"
                ]
                .contains(&kind)
            );
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
        let found = v["paths"]
            .as_array()
            .is_some_and(|ps| ps.iter().any(|p| p == "ordinary/.rewind/real-source"));
        seen.push(v);
        if found {
            break;
        }
    }
    stop.stop();
    handle.join().unwrap().unwrap();
    seen.extend(rx.try_iter());
    for event in seen {
        if let Some(paths) = event["paths"].as_array() {
            assert!(
                paths
                    .iter()
                    .all(|p| !p.as_str().unwrap().starts_with("vault"))
            );
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
        if let Some(paths) = event["paths"].as_array() {
            assert!(paths.iter().all(|p| {
                let path = p.as_str().unwrap();
                path != ".rewind" && !path.starts_with(".rewind/")
            }));
        }
    }
    let history = rewind::snapshot::list_snapshots(&store).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id(), manual.id());
}
