use proptest::prelude::*;
use rewind::{
    ChunkStore, hashing,
    tree::{DirectoryEntry, DirectoryNode, EntryTarget},
};
use std::fs;

fn file(name: &str, bytes: &[u8]) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryTarget::File {
            manifest: hashing::hash(bytes),
            length: bytes.len() as u64,
        },
    )
    .unwrap()
}
fn directory(name: &str, hash: String) -> DirectoryEntry {
    DirectoryEntry::new(name, EntryTarget::Directory { tree: hash }).unwrap()
}
fn path(store: &ChunkStore, hash: &str) -> std::path::PathBuf {
    store.root().join("trees").join(&hash[..2]).join(&hash[2..])
}

#[test]
fn canonical_format_and_order_are_stable() {
    let empty = DirectoryNode::new(vec![]).unwrap();
    assert_eq!(
        empty.canonical_bytes().unwrap(),
        br#"{"version":1,"object":"rewind-directory","entries":[]}"#
    );
    let a = file("a", b"first");
    let z = directory("z", empty.hash().unwrap());
    let first = DirectoryNode::new(vec![z.clone(), a.clone()]).unwrap();
    let second = DirectoryNode::new(vec![a, z]).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.hash().unwrap(), second.hash().unwrap());
    assert_eq!(first.entries()[0].name(), "a");
    let expected = format!(
        "{{\"version\":1,\"object\":\"rewind-directory\",\"entries\":[{{\"name\":\"a\",\"target\":{{\"kind\":\"file\",\"manifest\":\"{}\",\"length\":5}}}},{{\"name\":\"z\",\"target\":{{\"kind\":\"directory\",\"tree\":\"{}\"}}}}]}}",
        hashing::hash(b"first"),
        empty.hash().unwrap()
    );
    assert_eq!(first.canonical_bytes().unwrap(), expected.as_bytes());
    assert_eq!(
        DirectoryNode::from_bytes(expected.as_bytes()).unwrap(),
        first
    );
}

#[test]
fn merkle_change_propagates_and_unchanged_subtrees_reuse_objects() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path()).unwrap();
    let stable = DirectoryNode::new(vec![file("stable", b"same")]).unwrap();
    let stable_hash = stable.save(&store).unwrap();
    let stable_path = path(&store, &stable_hash);
    let modified = fs::metadata(&stable_path).unwrap().modified().unwrap();
    let old = DirectoryNode::new(vec![file("edited", b"old")]).unwrap();
    let new = DirectoryNode::new(vec![file("edited", b"new")]).unwrap();
    let old_hash = old.save(&store).unwrap();
    let new_hash = new.save(&store).unwrap();
    assert_ne!(old_hash, new_hash);
    let root_a = DirectoryNode::new(vec![
        directory("changed", old_hash),
        directory("stable", stable_hash.clone()),
    ])
    .unwrap();
    let root_b = DirectoryNode::new(vec![
        directory("changed", new_hash),
        directory("stable", stable_hash.clone()),
    ])
    .unwrap();
    assert_ne!(root_a.hash().unwrap(), root_b.hash().unwrap());
    let a = root_a.save(&store).unwrap();
    let b = root_b.save(&store).unwrap();
    assert_eq!(stable.save(&store).unwrap(), stable_hash);
    assert_eq!(
        fs::metadata(stable_path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(DirectoryNode::load(&store, &a).unwrap(), root_a);
    assert_eq!(DirectoryNode::load(&store, &b).unwrap(), root_b);
}

#[test]
fn rejects_unsafe_names_duplicates_and_invalid_hashes() {
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "/absolute",
        "C:drive",
        "x\0y",
        "x\ny",
        "NUL",
        "con.txt",
        "LPT1",
        "COM¹",
        "trailing.",
        "trailing ",
        "wild*",
        "a:b",
    ] {
        assert!(
            DirectoryEntry::new(
                name,
                EntryTarget::Directory {
                    tree: hashing::hash(b"")
                }
            )
            .is_err(),
            "{name:?}"
        );
    }
    assert!(
        DirectoryEntry::new(
            "x".repeat(256),
            EntryTarget::Directory {
                tree: hashing::hash(b"")
            }
        )
        .is_err()
    );
    assert!(DirectoryNode::new(vec![file("duplicate", b"a"), file("duplicate", b"b")]).is_err());
    assert!(
        DirectoryEntry::new(
            "ok",
            EntryTarget::Directory {
                tree: "../escape".into()
            }
        )
        .is_err()
    );
    assert!(
        DirectoryEntry::new(
            "ok",
            EntryTarget::File {
                manifest: "A".repeat(64),
                length: 0
            }
        )
        .is_err()
    );
    assert!(
        DirectoryEntry::new(
            "日本語.txt",
            EntryTarget::Directory {
                tree: hashing::hash(b"")
            }
        )
        .is_ok()
    );
}

