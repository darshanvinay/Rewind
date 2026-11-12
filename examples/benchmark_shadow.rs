//! Read-only diagnostic timings, not automatic-publication latency.
#[cfg(unix)]
fn main() -> rewind::Result<()> {
    use rewind::{
        ChunkStore,
        shadow::{DirtyPlan, DirtyScope, MerkleCache, ShadowResult},
    };
    use std::{fs, path::Path, sync::atomic::AtomicBool, time::Instant};
    fn candidate(
        cache: &mut MerkleCache,
        p: &DirtyPlan,
        stop: &AtomicBool,
    ) -> rewind::Result<(String, serde_json::Value)> {
        let start = Instant::now();
        let r = cache.candidate(p, stop)?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        match r {
            ShadowResult::Candidate(c) => Ok((
                c.root_hash,
                serde_json::json!({"candidate_ms":elapsed,"stats":c.stats}),
            )),
            _ => Err(rewind::Error::Invalid(format!(
                "no shadow candidate: {r:?}"
            ))),
        }
    }
    fn measure(
        cache: &mut MerkleCache,
        p: &DirtyPlan,
        store: &ChunkStore,
        source: &Path,
        stop: &AtomicBool,
    ) -> rewind::Result<serde_json::Value> {
        let (root, mut v) = candidate(cache, p, stop)?;
        let start = Instant::now();
        let full = MerkleCache::full_scan_root(store, source, stop)?;
        v["full_root_scan_ms"] = (start.elapsed().as_secs_f64() * 1000.0).into();
        assert_eq!(root, full);
        v["root_matches"] = true.into();
        Ok(v)
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let sizes: Vec<usize> = if args.is_empty() {
        vec![1000, 10000, 100000]
    } else {
        args.iter()
            .map(|s| s.parse().expect("integer file count"))
            .collect()
    };
    let mut cases = vec![];
    for count in sizes {
        if !(1..=100000).contains(&count) {
            return Err(rewind::Error::Invalid(
                "file count must be 1..100000".into(),
            ));
        }
        let temp = tempfile::tempdir()?;
        let store = ChunkStore::init(temp.path().join("store"))?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        for i in 0..count {
            let group = source.join(format!("group-{:04}", i / 100));
            if i.is_multiple_of(100) {
                fs::create_dir(&group)?;
            }
            let mut bytes = [0_u8; 4096];
            blake3::Hasher::new()
                .update(&(i as u64).to_le_bytes())
                .finalize_xof()
                .fill(&mut bytes);
            fs::write(group.join(format!("file-{:03}.bin", i % 100)), bytes)?;
        }
        let stop = AtomicBool::new(false);
        let mut cache = MerkleCache::default();
        let start = Instant::now();
        cache.diagnostic_baseline(&store, &source, &stop)?;
        let warmup = start.elapsed().as_secs_f64() * 1000.0;
        if cache.entry_count() == 0 {
            return Err(rewind::Error::Invalid(
                "cache budget prevented benchmark warmup".into(),
            ));
        }
        let relative = format!(
            "group-{:04}/file-{:03}.bin",
            (count / 2) / 100,
            (count / 2) % 100
        );
        let mut plan = DirtyPlan::default();
        plan.add(DirtyScope::File(relative.clone().into()));
        let mut edits = vec![];
        for trial in 0..5 {
            let mut bytes = [0_u8; 4096];
            bytes.fill((trial + 1) as u8);
            fs::write(source.join(&relative), bytes)?;
            let mut v = measure(&mut cache, &plan, &store, &source, &stop)?;
            v["trial"] = trial.into();
            edits.push(v);
            cache.diagnostic_baseline(&store, &source, &stop)?;
        }
        let mut renames = vec![];
        for trial in 0_usize..3 {
            let (from, to) = if trial.is_multiple_of(2) {
                ("group-0000", "moved-group")
            } else {
                ("moved-group", "group-0000")
            };
            fs::rename(source.join(from), source.join(to))?;
            let mut plan = DirtyPlan::default();
            plan.add(DirtyScope::Subtree(from.into()));
            plan.add(DirtyScope::Subtree(to.into()));
            renames.push(measure(&mut cache, &plan, &store, &source, &stop)?);
            cache.diagnostic_baseline(&store, &source, &stop)?;
        }
        let mut plan = DirtyPlan::default();
        for i in 0..count.min(100) {
            let name = format!("group-{:04}/file-{:03}.bin", i / 100, i % 100);
            let name = if source.join("group-0000").exists() {
                name
            } else {
                name.replace("group-0000", "moved-group")
            };
            fs::write(source.join(&name), vec![33; 4096])?;
            plan.add(DirtyScope::File(name.into()));
        }
        let burst = measure(&mut cache, &plan, &store, &source, &stop)?;
        assert!(!store.root().join("snapshots").exists());
        assert!(!store.root().join("capture.pending").exists());
        assert!(fs::read_dir(store.root().join("chunks"))?.next().is_none());
        cases.push(serde_json::json!({"files":count,"source_bytes":count*4096,"warmup_ms":warmup,"cache_entries":cache.entry_count(),"cache_charged_bytes":cache.charged_bytes(),"edit_samples":edits,"directory_rename_samples":renames,"burst_100":burst,"snapshots_created":0}));
        eprintln!("PASS {count} files: edit/rename/burst roots match; zero object writes");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"method":"Warm local 4 KiB unique files, fanout 100; 5 edits, 3 directory renames, 100-file burst; diagnostic full root scan is one read-only pass, NOT C's two-pass/archive/durability validation","cases":cases})
        )?
    );
    Ok(())
}
#[cfg(not(unix))]
fn main() {
    eprintln!("Unix diagnostic only");
}
