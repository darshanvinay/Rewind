#![cfg(unix)]

use proptest::prelude::*;
use rewind::{
    ChunkStore, FileManifest, hashing, reconstruct,
    snapshot::create_snapshot,
    tree::{DirectoryNode, EntryTarget},
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::symlink, net::UnixListener},
    path::{Path, PathBuf},
    process::Command,
};

fn records(store: &ChunkStore) -> Vec<PathBuf> {
    let directory = store.root().join("snapshots");
    if !directory.exists() {
        return vec![];
    }
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

// Test-only graph walk: independently check file-manifest identities, references,
// and reconstructed bytes without implementing Milestone C restoration APIs.
fn contents(store: &ChunkStore, root: &str) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(
        store: &ChunkStore,
        hash: &str,
        prefix: &Path,
        result: &mut BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) {
        let node = DirectoryNode::load(store, hash).unwrap();
        result.insert(prefix.to_owned(), None);
        for entry in node.entries() {
            let path = prefix.join(entry.name());
            match entry.target() {
                EntryTarget::Directory { tree } => visit(store, tree, &path, result),
                EntryTarget::File { manifest, length } => {
                    let manifest_path = store
                        .root()
                        .join("manifests")
                        .join(format!("{manifest}.json"));
                    assert_eq!(&hashing::hash(&fs::read(&manifest_path).unwrap()), manifest);
                    let manifest = FileManifest::load(manifest_path).unwrap();
                    assert_eq!(manifest.length, *length);
                    let mut bytes = Vec::new();
                    reconstruct(store, &manifest, &mut bytes).unwrap();
                    result.insert(path, Some(bytes));
                }
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(store, root, Path::new(""), &mut result);
    result
}

#[test]
fn nested_capture_unique_records_and_historical_graphs() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("nested/empty")).unwrap();
    fs::write(source.join("nested/binary"), [0, 255, 128, 0, 42]).unwrap();
    fs::write(source.join("text"), b"before").unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    let same = create_snapshot(&store, &source).unwrap();
    assert_eq!(first.root_hash(), same.root_hash());
    assert_ne!(first.id(), same.id());
    hashing::validate_hash(first.id()).unwrap();
    assert_eq!(
        first.source_path(),
        fs::canonicalize(&source).unwrap().to_str().unwrap()
    );
    assert!(first.created_at().0 > 0 && first.created_at().1 < 1_000_000_000);
    let before = contents(&store, first.root_hash());
    assert_eq!(before[Path::new("nested/empty")], None);
    assert_eq!(
        before[Path::new("nested/binary")],
        Some(vec![0, 255, 128, 0, 42])
    );
    assert_eq!(before[Path::new("text")], Some(b"before".to_vec()));
    fs::write(source.join("text"), b"after").unwrap();
    fs::write(source.join("added"), b"new").unwrap();
    let second = create_snapshot(&store, &source).unwrap();
    assert_ne!(first.root_hash(), second.root_hash());
    assert_eq!(contents(&store, first.root_hash()), before);
    let after = contents(&store, second.root_hash());
    assert_eq!(after[Path::new("text")], Some(b"after".to_vec()));
    assert_eq!(after[Path::new("added")], Some(b"new".to_vec()));
    assert_eq!(records(&store).len(), 3);
    let raw: serde_json::Value = serde_json::from_slice(
        &fs::read(
            store
                .root()
                .join("snapshots")
                .join(format!("{}.json", first.id())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["version"], 1);
    assert_eq!(raw["id"], first.id());
    assert_eq!(raw["root_hash"], first.root_hash());
    let first_root = DirectoryNode::load(&store, first.root_hash()).unwrap();
    let second_root = DirectoryNode::load(&store, second.root_hash()).unwrap();
    let unchanged = |node: &DirectoryNode| {
        node.entries()
            .iter()
            .find(|entry| entry.name() == "nested")
            .unwrap()
            .target()
            .clone()
    };
    assert_eq!(unchanged(&first_root), unchanged(&second_root));
}

#[test]
fn empty_root_and_actual_nested_store_exclusion() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let store = ChunkStore::init(source.join(".rewind")).unwrap();
    let first = create_snapshot(&store, &source).unwrap();
    assert_eq!(
        first.root_hash(),
        DirectoryNode::new(vec![]).unwrap().hash().unwrap()
    );
    let second = create_snapshot(&store, &source).unwrap();
    assert_eq!(first.root_hash(), second.root_hash());
    assert!(create_snapshot(&store, store.root()).is_err());
    assert!(create_snapshot(&store, store.root().join("chunks")).is_err());
    fs::create_dir_all(source.join("ordinary/.rewind")).unwrap();
    fs::write(source.join("ordinary/.rewind/keep"), b"ordinary data").unwrap();
    let third = create_snapshot(&store, &source).unwrap();
    let tree = contents(&store, third.root_hash());
    assert!(!tree.contains_key(Path::new(".rewind")));
    assert_eq!(
        tree[Path::new("ordinary/.rewind/keep")],
        Some(b"ordinary data".to_vec())
    );
}

#[test]
fn custom_store_location_is_excluded_without_losing_its_parent() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("nested/custom-store")).unwrap();
    let snapshot = create_snapshot(&store, temp.path()).unwrap();
    let tree = contents(&store, snapshot.root_hash());
    assert_eq!(tree.len(), 2);
    assert_eq!(tree[Path::new("nested")], None);
    assert!(!tree.contains_key(Path::new("nested/custom-store")));
}

