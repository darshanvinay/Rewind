#![cfg(any(target_os = "macos", target_os = "linux"))]

use proptest::prelude::*;
use rewind::{
    ChunkStore, FileManifest, hashing, restore_snapshot,
    snapshot::{SnapshotRecord, create_snapshot, list_snapshots},
    tree::{DirectoryEntry, DirectoryNode, EntryTarget},
};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
};

fn record_path(store: &ChunkStore, id: &str) -> PathBuf {
    store.root().join("snapshots").join(format!("{id}.json"))
}
fn tree_path(store: &ChunkStore, hash: &str) -> PathBuf {
    store.root().join("trees").join(&hash[..2]).join(&hash[2..])
}
fn alter_record(store: &ChunkStore, id: &str, mutate: impl FnOnce(&mut serde_json::Value)) {
    let path = record_path(store, id);
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    mutate(&mut value);
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}
fn assert_no_stage(parent: &Path) {
    for entry in fs::read_dir(parent).unwrap() {
        assert!(
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rewind-restore-")
        );
    }
}

#[test]
fn history_is_empty_then_sorted_and_ignores_temporary_files() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    assert!(list_snapshots(&store).unwrap().is_empty());
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    alter_record(&store, first.id(), |value| {
        value["created_unix_seconds"] = 20.into();
        value["created_nanoseconds"] = 0.into();
    });
    alter_record(&store, second.id(), |value| {
        value["created_unix_seconds"] = 10.into();
        value["created_nanoseconds"] = 1.into();
    });
    fs::write(store.root().join("snapshots/.pending-incomplete"), b"{").unwrap();
    fs::write(store.root().join("snapshots/notes.json"), b"not a record").unwrap();
    let loaded = list_snapshots(&store).unwrap();
    assert_eq!(
        loaded.iter().map(SnapshotRecord::id).collect::<Vec<_>>(),
        [second.id(), first.id()]
    );
    assert_eq!(loaded[0].root_hash(), second.root_hash());
    assert_eq!(loaded[0].source_path(), second.source_path());
    assert_eq!(loaded[0].created_at(), (10, 1));
    // Equal timestamps use the ID as a stable tiebreaker.
    alter_record(&store, first.id(), |value| {
        value["created_unix_seconds"] = 10.into();
        value["created_nanoseconds"] = 1.into();
    });
    let loaded = list_snapshots(&store).unwrap();
    assert!(loaded[0].id() < loaded[1].id());
    fs::write(record_path(&store, first.id()), b"{").unwrap();
    assert!(list_snapshots(&store).is_err());
}

#[test]
fn malformed_records_ids_and_history_paths_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let snapshot = create_snapshot(&store, &source).unwrap();
    let path = record_path(&store, snapshot.id());
    let original = fs::read(&path).unwrap();
    for id in ["../escape", "", "UPPERCASE", &"f".repeat(64)] {
        assert!(SnapshotRecord::load(&store, id).is_err());
    }
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("id", serde_json::json!(hashing::hash(b"wrong"))),
        ("root_hash", serde_json::json!("../escape")),
        ("source_path", serde_json::json!("relative")),
        ("source_path", serde_json::json!("/bad/../path")),
        ("source_path", serde_json::json!("/bad\0path")),
        ("created_nanoseconds", serde_json::json!(1_000_000_000)),
        ("created_unix_seconds", serde_json::json!(-1)),
        ("unexpected", serde_json::json!(true)),
    ] {
        fs::write(&path, &original).unwrap();
        alter_record(&store, snapshot.id(), |record| record[field] = value);
        assert!(
            SnapshotRecord::load(&store, snapshot.id()).is_err(),
            "{field}"
        );
        assert!(restore_snapshot(&store, snapshot.id(), temp.path().join("output")).is_err());
    }
    for bytes in [b"{}".to_vec(), b"null".to_vec(), vec![b' '; 64 * 1024 + 1]] {
        fs::write(&path, bytes).unwrap();
        assert!(SnapshotRecord::load(&store, snapshot.id()).is_err());
    }
    fs::remove_file(&path).unwrap();
    symlink(source.join("missing"), &path).unwrap();
    assert!(SnapshotRecord::load(&store, snapshot.id()).is_err());
    assert!(list_snapshots(&store).is_err());
    fs::remove_file(&path).unwrap();
    fs::remove_dir_all(store.root().join("snapshots")).unwrap();
    symlink(source.join("missing"), store.root().join("snapshots")).unwrap();
    assert!(list_snapshots(&store).is_err());
    assert!(!temp.path().join("output").exists());
    assert_no_stage(temp.path());
}

