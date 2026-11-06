#![cfg(any(target_os = "macos", target_os = "linux"))]

use rewind::{
    ChunkStore, hashing,
    snapshot::{SnapshotRecord, create_snapshot, list_snapshots},
    tree::{DirectoryNode, EntryTarget},
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::{
        fd::OwnedFd,
        unix::{fs::symlink, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn cli(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rewind"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}
fn success(cwd: &Path, args: &[&str]) -> String {
    let result = cli(cwd, args);
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}
fn state(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, relative: &Path, output: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        output.insert(relative.to_owned(), None);
        for entry in fs::read_dir(root.join(relative)).unwrap() {
            let entry = entry.unwrap();
            let path = relative.join(entry.file_name());
            if path == Path::new(".rewind") {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            assert!(!metadata.is_symlink());
            if metadata.is_dir() {
                walk(root, &path, output);
            } else {
                output.insert(path, Some(fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut output = BTreeMap::new();
    walk(root, Path::new(""), &mut output);
    output
}
fn directory_hashes(
    store: &ChunkStore,
    hash: &str,
    prefix: &str,
    output: &mut BTreeMap<String, String>,
) {
    output.insert(prefix.into(), hash.into());
    for entry in DirectoryNode::load(store, hash).unwrap().entries() {
        if let EntryTarget::Directory { tree } = entry.target() {
            let path = if prefix.is_empty() {
                entry.name().into()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            directory_hashes(store, tree, &path, output);
        }
    }
}

#[test]
fn complete_cli_workflow_and_merkle_ancestor_behavior_without_original_source() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    for name in [
        "src",
        "assets/nested",
        "empty",
        "dir-to-file/empty",
        "deleted-dir/empty",
    ] {
        fs::create_dir_all(project.join(name)).unwrap();
    }
    let mut deep = project.join("deep");
    for _ in 0..24 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("leaf"), b"deep data").unwrap();
    let binary: Vec<u8> = (0..300_000).map(|i| (i * 37 + i / 256) as u8).collect();
    for (name, bytes) in [
        ("src/main", b"before".as_slice()),
        ("delete", b"gone"),
        ("file-to-dir", b"file"),
        ("dir-to-file/child", b"child"),
        ("empty-file", b""),
    ] {
        fs::write(project.join(name), bytes).unwrap();
    }
    fs::write(project.join("assets/nested/binary"), &binary).unwrap();
    success(&project, &["init"]);
    let expected_a = state(&project);
    let a = success(&project, &["snapshot", "."]);
    let identical = success(&project, &["snapshot", "."]);
    let store = ChunkStore::open(project.join(".rewind")).unwrap();
    let record_a = SnapshotRecord::load(&store, &a).unwrap();
    assert_eq!(
        record_a.root_hash(),
        SnapshotRecord::load(&store, &identical)
            .unwrap()
            .root_hash()
    );
    let mut hashes_a = BTreeMap::new();
    directory_hashes(&store, record_a.root_hash(), "", &mut hashes_a);
    fs::write(project.join("src/main"), b"after").unwrap();
    fs::remove_file(project.join("delete")).unwrap();
    fs::remove_dir_all(project.join("deleted-dir")).unwrap();
    fs::remove_file(project.join("file-to-dir")).unwrap();
    fs::create_dir_all(project.join("file-to-dir/empty")).unwrap();
    fs::write(project.join("file-to-dir/new"), b"new").unwrap();
    fs::remove_dir_all(project.join("dir-to-file")).unwrap();
    fs::write(project.join("dir-to-file"), b"now a file").unwrap();
    fs::create_dir_all(project.join("added-dir/empty")).unwrap();
    fs::write(project.join("added"), b"added").unwrap();
    let expected_b = state(&project);
    let b = success(&project, &["snapshot", "."]);
    let mut hashes_b = BTreeMap::new();
    directory_hashes(
        &store,
        SnapshotRecord::load(&store, &b).unwrap().root_hash(),
        "",
        &mut hashes_b,
    );
    assert_ne!(hashes_a[""], hashes_b[""]);
    assert_ne!(hashes_a["src"], hashes_b["src"]);
    for name in ["assets", "assets/nested", "deep", "empty"] {
        assert_eq!(hashes_a[name], hashes_b[name]);
    }
    // The original absolute source path is now absent. Keep the store by moving
    // its containing project, then run diff and restore from that new location.
    let moved = temp.path().join("moved-project");
    fs::rename(&project, &moved).unwrap();
    assert!(!project.exists());
    assert!(success(&moved, &["diff", &a, &identical]).is_empty());
    let diff = success(&moved, &["diff", &a, &b]);
    assert_eq!(
        diff.lines().collect::<Vec<_>>(),
        [
            "A\tfile\t\"added\"",
            "A\tdirectory\t\"added-dir\"",
            "A\tdirectory\t\"added-dir/empty\"",
            "D\tfile\t\"delete\"",
            "D\tdirectory\t\"deleted-dir\"",
            "D\tdirectory\t\"deleted-dir/empty\"",
            "D\tdirectory\t\"dir-to-file\"",
            "A\tfile\t\"dir-to-file\"",
            "D\tfile\t\"dir-to-file/child\"",
            "D\tdirectory\t\"dir-to-file/empty\"",
            "D\tfile\t\"file-to-dir\"",
            "A\tdirectory\t\"file-to-dir\"",
            "A\tdirectory\t\"file-to-dir/empty\"",
            "A\tfile\t\"file-to-dir/new\"",
            "M\tfile\t\"src/main\"",
        ]
    );
    assert_eq!(success(&moved, &["snapshots"]).lines().count(), 4);
    success(&moved, &["restore-snapshot", &a, "../restored-a"]);
    success(&moved, &["restore-snapshot", &b, "../restored-b"]);
    assert_eq!(state(&temp.path().join("restored-a")), expected_a);
    assert_eq!(state(&temp.path().join("restored-b")), expected_b);
    assert_eq!(state(&moved), expected_b);
    assert!(!temp.path().join("restored-a/.rewind").exists());
    for id in ["../bad", "INVALID", &"0".repeat(64)] {
        assert!(!cli(&moved, &["diff", id, &a]).status.success());
        assert!(
            !cli(&moved, &["restore-snapshot", id, "../invalid-output"])
                .status
                .success()
        );
    }
    assert!(
        !cli(&moved, &["restore-snapshot", &a, "../restored-a"])
            .status
            .success()
    );
    assert_eq!(state(&temp.path().join("restored-a")), expected_a);
    let manifest = success(&moved, &["store", "assets/nested/binary"]);
    success(&moved, &["verify", &manifest]);
    success(&moved, &["restore", &manifest, "../single-file"]);
    assert_eq!(
        hashing::hash(&fs::read(temp.path().join("single-file")).unwrap()),
        hashing::hash(&binary)
    );
}

#[test]
fn stored_object_symlinks_and_nonregular_files_are_rejected_consistently() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let input = temp.path().join("input");
    fs::write(&input, b"bytes").unwrap();
    let manifest = rewind::store_file(&store, &input, Default::default()).unwrap();
    let manifest_path = manifest.save(&store).unwrap();
    let node = DirectoryNode::new(vec![]).unwrap();
    let hash = node.save(&store).unwrap();
    let tree_path = store.root().join("trees").join(&hash[..2]).join(&hash[2..]);
    let chunk = store.chunk_path(&manifest.chunks[0].hash).unwrap();
    for path in [&chunk, &manifest_path, &tree_path] {
        let original = fs::read(path).unwrap();
        let backing = temp.path().join("backing");
        fs::write(&backing, &original).unwrap();
        fs::remove_file(path).unwrap();
        symlink(&backing, path).unwrap();
        if path == &chunk {
            assert!(store.get(&manifest.chunks[0].hash).is_err());
        }
        if path == &manifest_path {
            assert!(rewind::FileManifest::load(path).is_err());
            assert!(manifest.save(&store).is_err());
        }
        if path == &tree_path {
            assert!(DirectoryNode::load(&store, &hash).is_err());
        }
        fs::remove_file(path).unwrap();
        fs::create_dir(path).unwrap();
        if path == &chunk {
            assert!(store.get(&manifest.chunks[0].hash).is_err());
        }
        if path == &manifest_path {
            assert!(rewind::FileManifest::load(path).is_err());
        }
        if path == &tree_path {
            assert!(DirectoryNode::load(&store, &hash).is_err());
        }
        fs::remove_dir(path).unwrap();
        fs::write(path, original).unwrap();
    }
}

#[test]
fn oversized_existing_manifest_is_rejected_without_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let input = temp.path().join("input");
    fs::write(&input, b"bytes").unwrap();
    let manifest = rewind::store_file(&store, input, Default::default()).unwrap();
    let path = manifest.save(&store).unwrap();
    File::create(&path)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    assert!(manifest.save(&store).is_err());
    assert_eq!(fs::metadata(path).unwrap().len(), 64 * 1024 * 1024 + 1);
}

#[test]
fn symlinked_history_publication_is_rejected_without_writing_outside_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    let outside = temp.path().join("outside");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, store.root().join("snapshots")).unwrap();
    assert!(create_snapshot(&store, source).is_err());
    assert!(list_snapshots(&store).is_err());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn closed_stdout_returns_failure_without_a_panic() {
    let temp = tempfile::tempdir().unwrap();
    ChunkStore::init(temp.path().join(".rewind")).unwrap();
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_rewind"))
        .current_dir(temp.path())
        .arg("snapshots")
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
}
