#![cfg(unix)]

use proptest::prelude::*;
use rewind::{
    ChunkStore,
    chunking::ChunkSizes,
    diff::{Change, ChangeKind, EntryKind},
    diff_snapshots, hashing,
    snapshot::create_snapshot,
    store_file,
    tree::{DirectoryEntry, DirectoryNode, EntryTarget},
};
use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};

fn change(path: &str, kind: ChangeKind, entry_kind: EntryKind) -> Change {
    Change {
        path: path.into(),
        kind,
        entry_kind,
    }
}
fn tree_path(store: &ChunkStore, hash: &str) -> PathBuf {
    store.root().join("trees").join(&hash[..2]).join(&hash[2..])
}
fn manifest_path(store: &ChunkStore, hash: &str) -> PathBuf {
    store.root().join("manifests").join(format!("{hash}.json"))
}
fn set_root(store: &ChunkStore, id: &str, root: &str) {
    let path = store.root().join("snapshots").join(format!("{id}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["root_hash"] = root.into();
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
}
fn file_manifest(node: &DirectoryNode, name: &str) -> String {
    match node
        .entries()
        .iter()
        .find(|entry| entry.name() == name)
        .unwrap()
        .target()
    {
        EntryTarget::File { manifest, .. } => manifest.clone(),
        _ => panic!("expected file"),
    }
}

#[test]
fn equal_roots_ignore_capture_identity_but_require_a_valid_root() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let first = create_snapshot(&store, &a).unwrap();
    let second = create_snapshot(&store, &b).unwrap();
    assert_ne!(first.id(), second.id());
    assert_ne!(first.source_path(), second.source_path());
    assert_eq!(first.root_hash(), second.root_hash());
    assert!(
        diff_snapshots(&store, first.id(), second.id())
            .unwrap()
            .is_empty()
    );
    let path = tree_path(&store, first.root_hash());
    fs::write(&path, b"corrupt").unwrap();
    assert!(diff_snapshots(&store, first.id(), second.id()).is_err());
    fs::remove_file(path).unwrap();
    assert!(diff_snapshots(&store, first.id(), first.id()).is_err());
    assert!(diff_snapshots(&store, "../escape", second.id()).is_err());
}

#[test]
fn nested_changes_include_directory_descendants_and_reverse_cleanly() {
    use ChangeKind::{Added as A, Deleted as D, Modified as M};
    use EntryKind::{Directory as Dir, File as F};
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("deleted-dir/empty")).unwrap();
    fs::create_dir_all(source.join("left")).unwrap();
    fs::create_dir_all(source.join("stable")).unwrap();
    for (name, bytes) in [
        ("deleted-dir/old", b"old".as_slice()),
        ("left/file", b"before"),
        ("stable/keep", b"same"),
        ("gone", b"gone"),
    ] {
        fs::write(source.join(name), bytes).unwrap();
    }
    let first = create_snapshot(&store, &source).unwrap();
    fs::remove_dir_all(source.join("deleted-dir")).unwrap();
    fs::remove_file(source.join("gone")).unwrap();
    fs::create_dir_all(source.join("new-dir/empty")).unwrap();
    fs::write(source.join("new-dir/new"), b"new").unwrap();
    fs::write(source.join("added"), b"added").unwrap();
    fs::write(source.join("left/file"), [0, 255, 128]).unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    let expected = vec![
        change("added", A, F),
        change("deleted-dir", D, Dir),
        change("deleted-dir/empty", D, Dir),
        change("deleted-dir/old", D, F),
        change("gone", D, F),
        change("left/file", M, F),
        change("new-dir", A, Dir),
        change("new-dir/empty", A, Dir),
        change("new-dir/new", A, F),
    ];
    assert_eq!(
        diff_snapshots(&store, first.id(), second.id()).unwrap(),
        expected
    );
    let reverse: Vec<_> = expected
        .iter()
        .map(|item| Change {
            kind: match item.kind {
                A => D,
                D => A,
                M => M,
            },
            ..item.clone()
        })
        .collect();
    assert_eq!(
        diff_snapshots(&store, second.id(), first.id()).unwrap(),
        reverse
    );
    assert_eq!(fs::read(source.join("left/file")).unwrap(), [0, 255, 128]);
}

