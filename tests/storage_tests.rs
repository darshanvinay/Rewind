use proptest::prelude::*;
use rewind::{
    ChunkStore, FileManifest, chunking::ChunkSizes, hashing, reconstruct, restore_file, store_file,
    verify,
};
use std::{
    fs,
    io::{self, Write},
};

fn round_trip(bytes: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(dir.path().join("store")).unwrap();
    let input = dir.path().join("input");
    fs::write(&input, bytes).unwrap();
    let manifest = store_file(&store, &input, ChunkSizes::default()).unwrap();
    let path = manifest.save(&store).unwrap();
    let loaded = FileManifest::load(path).unwrap();
    verify(&store, &loaded).unwrap();
    let output = dir.path().join("output");
    restore_file(&store, &loaded, &output).unwrap();
    let result = fs::read(&output).unwrap();
    assert_eq!(bytes, result);
    assert_eq!(hashing::hash(bytes), hashing::hash(&result));
    assert!(restore_file(&store, &loaded, &output).is_err());
    assert_eq!(fs::read(output).unwrap(), bytes);
}

#[test]
fn empty_small_repeated_and_large_binary() {
    round_trip(b"");
    round_trip(b"Rewind preserves exact bytes.\n");
    round_trip(&vec![42; 2_000_000]);
    let mut state = 0x0123456789abcdefu64;
    let data: Vec<u8> = (0..5_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    round_trip(&data);
}

#[test]
fn dedup_corruption_missing_and_stale_temps() {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(dir.path()).unwrap();
    let id = store.put(b"identical").unwrap();
    let path = store.chunk_path(&id).unwrap();
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(id, store.put(b"identical").unwrap());
    assert_eq!(before, fs::metadata(&path).unwrap().modified().unwrap());
    let mut interrupted = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    interrupted.write_all(b"partial uncommitted bytes").unwrap();
    let (_file, stale) = interrupted.keep().unwrap();
    let reopened = ChunkStore::open(dir.path()).unwrap();
    assert_eq!(reopened.get(&id).unwrap(), b"identical");
    assert!(stale.exists());
    fs::write(&path, b"corrupt").unwrap();
    assert!(store.get(&id).is_err());
    assert!(store.put(b"identical").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"corrupt");
    fs::remove_file(path).unwrap();
    assert!(store.get(&id).is_err());
    assert!(store.get("../../escape").is_err());
}

#[test]
fn invalid_manifests_and_atomic_failed_restore() {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(dir.path().join("store")).unwrap();
    let input = dir.path().join("input");
    fs::write(&input, b"some data").unwrap();
    let manifest = store_file(&store, &input, ChunkSizes::default()).unwrap();
    let mut bad = manifest.clone();
    bad.version = 2;
    assert!(bad.validate().is_err());
    bad = manifest.clone();
    bad.length += 1;
    assert!(bad.validate().is_err());
    bad = manifest.clone();
    bad.chunks[0].hash = "../invalid".into();
    assert!(bad.validate().is_err());
    bad = manifest.clone();
    bad.chunks[0].length = 0;
    assert!(bad.validate().is_err());
    let malformed = dir.path().join("bad.json");
    for bytes in [b"{".as_slice(), b"{}", b"null"] {
        fs::write(&malformed, bytes).unwrap();
        assert!(FileManifest::load(&malformed).is_err());
    }
    bad = manifest.clone();
    bad.file_hash = hashing::hash(b"wrong");
    let output = dir.path().join("out");
    assert!(restore_file(&store, &bad, &output).is_err());
    assert!(!output.exists());
    fs::remove_file(store.chunk_path(&manifest.chunks[0].hash).unwrap()).unwrap();
    assert!(verify(&store, &manifest).is_err());
    assert!(restore_file(&store, &manifest, &output).is_err());
    assert!(!output.exists());
}

#[test]
fn invalid_sizes_and_write_errors() {
    assert!(
        ChunkSizes {
            min: 0,
            avg: 1,
            max: 2
        }
        .validate()
        .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(dir.path().join("store")).unwrap();
    let input = dir.path().join("in");
    fs::write(&input, b"hello").unwrap();
    let manifest = store_file(&store, input, ChunkSizes::default()).unwrap();
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(reconstruct(&store, &manifest, Broken).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/storage_tests.proptest-regressions"))), ..ProptestConfig::default() })]
    #[test]
    fn arbitrary_bytes_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..300_000)) {
        round_trip(&bytes);
    }
}

#[test]
fn cli_smoke_real_file_blake3() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rewind"))
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap()
    };
    fs::write(
        dir.path().join("source.bin"),
        include_bytes!("../Cargo.lock"),
    )
    .unwrap();
    assert!(run(&["init"]).status.success());
    let stored = run(&["store", "source.bin"]);
    assert!(stored.status.success(), "{:?}", stored);
    let manifest = String::from_utf8(stored.stdout).unwrap();
    let manifest = manifest.trim();
    assert!(run(&["verify", manifest]).status.success());
    assert!(run(&["restore", manifest, "restored.bin"]).status.success());
    let original = fs::read(dir.path().join("source.bin")).unwrap();
    let restored = fs::read(dir.path().join("restored.bin")).unwrap();
    assert_eq!(original, restored);
    assert_eq!(hashing::hash(&original), hashing::hash(&restored));
    println!("CLI round-trip BLAKE3: {}", hashing::hash(&original));
    assert!(!run(&["restore", manifest, "restored.bin"]).status.success());
    assert!(!run(&["unknown"]).status.success());
}

#[test]
fn custom_sizes_and_manifest_order_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(dir.path().join("store")).unwrap();
    let input = dir.path().join("input");
    let bytes: Vec<u8> = (0..50_000).map(|i| (i * 37 + i / 256) as u8).collect();
    fs::write(&input, &bytes).unwrap();
    let sizes = ChunkSizes {
        min: 64,
        avg: 256,
        max: 1024,
    };
    let mut manifest = store_file(&store, input, sizes).unwrap();
    assert!(manifest.chunks.len() > 1);
    assert!(manifest.chunks.iter().all(|c| c.length <= 1024));
    manifest.chunks.reverse();
    assert!(verify(&store, &manifest).is_err());
    assert!(
        ChunkSizes {
            min: 65,
            avg: 256,
            max: 1024
        }
        .validate()
        .is_err()
    );
}