#[test]
fn restore_independent_history_empty_directories_binary_and_deleted_source() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("nested/empty")).unwrap();
    let bytes: Vec<_> = (0..1_000_000).map(|i| (i * 37) as u8).collect();
    fs::write(source.join("nested/binary"), &bytes).unwrap();
    fs::write(source.join("empty-file"), b"").unwrap();
    fs::write(source.join("text"), b"before").unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    fs::write(source.join("text"), b"after").unwrap();
    fs::remove_file(source.join("empty-file")).unwrap();
    fs::write(source.join("added"), b"new").unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    let a = temp.path().join("restored-a");
    let b = temp.path().join("restored-b");
    restore_snapshot(&store, first.id(), &a).unwrap();
    assert_eq!(fs::read(source.join("text")).unwrap(), b"after");
    // Restoration depends on stored objects, not the live source.
    fs::remove_dir_all(&source).unwrap();
    restore_snapshot(&store, second.id(), &b).unwrap();
    assert_eq!(fs::read(a.join("text")).unwrap(), b"before");
    assert_eq!(fs::read(b.join("text")).unwrap(), b"after");
    assert_eq!(fs::read(a.join("nested/binary")).unwrap(), bytes);
    assert_eq!(fs::read(b.join("nested/binary")).unwrap(), bytes);
    assert_eq!(
        hashing::hash(&fs::read(a.join("nested/binary")).unwrap()),
        hashing::hash(&bytes)
    );
    assert!(a.join("nested/empty").is_dir() && b.join("nested/empty").is_dir());
    assert_eq!(fs::read(a.join("empty-file")).unwrap(), b"");
    assert!(!b.join("empty-file").exists() && !a.join("added").exists());
    assert_eq!(fs::read(b.join("added")).unwrap(), b"new");
    assert_no_stage(temp.path());
}

#[test]
fn refuses_existing_outputs_symlinks_and_destinations_in_source_or_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let snapshot = create_snapshot(&store, &source).unwrap();
    for name in ["empty", "occupied"] {
        fs::create_dir(temp.path().join(name)).unwrap();
    }
    fs::write(temp.path().join("occupied/keep"), b"keep").unwrap();
    fs::write(temp.path().join("file"), b"keep").unwrap();
    symlink(temp.path().join("occupied"), temp.path().join("link")).unwrap();
    symlink(temp.path().join("missing"), temp.path().join("dangling")).unwrap();
    for name in ["empty", "occupied", "file", "link", "dangling"] {
        assert!(
            restore_snapshot(&store, snapshot.id(), temp.path().join(name)).is_err(),
            "{name}"
        );
    }
    assert!(restore_snapshot(&store, snapshot.id(), &source).is_err());
    assert!(restore_snapshot(&store, snapshot.id(), source.join("new-output")).is_err());
    assert!(restore_snapshot(&store, snapshot.id(), store.root().join("new-output")).is_err());
    assert!(
        restore_snapshot(
            &store,
            snapshot.id(),
            temp.path().join("absent-parent/output")
        )
        .is_err()
    );
    assert_eq!(
        fs::read(temp.path().join("occupied/keep")).unwrap(),
        b"keep"
    );
    assert_eq!(fs::read(temp.path().join("file")).unwrap(), b"keep");
    assert!(!temp.path().join("missing").exists());
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    assert_no_stage(temp.path());
    let output = temp.path().join("fresh");
    restore_snapshot(&store, snapshot.id(), &output).unwrap();
    assert_eq!(fs::read_dir(output).unwrap().count(), 0);
}