#[test]
fn type_replacements_are_delete_then_add_without_rename_detection() {
    use ChangeKind::{Added as A, Deleted as D};
    use EntryKind::{Directory as Dir, File as F};
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("to-file/empty")).unwrap();
    fs::write(source.join("to-file/old"), b"old").unwrap();
    fs::write(source.join("to-dir"), b"before").unwrap();
    fs::write(source.join("rename-old"), b"same").unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    fs::remove_dir_all(source.join("to-file")).unwrap();
    fs::write(source.join("to-file"), b"new").unwrap();
    fs::remove_file(source.join("to-dir")).unwrap();
    fs::create_dir_all(source.join("to-dir/empty")).unwrap();
    fs::write(source.join("to-dir/new"), b"new").unwrap();
    fs::rename(source.join("rename-old"), source.join("rename-new")).unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    assert_eq!(
        diff_snapshots(&store, first.id(), second.id()).unwrap(),
        vec![
            change("rename-new", A, F),
            change("rename-old", D, F),
            change("to-dir", D, F),
            change("to-dir", A, Dir),
            change("to-dir/empty", A, Dir),
            change("to-dir/new", A, F),
            change("to-file", D, Dir),
            change("to-file", A, F),
            change("to-file/empty", D, Dir),
            change("to-file/old", D, F),
        ]
    );
}

#[test]
fn equal_subtrees_are_not_read_and_changed_subtrees_are_verified() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("stable")).unwrap();
    fs::create_dir_all(source.join("changed")).unwrap();
    fs::write(source.join("stable/file"), b"same").unwrap();
    fs::write(source.join("changed/file"), b"before").unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    fs::write(source.join("changed/file"), b"after").unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    let old = DirectoryNode::load(&store, first.root_hash()).unwrap();
    let new = DirectoryNode::load(&store, second.root_hash()).unwrap();
    let child = |node: &DirectoryNode, name: &str| match node
        .entries()
        .iter()
        .find(|entry| entry.name() == name)
        .unwrap()
        .target()
    {
        EntryTarget::Directory { tree } => tree.clone(),
        _ => panic!("expected directory"),
    };
    let stable = child(&old, "stable");
    assert_eq!(stable, child(&new, "stable"));
    // Removing the equal subtree deliberately proves it is never opened. A diff
    // is not an integrity scan and does not assert this archive is restorable.
    fs::remove_file(tree_path(&store, &stable)).unwrap();
    assert_eq!(
        diff_snapshots(&store, first.id(), second.id()).unwrap(),
        vec![change(
            "changed/file",
            ChangeKind::Modified,
            EntryKind::File
        )]
    );
    let path = tree_path(&store, &child(&new, "changed"));
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, b"corrupt").unwrap();
    assert!(diff_snapshots(&store, first.id(), second.id()).is_err());
    fs::remove_file(&path).unwrap();
    assert!(diff_snapshots(&store, first.id(), second.id()).is_err());
    fs::write(path, bytes).unwrap();
}

#[test]
fn file_byte_identity_ignores_rechunking_and_diff_does_not_read_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let data: Vec<u8> = (0..50_000).map(|i| (i * 37 + i / 256) as u8).collect();
    fs::write(source.join("data"), &data).unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    let old = DirectoryNode::load(&store, first.root_hash()).unwrap();
    let rechunked = store_file(
        &store,
        source.join("data"),
        ChunkSizes {
            min: 64,
            avg: 256,
            max: 1024,
        },
    )
    .unwrap();
    let manifest_path = rechunked.save(&store).unwrap();
    let manifest_hash = manifest_path
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_ne!(file_manifest(&old, "data"), manifest_hash);
    let root = DirectoryNode::new(vec![
        DirectoryEntry::new(
            "data",
            EntryTarget::File {
                manifest: manifest_hash,
                length: rechunked.length,
            },
        )
        .unwrap(),
    ])
    .unwrap()
    .save(&store)
    .unwrap();
    set_root(&store, second.id(), &root);
    fs::remove_file(store.chunk_path(&rechunked.chunks[0].hash).unwrap()).unwrap();
    assert!(
        diff_snapshots(&store, first.id(), second.id())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn invalid_manifest_identity_length_and_late_cli_errors_are_reported() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join(".rewind")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("z"), b"before").unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    fs::write(source.join("a"), b"added").unwrap();
    fs::write(source.join("z"), b"after").unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    let node = DirectoryNode::load(&store, second.root_hash()).unwrap();
    let manifest = file_manifest(&node, "z");
    let path = manifest_path(&store, &manifest);
    let original = fs::read(&path).unwrap();
    // The earlier addition would have been emitted by a streaming CLI. Results
    // must instead be collected successfully before anything reaches stdout.
    fs::write(&path, b"corrupt").unwrap();
    assert!(diff_snapshots(&store, first.id(), second.id()).is_err());
    let cli = Command::new(env!("CARGO_BIN_EXE_rewind"))
        .current_dir(temp.path())
        .args(["diff", first.id(), second.id()])
        .output()
        .unwrap();
    assert!(!cli.status.success() && cli.stdout.is_empty() && !cli.stderr.is_empty());
    fs::remove_file(&path).unwrap();
    assert!(diff_snapshots(&store, first.id(), second.id()).is_err());
    fs::write(path, original).unwrap();
    let bad_root = DirectoryNode::new(vec![
        DirectoryEntry::new(
            "z",
            EntryTarget::File {
                manifest,
                length: 999,
            },
        )
        .unwrap(),
    ])
    .unwrap()
    .save(&store)
    .unwrap();
    set_root(&store, second.id(), &bad_root);
    assert!(diff_snapshots(&store, first.id(), second.id()).is_err());
}

