# Verified existing-object reuse

The approved fast path is implemented for immutable chunks, file manifests and
directory Merkle nodes. Snapshot records remain unique new durable writes.
No format, public API, synchronization policy, watcher or Phase 3 feature changed.

## Integrity and control flow

`storage::publish_if_missing` centralizes the policy using two closures (delayed
operations). The verifier can run twice (`FnMut`): once up front, and again if a
publisher loses a no-clobber race. The publisher runs at most once (`FnOnce`).

1. Verify the expected object using the same bounded validation as before.
2. On success, reuse it without creating/writing/syncing a temporary.
3. On **I/O NotFound only**, use the existing durable publication path: create a
   same-filesystem unique temporary, write all bytes, flush, sync the file,
   publish without replacement, and sync the parent directory as before.
4. Propagate every other error. Corrupt/truncated data, invalid types, symlinks,
   oversized objects and permissions failures do not become misses or repairs.
5. If publication loses a race to an existing object, verify that winner before
   returning success. A corrupt winner is preserved and reported as an error.
   A disappearing winner is an error rather than a false success or infinite retry.

There is no `exists()`-only shortcut or remembered verification cache:

- Chunks use `ChunkStore::get`, checking the actual stored bytes against the
  expected BLAKE3 identity, including nonempty/size/type constraints.
- Manifests compare the bounded existing bytes with the newly validated and hashed
  bytes being saved. Exact equality also establishes the expected content identity
  and schema; merely parsing JSON is insufficient.
- Nodes use `DirectoryNode::load`, retaining BLAKE3, canonical encoding and schema
  validation. Standalone node save still does not traverse descendants; capture
  builds/checks the graph bottom-up just as before.

All source files are still read, chunked and hashed. Existing chunk data is re-read
and hashed on every reuse. Single-writer/trusted-store and no-concurrent-GC limits
remain. Reuse assumes previously committed objects; it is not a new durability
repair protocol after an earlier sync error. Directory batching, worker pools and
packed storage are not implemented.

## Regression and validation results

- `cargo fmt --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo test`: **57 passed, 0 failed, 0 ignored** on macOS (nine unit tests and
  48 integration tests). The previous 49 tests remain passing; five policy/race
  unit tests and three real-filesystem integration tests were added.
- `cargo build --release --locked`: passed.
- Before/after/profile captures and A/B restores matched in every comparison.

New tests prove verified hits never invoke publication; only NotFound invokes
it; corruption/invalid-object/permission errors never invoke it; publication
errors propagate; an actual race winner is verified and never overwritten; and a
lost winner does not trigger an unbounded retry. Real objects cover same-length
chunk corruption, truncation, zero bytes, appended bytes, corrupt manifests/nodes,
missing objects, dangling symlinks and directories. Corrupt bytes are left intact.
The existing suite retains restore, publication-failure, malformed metadata,
source/store-exclusion, special-file, property and complete CLI workflow checks.

## Paired before/after measurements

Same Apple M4 / 16 GiB / macOS 15.7.4 APFS host and Rust 1.99.1 release builds as
[the initial investigation](capture-performance.md). Each dataset contains exactly
32 MiB, either one file or 256 distinct 128 KiB files, generated from the same
SHAKE256 seed as that investigation. Three independent fresh stores per executable
and layout run fresh, unchanged and 4 KiB-edit captures. Execution order rotates
across the saved pre-change executable, the optimized uninstrumented executable,
and a disposable instrumented optimized copy. No builds/tests ran during timing.

Baseline/optimized timings below are **uninstrumented median elapsed seconds**;
ranges are min–max over three trials. Startup/native-time launcher overhead is
included. Warm/uncontrolled caches and system load limit generalization; notably
one optimized many-file unchanged trial took 0.223 s. These are local samples, not
a universal speedup claim. Fresh captures retain the same sync counts and add
lookup work, so they are not the target of this optimization.