#[test]
fn corrupt_or_missing_references_leave_no_partial_output() {
    // Exercise a failure after an earlier file has already been staged.
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("z-directory")).unwrap();
    fs::write(source.join("a-first"), b"first").unwrap();
    fs::write(source.join("z-directory/last"), b"last").unwrap();
    let snapshot = create_snapshot(&store, &source).unwrap();
    let root = DirectoryNode::load(&store, snapshot.root_hash()).unwrap();
    let child = match root.entries()[1].target() {
        EntryTarget::Directory { tree } => tree,
        _ => panic!("directory expected"),
    };
    let child_node = DirectoryNode::load(&store, child).unwrap();
    let manifest = match child_node.entries()[0].target() {
        EntryTarget::File { manifest, .. } => manifest,
        _ => panic!("file expected"),
    };
    let manifest_path = store
        .root()
        .join("manifests")
        .join(format!("{manifest}.json"));
    let chunk_path = store.chunk_path(&hashing::hash(b"last")).unwrap();
    for path in [tree_path(&store, child), manifest_path, chunk_path] {
        let original = fs::read(&path).unwrap();
        for corrupt in [true, false] {
            if corrupt {
                fs::write(&path, b"corrupt").unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
            let output = temp.path().join("failed-output");
            assert!(restore_snapshot(&store, snapshot.id(), &output).is_err());
            assert!(!output.exists());
            assert_no_stage(temp.path());
            fs::write(&path, &original).unwrap();
        }
    }
    restore_snapshot(&store, snapshot.id(), temp.path().join("complete")).unwrap();
}

#[test]
fn malicious_nodes_manifest_identity_and_length_are_verified() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("data"), b"bytes").unwrap();
    let snapshot = create_snapshot(&store, &source).unwrap();
    let node = DirectoryNode::load(&store, snapshot.root_hash()).unwrap();
    let (manifest_hash, length) = match node.entries()[0].target() {
        EntryTarget::File { manifest, length } => (manifest, *length),
        _ => panic!("file expected"),
    };
    let wrong_length = DirectoryNode::new(vec![
        DirectoryEntry::new(
            "data",
            EntryTarget::File {
                manifest: manifest_hash.clone(),
                length: length + 1,
            },
        )
        .unwrap(),
    ])
    .unwrap()
    .save(&store)
    .unwrap();
    alter_record(&store, snapshot.id(), |record| {
        record["root_hash"] = wrong_length.into()
    });
    assert!(restore_snapshot(&store, snapshot.id(), temp.path().join("output")).is_err());
    // A valid but different manifest must not pass under the expected identity.
    alter_record(&store, snapshot.id(), |record| {
        record["root_hash"] = snapshot.root_hash().into()
    });
    let original_manifest = FileManifest::load_by_hash(&store, manifest_hash).unwrap();
    let mut alternate = original_manifest.clone();
    alternate.file_hash = hashing::hash(b"wrong");
    fs::write(
        store
            .root()
            .join("manifests")
            .join(format!("{manifest_hash}.json")),
        serde_json::to_vec(&alternate).unwrap(),
    )
    .unwrap();
    assert!(FileManifest::load_by_hash(&store, manifest_hash).is_err());
    assert!(restore_snapshot(&store, snapshot.id(), temp.path().join("output")).is_err());
    // Even a hash-valid stored node is untrusted until its names/schema validate.
    for name in ["../escaped", "/absolute", "a/b", "a\\b", ".."] {
        // Preserve canonical schema order so an unsafe name is the reason for rejection.
        let escaped_name = serde_json::to_string(name).unwrap();
        let bytes = format!("{{\"version\":1,\"object\":\"rewind-directory\",\"entries\":[{{\"name\":{escaped_name},\"target\":{{\"kind\":\"file\",\"manifest\":\"{manifest_hash}\",\"length\":{length}}}}}]}}").into_bytes();
        let malicious_hash = hashing::hash(&bytes);
        let path = tree_path(&store, &malicious_hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        alter_record(&store, snapshot.id(), |record| {
            record["root_hash"] = malicious_hash.into()
        });
        assert!(restore_snapshot(&store, snapshot.id(), temp.path().join("output")).is_err());
    }
    assert!(!temp.path().join("output").exists() && !temp.path().join("escaped").exists());
    assert_eq!(fs::read(source.join("data")).unwrap(), b"bytes");
    assert_no_stage(temp.path());
}