#[test]
fn overdeep_added_directory_graph_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    let mut hash = DirectoryNode::new(vec![]).unwrap().save(&store).unwrap();
    for _ in 0..257 {
        hash = DirectoryNode::new(vec![
            DirectoryEntry::new("d", EntryTarget::Directory { tree: hash }).unwrap(),
        ])
        .unwrap()
        .save(&store)
        .unwrap();
    }
    set_root(&store, second.id(), &hash);
    assert!(diff_snapshots(&store, first.id(), second.id()).is_err());
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cli_full_phase2_smoke_diff_and_independent_restoration() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("source/nested/empty")).unwrap();
    let binary: Vec<_> = (0..100_000).map(|i| (i * 37) as u8).collect();
    fs::write(temp.path().join("source/nested/binary"), &binary).unwrap();
    fs::write(temp.path().join("source/nested/file"), b"before").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rewind"))
            .current_dir(temp.path())
            .args(args)
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
    let diff = run(&["diff", first.trim(), second.trim()]);
    assert!(diff.status.success());
    assert_eq!(
        String::from_utf8(diff.stdout).unwrap(),
        "A\tfile\t\"added\"\nM\tfile\t\"nested/file\"\n"
    );
    let identical = run(&["diff", first.trim(), first.trim()]);
    assert!(identical.status.success() && identical.stdout.is_empty());
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
    assert_eq!(fs::read(temp.path().join("b/added")).unwrap(), b"added");
    assert!(!temp.path().join("a/added").exists());
    for directory in ["a", "b"] {
        let output = temp.path().join(directory);
        assert!(output.join("nested/empty").is_dir());
        let restored = fs::read(output.join("nested/binary")).unwrap();
        assert_eq!(restored, binary);
        assert_eq!(hashing::hash(&restored), hashing::hash(&binary));
    }
    assert_eq!(
        fs::read(temp.path().join("source/nested/file")).unwrap(),
        b"after"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/diff_tests.proptest-regressions"))), ..ProptestConfig::default() })]
    #[test]
    fn diff_matches_full_byte_map_oracle(
        before in prop::collection::btree_map("bucket-[ab]/f-[a-z]{1,6}", prop::collection::vec(any::<u8>(), 0..256), 0..6),
        after in prop::collection::btree_map("bucket-[ab]/f-[a-z]{1,6}", prop::collection::vec(any::<u8>(), 0..256), 0..6)
    ) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        for bucket in ["bucket-a", "bucket-b"] { fs::create_dir_all(source.join(bucket)).unwrap(); }
        for (name, data) in &before { fs::write(source.join(name), data).unwrap(); }
        let first = create_snapshot(&store, &source).unwrap();
        for name in before.keys() { fs::remove_file(source.join(name)).unwrap(); }
        for (name, data) in &after { fs::write(source.join(name), data).unwrap(); }
        let second = create_snapshot(&store, &source).unwrap();
        let mut expected = BTreeMap::new();
        for (name, bytes) in &before {
            if !after.contains_key(name) { expected.insert(name.clone(), ChangeKind::Deleted); }
            else if after.get(name) != Some(bytes) { expected.insert(name.clone(), ChangeKind::Modified); }
        }
        for name in after.keys() { if !before.contains_key(name) { expected.insert(name.clone(), ChangeKind::Added); } }
        let expected: Vec<_> = expected.into_iter().map(|(path, kind)| Change { path, kind, entry_kind: EntryKind::File }).collect();
        prop_assert_eq!(diff_snapshots(&store, first.id(), second.id()).unwrap(), expected);
    }
}
