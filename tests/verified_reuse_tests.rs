use rewind::{ChunkStore, Error, store_file, tree::DirectoryNode};
use std::fs;

#[test]
fn duplicate_chunks_reject_same_length_corruption_truncation_empty_and_extra_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let original = b"original chunk";
    let id = store.put(original).unwrap();
    let path = store.chunk_path(&id).unwrap();
    for broken in [
        b"corrupted data".as_slice(),
        b"orig",
        b"",
        b"original chunk plus bytes",
    ] {
        fs::write(&path, broken).unwrap();
        assert!(matches!(store.put(original), Err(Error::Corrupt(_))));
        assert_eq!(fs::read(&path).unwrap(), broken);
    }
    // A genuinely absent chunk still takes the durable publication path.
    fs::remove_file(&path).unwrap();
    assert_eq!(store.put(original).unwrap(), id);
    assert_eq!(store.get(&id).unwrap(), original);
}

#[test]
fn duplicate_manifests_and_nodes_are_verified_not_repaired() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("input");
    fs::write(&source, b"bytes").unwrap();
    let manifest = store_file(&store, source, Default::default()).unwrap();
    let manifest_path = manifest.save(&store).unwrap();
    assert_eq!(manifest.save(&store).unwrap(), manifest_path);
    let node = DirectoryNode::new(vec![]).unwrap();
    let hash = node.save(&store).unwrap();
    let node_path = store.root().join("trees").join(&hash[..2]).join(&hash[2..]);
    assert_eq!(node.save(&store).unwrap(), hash);
    for broken in [b"".as_slice(), b"{}", b"corrupted metadata"] {
        fs::write(&manifest_path, broken).unwrap();
        assert!(matches!(manifest.save(&store), Err(Error::Corrupt(_))));
        assert_eq!(fs::read(&manifest_path).unwrap(), broken);
        fs::write(&node_path, broken).unwrap();
        assert!(matches!(node.save(&store), Err(Error::Corrupt(_))));
        assert_eq!(fs::read(&node_path).unwrap(), broken);
    }
    fs::remove_file(&manifest_path).unwrap();
    fs::remove_file(&node_path).unwrap();
    assert_eq!(manifest.save(&store).unwrap(), manifest_path);
    assert_eq!(node.save(&store).unwrap(), hash);
}

#[test]
#[cfg(unix)]
fn dangling_symlinks_and_directories_are_errors_not_missing_objects() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("input");
    fs::write(&source, b"bytes").unwrap();
    let manifest = store_file(&store, source, Default::default()).unwrap();
    let manifest_path = manifest.save(&store).unwrap();
    let node = DirectoryNode::new(vec![]).unwrap();
    let hash = node.save(&store).unwrap();
    let paths = [
        store.chunk_path(&manifest.chunks[0].hash).unwrap(),
        manifest_path,
        store.root().join("trees").join(&hash[..2]).join(&hash[2..]),
    ];
    let absent_target = temp.path().join("absent");
    for (index, path) in paths.iter().enumerate() {
        fs::remove_file(path).unwrap();
        symlink(&absent_target, path).unwrap();
        let save = || match index {
            0 => store.put(b"bytes").map(|_| ()),
            1 => manifest.save(&store).map(|_| ()),
            _ => node.save(&store).map(|_| ()),
        };
        assert!(save().is_err());
        assert_eq!(fs::read_link(path).unwrap(), absent_target);
        assert!(!absent_target.exists());
        fs::remove_file(path).unwrap();
        fs::create_dir(path).unwrap();
        assert!(save().is_err());
        assert!(path.is_dir());
    }
}