#[test]
fn destination_name_collisions_cannot_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("data"), b"bytes").unwrap();
    let snapshot = create_snapshot(&store, &source).unwrap();
    let original = DirectoryNode::load(&store, snapshot.root_hash()).unwrap();
    let target = original.entries()[0].target().clone();
    let node = DirectoryNode::new(vec![
        DirectoryEntry::new("Name", target.clone()).unwrap(),
        DirectoryEntry::new("name", target).unwrap(),
    ])
    .unwrap();
    let hash = node.save(&store).unwrap();
    alter_record(&store, snapshot.id(), |record| {
        record["root_hash"] = hash.into()
    });
    // Probe the actual destination filesystem's case behavior.
    fs::write(temp.path().join("CaseProbe"), b"probe").unwrap();
    let folds_case = temp.path().join("caseprobe").exists();
    fs::remove_file(temp.path().join("CaseProbe")).unwrap();
    let output = temp.path().join("output");
    let result = restore_snapshot(&store, snapshot.id(), &output);
    if folds_case {
        assert!(result.is_err());
        assert!(!output.exists());
    } else {
        result.unwrap();
        assert_eq!(fs::read_dir(output).unwrap().count(), 2);
    }
    assert_no_stage(temp.path());
}

#[test]
fn cli_lists_history_and_restores_two_versions() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("source/nested/empty")).unwrap();
    fs::write(temp.path().join("source/nested/file"), b"before").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rewind"))
            .args(args)
            .current_dir(temp.path())
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    let first = run(&["snapshot", "source"]);
    assert!(first.status.success());
    let first = String::from_utf8(first.stdout).unwrap();
    fs::write(temp.path().join("source/nested/file"), b"after").unwrap();
    fs::write(temp.path().join("source/added"), b"added").unwrap();
    let second = run(&["snapshot", "source"]);
    assert!(second.status.success());
    let second = String::from_utf8(second.stdout).unwrap();
    let listed = run(&["snapshots"]);
    assert!(listed.status.success());
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert_eq!(listed.lines().count(), 3);
    assert!(listed.contains(first.trim()) && listed.contains(second.trim()));
    assert!(listed.contains("source") && listed.contains("UNIX_TIMESTAMP"));
    assert!(
        run(&["restore-snapshot", first.trim(), "a"])
            .status
            .success()
    );
    assert!(
        run(&["restore-snapshot", second.trim(), "b"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(temp.path().join("a/nested/file")).unwrap(),
        b"before"
    );
    assert_eq!(
        fs::read(temp.path().join("b/nested/file")).unwrap(),
        b"after"
    );
    assert!(
        temp.path().join("a/nested/empty").is_dir() && temp.path().join("b/nested/empty").is_dir()
    );
    assert!(!temp.path().join("a/added").exists());
    assert_eq!(fs::read(temp.path().join("b/added")).unwrap(), b"added");
    assert!(
        !run(&["restore-snapshot", first.trim(), "a"])
            .status
            .success()
    );
    assert!(
        !run(&["restore-snapshot", "../invalid", "bad"])
            .status
            .success()
    );
    assert_eq!(
        fs::read(temp.path().join("source/nested/file")).unwrap(),
        b"after"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 8, failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/snapshot_restore_tests.proptest-regressions"))), ..ProptestConfig::default() })]
    #[test]
    fn arbitrary_file_maps_restore_exact_bytes(files in prop::collection::btree_map("f-[a-z]{1,8}", prop::collection::vec(any::<u8>(), 0..4096), 0..6)) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested/empty")).unwrap();
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        for (name, bytes) in &files { fs::write(source.join("nested").join(name), bytes).unwrap(); }
        let snapshot = create_snapshot(&store, &source).unwrap();
        let output = temp.path().join("restored");
        restore_snapshot(&store, snapshot.id(), &output).unwrap();
        prop_assert!(output.join("nested/empty").is_dir());
        prop_assert_eq!(fs::read_dir(output.join("nested")).unwrap().count(), files.len() + 1);
        for (name, bytes) in files { prop_assert_eq!(fs::read(output.join("nested").join(name)).unwrap(), bytes); }
        assert_no_stage(temp.path());
    }
}