| Layout | Phase | Before median (range), s | After median (range), s |
| --- | --- | ---: | ---: |
| 1 × 32 MiB | fresh | 4.6595 (4.5192–4.6757) | 4.5456 (4.5423–4.6091) |
| 1 × 32 MiB | unchanged | 2.0242 (2.0218–2.1515) | 0.0942 (0.0930–0.0952) |
| 1 × 32 MiB | modified | 2.1048 (2.0467–2.1226) | 0.1000 (0.0993–0.1002) |
| 256 × 128 KiB | fresh | 8.1733 (7.8138–8.2694) | 8.2823 (7.8372–8.3341) |
| 256 × 128 KiB | unchanged | 4.0880 (3.8640–4.2395) | 0.0964 (0.0951–0.2228) |
| 256 × 128 KiB | modified | 3.9981 (3.8825–4.2124) | 0.1098 (0.1072–0.1120) |

### Operation counts from the instrumented optimized copy

Every listed count was identical across the three trials. These are Rust data-write
and `sync_all` calls, not physical disk writes or a kernel syscall trace. New and
existing metadata retain their type/schema checks. Directory sync policy for new
objects is unchanged.

| Layout | Phase | Chunks generated | Verified existing chunks | Chunk data writes | All data writes / file syncs | Directory syncs |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 files | fresh | 415 | 0 | 415 | 418 / 418 | 844 |
| 1 files | unchanged | 415 | 415 | 0 | 1 / 1 | 1 |
| 1 files | modified | 415 | 414 | 1 | 4 / 4 | 6 |
| 256 files | fresh | 567 | 0 | 567 | 825 / 825 | 1293 |
| 256 files | unchanged | 567 | 567 | 0 | 1 / 1 | 1 |
| 256 files | modified | 567 | 566 | 1 | 4 / 4 | 6 |

An unchanged capture now writes/syncs only its new history record. The 4 KiB edit
in these datasets writes one changed chunk, one changed manifest, one root node
and one record. Fresh counts are unchanged from the initial profile: 418 or 825
file syncs, and 844 or 1293 directory syncs. The fresh directory-sync bottleneck
remains and needs its own approved design.

All profiled captures still read 33,554,432 source bytes. Unchanged captures record
67,108,864 BLAKE3 chunk-hashing bytes: hashing source chunks plus hashing the stored
chunks again. Whole-file BLAKE3 hashing is additional. Thus the improvement comes
from removing redundant temporary writes/syncs, not from dropping verification or
skipping source reads. A same-length corrupt chunk is explicitly covered by tests.

For each trial, saved baseline, optimized and instrumented roots/chunk growth were
identical. After capture, the source was renamed so its recorded path was absent;
the diff and both complete historical restore maps still matched. The raw report
contains all stage timers, native CPU/RSS metrics, hashes, ranges, execution orders
and per-operation counters: [capture-profile-verified-reuse.json](capture-profile-verified-reuse.json).
A hashing-byte and write/sync-count audit was also performed against the raw data.

## Reproduction

The pre-change reference was saved **before** rebuilding the optimized checkout.
Its SHA-256 matches the original investigation's baseline digest. Example command
used in this session:

```sh
python3 scripts/profile_capture.py --reference-binary /tmp/rewind-verified-reuse-before/rewind --trials 3 --mib 32 --many-files 256 --output docs/capture-profile-verified-reuse.json
```

That `/tmp` reference is ephemeral; retain a separate executable when performing
future before/after comparisons. Without `--reference-binary`, the harness profiles
the current checkout against its instrumented copy. It does not reconstruct a past
version from the raw measurements. The initial pre-optimization measurements are
retained separately and clearly labeled as historical.

The profiler adds counters/timers only to disposable copied sources. Instrumented
elapsed times are not substituted for the uninstrumented after column above. Its
per-thread exclusive timing model applies to the current serial engine; future
worker pools would require a different aggregation model. No synchronization,
verification, public persistence guarantee or Phase 3 behavior was relaxed here.
