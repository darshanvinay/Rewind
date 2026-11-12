#![cfg(unix)]
use rewind::{
    ChunkStore,
    shadow::{CacheLimits, DirtyPlan, DirtyScope, Invalidation, MerkleCache, ShadowResult},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

fn fixture() -> (tempfile::TempDir, ChunkStore, PathBuf, MerkleCache) {
    let temp = tempfile::tempdir().unwrap();
    let store = ChunkStore::init(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("a/deep/empty")).unwrap();
    fs::create_dir_all(source.join("b")).unwrap();
    fs::write(source.join("a/file"), b"initial-a").unwrap();
    fs::write(source.join("b/file"), b"initial-b").unwrap();
    let mut cache = MerkleCache::default();
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    (temp, store, source, cache)
}
fn plan(path: &str) -> DirtyPlan {
    let mut p = DirtyPlan::default();
    p.add(DirtyScope::File(path.into()));
    p
}
fn root(cache: &mut MerkleCache, plan: &DirtyPlan) -> String {
    match cache.candidate(plan, &AtomicBool::new(false)).unwrap() {
        ShadowResult::Candidate(c) => c.root_hash,
        other => panic!("{other:?}"),
    }
}
fn oracle(store: &ChunkStore, source: &Path) -> String {
    MerkleCache::full_scan_root(store, source, &AtomicBool::new(false)).unwrap()
}
fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(p: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if p.is_file() {
            out.insert(p.to_owned(), fs::read(p).unwrap());
        } else {
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

#[test]
fn changed_file_rebuilds_only_ancestors_and_matches_full_root_without_writing() {
    let (_temp, store, source, mut cache) = fixture();
    let before = inventory(store.root());
    let baseline = cache.baseline_root().unwrap().to_owned();
    fs::write(source.join("a/file"), b"changed-a").unwrap();
    let result = cache
        .candidate(&plan("a/file"), &AtomicBool::new(false))
        .unwrap();
    let ShadowResult::Candidate(c) = result else {
        panic!()
    };
    assert_eq!(c.root_hash, oracle(&store, &source));
    assert_ne!(c.root_hash, baseline);
    assert_eq!(c.stats.files_read, 1);
    assert_eq!(c.stats.directories_listed, 2);
    assert_eq!(c.stats.nodes_rebuilt, 2);
    assert!(c.stats.entries_reused >= 2);
    assert_eq!(cache.baseline_root(), Some(baseline.as_str()));
    assert_eq!(inventory(store.root()), before);
    assert!(!store.root().join("capture.pending").exists());
    assert!(!store.root().join("snapshots").exists());
}
#[test]
fn missed_event_produces_a_detectable_shadow_mismatch_not_false_authority() {
    let (_temp, store, source, mut cache) = fixture();
    fs::write(source.join("a/file"), b"silently-changed").unwrap();
    fs::write(source.join("b/file"), b"reported-change").unwrap();
    let candidate = root(&mut cache, &plan("b/file"));
    let full = oracle(&store, &source);
    assert_ne!(candidate, full);
    // C, not the stale candidate, publishes the current whole tree.
    let record =
        rewind::snapshot::capture_automatic(&store, &source, &AtomicBool::new(false)).unwrap();
    let rewind::snapshot::AutomaticCapture::Created(record) = record else {
        panic!()
    };
    assert_eq!(record.root_hash(), full);
}
#[test]
fn directory_renames_rescan_subtree_and_preserve_empty_directories() {
    let (_temp, store, source, mut cache) = fixture();
    fs::rename(source.join("a"), source.join("renamed")).unwrap();
    let mut p = DirtyPlan::default();
    p.add(DirtyScope::Subtree("a".into()));
    p.add(DirtyScope::Subtree("renamed".into()));
    assert_eq!(root(&mut cache, &p), oracle(&store, &source));
    assert!(source.join("renamed/deep/empty").is_dir());
}
#[test]
fn hard_link_aliases_in_separate_subtrees_are_invalidated_together() {
    let (_temp, store, source, mut cache) = fixture();
    fs::hard_link(source.join("a/file"), source.join("b/alias")).unwrap();
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    fs::write(source.join("a/file"), b"linked-change").unwrap();
    let ShadowResult::Candidate(c) = cache
        .candidate(&plan("a/file"), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(c.root_hash, oracle(&store, &source));
    assert_eq!(c.stats.files_read, 2);
}
#[test]
fn external_hard_link_alias_requires_reconciliation() {
    let (temp, store, source, mut cache) = fixture();
    fs::hard_link(source.join("a/file"), temp.path().join("external")).unwrap();
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    assert!(matches!(
        cache
            .candidate(&plan("a/file"), &AtomicBool::new(false))
            .unwrap(),
        ShadowResult::NeedsReconciliation {
            reason: Invalidation::Ambiguous
        }
    ));
}
#[test]
fn invalidation_budgets_identity_and_bad_paths_fail_closed() {
    let (_temp, store, source, mut cache) = fixture();
    let old_epoch = cache.epoch();
    cache.invalidate(Invalidation::EventLoss);
    assert_eq!(cache.entry_count(), 0);
    assert!(cache.epoch() > old_epoch);
    assert!(matches!(
        cache
            .candidate(&plan("a/file"), &AtomicBool::new(false))
            .unwrap(),
        ShadowResult::NeedsReconciliation { .. }
    ));
    let mut tiny = MerkleCache::new(CacheLimits {
        entries: 2,
        bytes: 1024,
    });
    tiny.diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(tiny.entry_count(), 0);
    assert_eq!(tiny.charged_bytes(), 0);
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    let mut bad = DirtyPlan::default();
    bad.add(DirtyScope::File("../escape".into()));
    assert!(matches!(
        cache.candidate(&bad, &AtomicBool::new(false)).unwrap(),
        ShadowResult::NeedsReconciliation { .. }
    ));
    assert_eq!(cache.entry_count(), 0);
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    fs::rename(&source, source.with_file_name("old-source")).unwrap();
    fs::create_dir(&source).unwrap();
    assert!(matches!(
        cache
            .candidate(&DirtyPlan::default(), &AtomicBool::new(false))
            .unwrap(),
        ShadowResult::NeedsReconciliation {
            reason: Invalidation::IdentityChanged
        }
    ));
    assert_eq!(cache.entry_count(), 0);
}
#[test]
fn atomic_save_deletion_and_type_replacements_match_oracle() {
    let (_temp, store, source, mut cache) = fixture();
    fs::write(source.join("a/.temporary"), b"atomic").unwrap();
    fs::rename(source.join("a/.temporary"), source.join("a/file")).unwrap();
    let mut p = plan("a/file");
    p.add(DirtyScope::File("a/.temporary".into()));
    assert_eq!(root(&mut cache, &p), oracle(&store, &source));
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    fs::remove_file(source.join("a/file")).unwrap();
    fs::create_dir(source.join("a/file")).unwrap();
    fs::write(source.join("a/file/nested"), [0, 255, 42]).unwrap();
    assert_eq!(root(&mut cache, &plan("a/file")), oracle(&store, &source));
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    fs::remove_dir_all(source.join("a/file")).unwrap();
    fs::write(source.join("a/file"), b"file-again").unwrap();
    assert_eq!(root(&mut cache, &plan("a/file")), oracle(&store, &source));
}
#[test]
fn symlinks_store_exclusion_and_cancellation_keep_candidates_read_only() {
    let (_temp, store, source, mut cache) = fixture();
    let before = inventory(store.root());
    std::os::unix::fs::symlink("missing", source.join("a/link")).unwrap();
    assert!(
        cache
            .candidate(&plan("a/link"), &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(inventory(store.root()), before);
    fs::remove_file(source.join("a/link")).unwrap();
    assert!(matches!(
        cache.candidate(&plan("a/file"), &AtomicBool::new(true)),
        Err(rewind::Error::Cancelled)
    ));
    let inside = ChunkStore::init(source.join("vault")).unwrap();
    cache
        .diagnostic_baseline(&inside, &source, &AtomicBool::new(false))
        .unwrap();
    inside.put(b"excluded").unwrap();
    assert_eq!(
        root(&mut cache, &DirtyPlan::default()),
        oracle(&inside, &source)
    );
}
#[test]
fn deterministic_randomized_operations_match_authoritative_roots() {
    let (_temp, store, source, mut cache) = fixture();
    let mut rng = 0x1234_5678_u64;
    for step in 0..80 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let index = (rng >> 32) % 12;
        let relative = format!(
            "{}/r-{index}",
            if index.is_multiple_of(2) { "a" } else { "b" }
        );
        let path = source.join(&relative);
        let mut p = plan(&relative);
        match rng % 5 {
            0 => {
                if path.is_dir() {
                    fs::remove_dir_all(&path).unwrap();
                }
                fs::write(&path, format!("random-{step}-{rng}")).unwrap();
            }
            1 => {
                if path.is_dir() {
                    fs::remove_dir_all(&path).unwrap();
                } else if path.exists() {
                    fs::remove_file(&path).unwrap();
                }
            }
            2 => {
                if path.is_file() {
                    fs::remove_file(&path).unwrap();
                }
                fs::create_dir_all(path.join("empty")).unwrap();
                p.add(DirtyScope::Subtree(relative.clone().into()));
            }
            3 => {
                if path.is_file() {
                    let target = format!("b/moved-{index}");
                    fs::rename(&path, source.join(&target)).unwrap();
                    p.add(DirtyScope::File(target.into()));
                }
            }
            _ => {
                if path.is_file() {
                    let target = format!("b/hard-{index}");
                    let target_path = source.join(&target);
                    if target_path.exists() {
                        fs::remove_file(&target_path).unwrap();
                    }
                    fs::hard_link(&path, target_path).unwrap();
                    p.add(DirtyScope::File(target.into()));
                }
            }
        }
        match cache.candidate(&p, &AtomicBool::new(false)).unwrap() {
            ShadowResult::Candidate(c) => {
                assert_eq!(c.root_hash, oracle(&store, &source), "step {step}")
            }
            ShadowResult::NeedsReconciliation { .. } => {}
        }
        cache
            .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
            .unwrap();
    }
}

#[test]
fn excessive_dirty_scopes_promote_to_full_reconciliation() {
    let (_temp, _store, _source, mut cache) = fixture();
    let mut p = DirtyPlan::default();
    for i in 0..4097 {
        p.add(DirtyScope::File(format!("file-{i}").into()));
    }
    assert!(matches!(
        cache.candidate(&p, &AtomicBool::new(false)).unwrap(),
        ShadowResult::NeedsReconciliation {
            reason: Invalidation::Budget
        }
    ));
    assert_eq!(cache.entry_count(), 0);
    let mut bounded = MerkleCache::new(CacheLimits {
        entries: usize::MAX,
        bytes: usize::MAX,
    });
    bounded.invalidate(Invalidation::Explicit);
    assert_eq!(bounded.charged_bytes(), 0);
}

#[test]
fn preserved_mtime_and_length_do_not_prove_dirty_file_contents_unchanged() {
    let (_temp, store, source, mut cache) = fixture();
    let file = source.join("a/file");
    let time = fs::metadata(&file).unwrap().modified().unwrap();
    let old = cache.baseline_root().unwrap().to_owned();
    fs::write(&file, b"different").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
    let candidate = root(&mut cache, &plan("a/file"));
    assert_ne!(candidate, old);
    assert_eq!(candidate, oracle(&store, &source));
}

#[test]
fn candidate_manifest_work_budget_fails_without_publishing() {
    let (_temp, store, source, _cache) = fixture();
    fs::write(source.join("a/file"), vec![0; 4 * 1024 * 1024]).unwrap();
    let mut cache = MerkleCache::new(CacheLimits {
        entries: 100,
        bytes: 4096,
    });
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    // Use a source with just one file so the metadata seed fits this small budget.
    fs::remove_dir_all(source.join("b")).unwrap();
    fs::remove_dir_all(source.join("a/deep")).unwrap();
    cache
        .diagnostic_baseline(&store, &source, &AtomicBool::new(false))
        .unwrap();
    assert!(cache.entry_count() > 0);
    assert!(
        cache
            .candidate(&plan("a/file"), &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(cache.entry_count(), 0);
    assert!(!store.root().join("snapshots").exists());
    assert!(!store.root().join("capture.pending").exists());
}