#[test]
fn symlinks_dangling_links_and_special_files_do_not_commit() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    let outside = temp.path().join("outside");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"outside bytes").unwrap();
    symlink(&outside, source.join("link")).unwrap();
    assert!(create_snapshot(&store, &source).is_err());
    for root in [
        source.join("link"),
        source.join("link/"),
        source.join("link/."),
    ] {
        assert!(create_snapshot(&store, root).is_err());
    }
    fs::remove_file(source.join("link")).unwrap();
    symlink(outside.join("secret"), source.join("file-link")).unwrap();
    assert!(create_snapshot(&store, &source).is_err());
    fs::remove_file(source.join("file-link")).unwrap();
    symlink(outside.join("missing"), source.join("dangling")).unwrap();
    assert!(create_snapshot(&store, &source).is_err());
    fs::remove_file(source.join("dangling")).unwrap();
    let _socket = UnixListener::bind(source.join("socket")).unwrap();
    assert!(create_snapshot(&store, &source).is_err());
    assert!(records(&store).is_empty());
    assert!(
        !store
            .chunk_path(&hashing::hash(b"outside bytes"))
            .unwrap()
            .exists()
    );
    assert_eq!(fs::read(outside.join("secret")).unwrap(), b"outside bytes");
}

#[test]
fn invalid_input_and_corrupt_chunks_do_not_publish_partial_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("data"), b"content").unwrap();
    let snapshot = create_snapshot(&store, &source).unwrap();
    fs::write(
        store.chunk_path(&hashing::hash(b"content")).unwrap(),
        b"corrupt",
    )
    .unwrap();
    assert!(create_snapshot(&store, &source).is_err());
    assert_eq!(records(&store).len(), 1);
    assert!(
        store
            .root()
            .join("snapshots")
            .join(format!("{}.json", snapshot.id()))
            .exists()
    );
    fs::remove_file(source.join("data")).unwrap();
    for name in ["bad:name", "reserved?"] {
        let path = source.join(name);
        fs::write(&path, b"bytes").unwrap();
        assert!(create_snapshot(&store, &source).is_err());
        fs::remove_file(path).unwrap();
    }
    assert!(create_snapshot(&store, source.join("missing")).is_err());
    fs::write(source.join("file"), b"file").unwrap();
    assert!(create_snapshot(&store, source.join("file")).is_err());
    assert_eq!(records(&store).len(), 1);
}

#[test]
fn publication_errors_and_depth_limit_propagate() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(store.root().join("snapshots"), b"block history directory").unwrap();
    assert!(create_snapshot(&store, &source).is_err());
    assert_eq!(
        fs::read(store.root().join("snapshots")).unwrap(),
        b"block history directory"
    );
    fs::remove_file(store.root().join("snapshots")).unwrap();
    let mut deep = source.clone();
    for _ in 0..257 {
        deep.push("d");
    }
    fs::create_dir_all(deep).unwrap();
    assert!(create_snapshot(&store, source).is_err());
    assert!(records(&store).is_empty());
}

#[test]
fn cli_snapshot_prints_only_the_id_and_commits_a_complete_graph() {
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
    assert!(first.status.success(), "{:?}", first);
    assert!(first.stderr.is_empty());
    let id = String::from_utf8(first.stdout).unwrap();
    hashing::validate_hash(id.trim()).unwrap();
    let store = ChunkStore::open(temp.path().join(".rewind")).unwrap();
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(
            store
                .root()
                .join("snapshots")
                .join(format!("{}.json", id.trim())),
        )
        .unwrap(),
    )
    .unwrap();
    let before = contents(&store, record["root_hash"].as_str().unwrap());
    assert_eq!(before[Path::new("nested/file")], Some(b"before".to_vec()));
    fs::write(temp.path().join("source/nested/file"), b"after").unwrap();
    fs::write(temp.path().join("source/added"), b"added").unwrap();
    assert!(run(&["snapshot", "source"]).status.success());
    assert_eq!(records(&store).len(), 2);
    assert_eq!(
        contents(&store, record["root_hash"].as_str().unwrap()),
        before
    );
    assert!(!run(&["snapshot", "missing"]).status.success());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 8, failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/snapshot_tests.proptest-regressions"))), ..ProptestConfig::default() })]
    #[test]
    fn arbitrary_file_maps_capture_exact_bytes(files in prop::collection::btree_map("f-[a-z]{1,8}", prop::collection::vec(any::<u8>(), 0..2048), 0..6)) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        for (name, bytes) in &files { fs::write(source.join(name), bytes).unwrap(); }
        let snapshot = create_snapshot(&store, &source).unwrap();
        let actual = contents(&store, snapshot.root_hash());
        prop_assert_eq!(actual.len(), files.len() + 1);
        for (name, bytes) in files { prop_assert_eq!(&actual[Path::new(&name)], &Some(bytes)); }
    }
}