#[test]
fn malformed_and_noncanonical_metadata_is_rejected() {
    let empty = DirectoryNode::new(vec![])
        .unwrap()
        .canonical_bytes()
        .unwrap();
    for bytes in [
        b"{".as_slice(),
        b"{}",
        b"null",
        b"[]",
        b"{\"version\":1,\"version\":1,\"object\":\"rewind-directory\",\"entries\":[]}",
    ] {
        assert!(DirectoryNode::from_bytes(bytes).is_err());
    }
    let text = String::from_utf8(empty.clone()).unwrap();
    for text in [
        format!("{text}\n"),
        text.replace("\"version\":1", "\"version\":2"),
        text.replace("rewind-directory", "other"),
        text.replace("\"entries\":[]", "\"entries\":[],\"extra\":0"),
    ] {
        assert!(DirectoryNode::from_bytes(text.as_bytes()).is_err());
    }
    let node = DirectoryNode::new(vec![file("a", b"a"), file("b", b"b")]).unwrap();
    let mut value = serde_json::to_value(&node).unwrap();
    value["entries"].as_array_mut().unwrap().reverse();
    assert!(DirectoryNode::from_bytes(&serde_json::to_vec(&value).unwrap()).is_err());
    value["entries"][0]["name"] = "../escape".into();
    assert!(DirectoryNode::from_bytes(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(DirectoryNode::from_bytes(&vec![b' '; 16 * 1024 * 1024 + 1]).is_err());
}

#[test]
fn stored_nodes_detect_corruption_missing_and_wrong_schema() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path()).unwrap();
    let node = DirectoryNode::new(vec![]).unwrap();
    let hash = node.save(&store).unwrap();
    assert_eq!(DirectoryNode::load(&store, &hash).unwrap(), node);
    fs::write(path(&store, &hash), b"corrupt").unwrap();
    assert!(DirectoryNode::load(&store, &hash).is_err());
    assert!(node.save(&store).is_err());
    assert_eq!(fs::read(path(&store, &hash)).unwrap(), b"corrupt");
    fs::remove_file(path(&store, &hash)).unwrap();
    assert!(DirectoryNode::load(&store, &hash).is_err());
    assert!(DirectoryNode::load(&store, "../../escape").is_err());
    let bad_bytes = b"{}";
    let bad_hash = hashing::hash(bad_bytes);
    fs::create_dir_all(path(&store, &bad_hash).parent().unwrap()).unwrap();
    fs::write(path(&store, &bad_hash), bad_bytes).unwrap();
    assert!(DirectoryNode::load(&store, &bad_hash).is_err());
}

#[test]
fn connects_to_existing_file_manifests() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let input = temp.path().join("binary");
    fs::write(&input, [0, 255, 128, 0]).unwrap();
    let manifest = rewind::store_file(&store, input, Default::default()).unwrap();
    let manifest_path = manifest.save(&store).unwrap();
    let manifest_hash = hashing::hash(&fs::read(manifest_path).unwrap());
    let entry = DirectoryEntry::new(
        "binary",
        EntryTarget::File {
            manifest: manifest_hash,
            length: manifest.length,
        },
    )
    .unwrap();
    let node = DirectoryNode::new(vec![entry]).unwrap();
    let hash = node.save(&store).unwrap();
    assert_eq!(DirectoryNode::load(&store, &hash).unwrap(), node);
    rewind::verify(&store, &manifest).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/tree_tests.proptest-regressions"))), ..ProptestConfig::default() })]
    #[test]
    fn permutation_does_not_change_hash(names in prop::collection::btree_set("[a-z]{1,12}", 0..40)) {
        let entries: Vec<_> = names.iter().map(|name| file(name, name.as_bytes())).collect();
        let first = DirectoryNode::new(entries.clone()).unwrap();
        let second = DirectoryNode::new(entries.into_iter().rev().collect()).unwrap();
        prop_assert_eq!(first.hash().unwrap(), second.hash().unwrap());
        let bytes = first.canonical_bytes().unwrap();
        prop_assert_eq!(DirectoryNode::from_bytes(&bytes).unwrap(), first);
    }
}
