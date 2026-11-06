# Phase 2 capture performance investigation

**Historical pre-optimization baseline.** The approved verified-existing-object
fast path has since been implemented; see [verified reuse results](verified-reuse.md).
The measurements and code behavior below describe the version before that change.

Profiling completed 2026-11-06 on the same Apple M4 MacBook Air, 16 GiB RAM,
macOS 15.7.4/APFS host. Rust/Cargo 1.99.1 release builds, default chunk sizes
16/64/256 KiB. **No repository engine source was modified and no optimization was
implemented.** Engine source fingerprints were checked before/after; the raw
report includes every `src/*.rs` SHA-256 and both executable digests.

## Method and reproducibility

```sh
# From the repository, with stable Rust/Cargo and Python 3.11+ available:
python3 scripts/profile_capture.py --trials 3 --mib 32 --many-files 256 --output docs/capture-profile.json
```

[Driver](../scripts/profile_capture.py), [copy/instrumentation builder](../scripts/capture_profile/instrument.py),
[timer module](../scripts/capture_profile/profile.rs), [raw results](capture-profile.json).
The driver copies Cargo files, source and tests into a new temporary directory,
adds timing instrumentation only there, and builds a separate release executable.
It never patches the engine checkout. Both unmodified and instrumented binaries
run each scenario three times on independent fresh stores. Baseline/profile order
alternates by trial. Builds/tests finish before timed scenarios begin.

Datasets contain exactly 33,554,432 bytes from
`SHAKE256("rewind-capture-profile-v1")`, either one 32 MiB file or 256 distinct
128 KiB files in a single source directory. This differs from the earlier Phase 2
harness, which adds 16 small files and an empty file/directory structure. Every
trial captures fresh A, repeats it unchanged, then captures B after XOR-replacing
4096 bytes in the middle file at file-midpoint +137 (without changing length).
Caches are warm/uncontrolled: generation and SHA-256 validation precede capture.
No cache drop or physical-disk I/O accounting was performed.

Every baseline/profile pair produced identical roots and chunk-store growth for
each phase. Each run then renamed its source so the recorded path was absent,
verified the diff, restored A/B, compared SHA-256/length/directory maps, and checked
source preservation. An initial disposable instrumented copy also passed all 49
existing tests (zero failures/ignored tests) before measurements. A small timing
sanity check proved that the sum of exclusive scopes equals the capture duration;
that equality and zero unclosed scopes were checked for every profiled capture.

Xcode's `xctrace` was not installed. Native `/usr/bin/time -l` supplied CPU/RSS;
its detailed counters required execution outside the restricted Codex sandbox.
Source-level `Instant` timers supplied operation wall time and counts. **Counts
are Rust I/O/`sync_all`/publication API calls, not an OS-wide syscall trace or
physical disk writes.** Data-write counters exclude implicit filesystem metadata
writes. The source code was checked against the installed tempfile dependency:
`persist_noclobber` first tries native no-replace rename on macOS; it has a
hard-link/unlink fallback. No assertion about which kernel fallback ran is made.

Rust scope guards record time on `Drop` when a function/block returns, including
error returns. Child durations are subtracted from their parents for **exclusive**
time. For example, FastCDC `next()` reads source bytes internally; subtracting the
read guard prevents counting that time once as reading and again as chunking.
Timers are per-thread for the current serial capture path; a future worker pool
would need thread-aware aggregation rather than this additive wall-time model.
Timers necessarily add overhead. Source reads measure `Read::read`; FastCDC time
covers `next()` boundary detection/copying excluding those reads. Constructor and
other ingestion bookkeeping fall into residual work. BLAKE3 covers whole-file,
chunk, manifest and tree digests/hex encoding; the tiny snapshot-ID digest remains
inside snapshot publication rather than the hashing row.

## End-to-end control measurements

All values are seconds. Baseline wall time includes native timing launcher/process
startup. CPU values are native user/system medians (0.01-second print precision).
Profile outer wall time also includes exporting the timing report after capture.
The capture-only stage budgets below exclude startup/export. Median ratios between
profile/baseline outer wall times varied roughly -0.6% to +3.1%; cache/load/sync
variation prevents treating that difference as an exact instrumentation overhead.

| Files | Phase | Baseline wall median (min–max) | Baseline CPU user/system | Profile outer wall median |
| ---: | --- | ---: | ---: | ---: |
| 1 | fresh | 4.4344 (4.3669–4.5524) | 0.15 / 0.50 | 4.5441 |
| 1 | unchanged | 2.0572 (2.0527–2.1781) | 0.20 / 0.30 | 2.1213 |
| 1 | modified | 2.1171 (2.1171–2.1377) | 0.21 / 0.31 | 2.1038 |
| 256 | fresh | 8.7577 (7.8493–8.8291) | 0.17 / 0.93 | 8.9100 |
| 256 | unchanged | 4.2817 (3.9827–4.4803) | 0.21 / 0.62 | 4.3831 |
| 256 | modified | 4.2513 (3.9785–4.3244) | 0.21 / 0.61 | 4.3838 |

## Where capture time goes

The next table shows **mean exclusive milliseconds** across three profiled trials.
Unlike separately selected medians, means allow the categories plus residual to
sum to mean total capture time. File/directory sync rows include all object types.
Directory sync here is the `sync_all` call itself; opening the directory handle is
in residual setup. Traversal includes enumeration/stat/open/sorting, but excludes nested ingestion and persistence. Chunk-path
metadata checks include directory existence checks and duplicate object checks;
there is **no upfront chunk-file existence probe** in the current implementation.

The no-clobber chunk row is the actual publication attempt that discovers EEXIST
on repeats, not a cheap pre-write existence test. Stored duplicate reading includes
open/read/allocation/validation bookkeeping but excludes separately timed hashing
and metadata checks. Residual includes other writes, tempfile setup/cleanup,
directory creation, JSON work, parent opens and instrumentation bookkeeping.

| Exclusive stage (ms) | 1 fresh | 1 unchanged | 1 modified | 256 fresh | 256 unchanged | 256 modified |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Directory traversal | 0.116 | 0.131 | 0.119 | 28.314 | 14.278 | 13.759 |
| Source file reads | 21.410 | 16.993 | 16.059 | 14.243 | 11.470 | 11.404 |
| FastCDC, excluding reads | 48.285 | 43.894 | 43.748 | 39.659 | 37.390 | 36.996 |
| BLAKE3/hex encoding | 102.085 | 139.344 | 141.696 | 103.334 | 149.321 | 147.815 |
| Chunk-path metadata checks | 15.131 | 12.778 | 12.864 | 15.693 | 16.183 | 16.396 |
| Chunk no-clobber publication / late existence detection | 172.852 | 10.921 | 10.937 | 204.707 | 14.284 | 14.401 |
| Chunk data writes | 79.017 | 42.642 | 39.113 | 146.840 | 67.748 | 70.659 |
| Stored duplicate chunk reads/bookkeeping | 0.000 | 54.092 | 55.238 | 0.000 | 123.740 | 68.958 |
| All file sync calls | 1536.971 | 1604.857 | 1590.951 | 3245.173 | 3421.588 | 3419.280 |
| All directory sync calls | 2282.066 | 3.577 | 17.308 | 4295.603 | 4.152 | 20.646 |
| Other setup/metadata/cleanup | 254.638 | 187.263 | 187.517 | 571.079 | 452.387 | 454.509 |
| **Total inside capture** | 4512.571 | 2116.492 | 2115.551 | 8664.645 | 4312.541 | 4274.824 |

High-level persistence operations below show **mean inclusive milliseconds**.
They include their child writes/hash/sync work already present above. **Do not add
these values to the exclusive table.** Manifest persistence includes serialization,
hashing, temporary publication and duplicate verification; Merkle persistence
includes canonicalization/hash/validation/publication. Snapshot publication
includes history namespace/nonce creation, ID/record generation and durable commit.

| Inclusive stage (ms) | 1 fresh | 1 unchanged | 1 modified | 256 fresh | 256 unchanged | 256 modified |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Chunk store put (all chunks) | 4347.355 | 1987.620 | 1975.720 | 6154.083 | 2926.112 | 2872.899 |
| Manifest persistence | 8.254 | 4.870 | 9.007 | 2332.267 | 1251.729 | 1260.232 |
| Merkle node persistence | 17.210 | 4.595 | 11.586 | 19.920 | 5.484 | 14.216 |
| Snapshot publication | 13.296 | 8.935 | 8.919 | 13.090 | 9.595 | 9.198 |

## Counts and write amplification

Counts were identical across all three trials for a given layout/phase. Init is
outside capture: its store/root/chunks/manifests setup is not charged here. Both
source layouts have one directory; the 256-file layout has 256 file manifests,
not 256 directory nodes. Snapshot IDs are always new, so one final record is
necessarily written even for an unchanged tree.

| Files | Phase | Chunks generated | Chunk data writes | Metadata data writes | File syncs | Directory syncs | Existing chunks detected late | New chunks |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | fresh | 415 | 415 | 3 | 418 | 844 | 0 | 415 |
| 1 | unchanged | 415 | 415 | 3 | 418 | 1 | 415 | 0 |
| 1 | modified | 415 | 415 | 3 | 418 | 6 | 414 | 1 |
| 256 | fresh | 567 | 567 | 258 | 825 | 1293 | 0 | 567 |
| 256 | unchanged | 567 | 567 | 258 | 825 | 1 | 567 | 0 |
| 256 | modified | 567 | 567 | 258 | 825 | 6 | 566 | 1 |

The metadata writes are 1 manifest + 1 tree + 1 snapshot for the one-file layout,
and 256 manifests + 1 tree + 1 snapshot for the many-file layout. Each data-write
call writes a whole temporary object. Their file sync counts have the same split.
Each capture also creates one empty nonce tempfile for its ID (no data-write/sync
call), in addition to one temporary per object. All phases read exactly 32 MiB of
source bytes **and write 32 MiB of chunk temporary payload**, including unchanged
repeats. Duplicate temporaries are removed; zero chunk-store growth does not mean
zero physical work.

Fresh one-file capture creates 210 chunk shard directories. Chunk directory syncs
are `415 publications + 2 × 210 new-directory syncs = 835`. The remaining 9 syncs
are 1 manifest, 5 tree namespace/shard/publication and 3 snapshot namespace/commit
syncs, totaling **844**. Fresh many-file capture creates 231 chunk shards:
`567 + 2 × 231 = 1029` chunk directory syncs, plus 256 manifest, 5 tree and 3 record
syncs, totaling **1293**. A repeat creates no shards and only the new snapshot
record's parent is synced. A small change adds one chunk, one manifest, one root
node and one record, but still syncs every duplicate temporary file too.

The 4 KiB edit added 1 chunk / 201,472 payload bytes in the one-file case, and
1 chunk / 124,180 bytes in the many-file case. Every prior chunk remained available
for A. These are content-defined chunk replacements, not 4 KiB patch storage.

## Bottleneck conclusion

File/directory synchronization dominates measured wall time, not CPU hashing or
source-byte reads. The percentage below is computed from summed stage times across
the same trials (not ratios of independently selected medians).

| Files | Phase | Time in file + directory sync calls |
| ---: | --- | ---: |
| 1 | fresh | 84.6% |
| 1 | unchanged | 76.0% |
| 1 | modified | 76.0% |
| 256 | fresh | 87.0% |
| 256 | unchanged | 79.4% |
| 256 | modified | 80.5% |

Fresh one-file capture spends about 3.82 of 4.51 capture seconds inside sync calls;
fresh many-file capture spends about 7.54 of 8.66 seconds there. FastCDC plus
BLAKE3 are approximately 0.15 seconds for either fresh layout; source reads are
about 0.01–0.02 seconds with these warm caches. More source files create more tail
chunks and many more manifest objects, hence more synchronization for the same
byte total. Native CPU times are much smaller than elapsed time.

On repeats/edits, the directory-sync cost mostly disappears but the engine still
writes, syncs, attempts no-clobber publication, discovers duplicates, then reads
and verifies them. The measured file-sync time is 1.60 seconds on the one-file
repeat and 3.42 seconds on the many-file repeat. Lookup/publication attempts and
chunk writes are much smaller costs than the preceding syncs.

These timers establish time **inside synchronization APIs**, including CPU and
blocking. They cannot distinguish APFS journaling, device/controller flush work,
scheduling or other kernel internals. No physical-disk trace was collected, so
claiming a specific SSD/APFS defect would be unsupported. The measured limitation
is the serial per-object durability path. Restore writes complete destination
files and syncs far fewer output objects; diff reads metadata without syncs.
Their lower times do not imply capture's chunker/hash is slow.

## Proposed optimizations — approval required

No speedup below has been implemented or benchmarked. Expected impact is tied to
the measured operation budget, not an asserted elapsed-time prediction.

| Priority | Change | Evidence / expected impact | Risk and durability requirements |
| ---: | --- | --- | --- |
| 1 | Verify existing chunks/manifests/trees **before** writing a temporary. | An unchanged one-file capture has 417 redundant file-sync requests out of 418; many-file has 824 out of 825. It could avoid those writes/syncs while retaining the new record commit. Benefits repeats/small edits; a fresh miss gains nothing and adds a lookup. | Low–medium. Keep bounded reads, BLAKE3/length/schema or exact-byte checks, corruption errors, and atomic no-clobber publication on misses/races. Continue reading/chunking/hashing all source files. Never trust a filename alone or silently repair corrupt data. Existing recovery semantics must remain explicit. |
| 2 | Coalesce dirty-directory syncs within an explicit capture commit. | Fresh directory syncs alone consume 2.28 s / 4.30 s. Many calls repeat the same parent/shard. A proven bottom-up barrier could avoid repeated empty-shard and per-object parent syncs. | Medium–high. Preserve public per-object APIs and their durability contracts; a capture-specific batch must not expose a committed record until all new files and required directory ancestors have been synced in dependency order. This requires fault tests, including new shard creation and sync failure. Merely deleting/moving sync calls out of the helper is unsafe. |
| 3 | Experiment with small bounded concurrency for independent object persistence. | Fresh file syncs still consume 1.54 s / 3.25 s after the directory budget. Overlap may help, but the measurements do not establish how much or whether APFS will benefit. | Medium–high, unproven. Keep every required sync, bound buffered data, join all writes/errors before parent commit, retain no-clobber semantics and the external single-writer contract. Benchmark 2–4 workers against the serial reference; revert if worse. |
| Deferred | Pack multiple immutable objects into durable segments. | Could amortize per-file creation/sync cost if simpler fixes remain inadequate. | High: format/index/recovery/migration redesign. Not an incremental first optimization and not started. |

Priorities 1–3 retain content verification. Directory batching/concurrency must be
implemented with an explicit successful commit boundary; public chunk/manifest
persistence cannot quietly become less durable. A relaxed durability option is
not part of this proposal. Nor should initializing/precreating shards outside the
measurement simply move the cost out of the timed operation: include init+capture
if evaluating that alternative. FastCDC replacement, BLAKE3 removal and source
mtime-based skipping are not supported by these measurements.

Recommended next action: approve **priority 1** first, with corruption/race/error
regression tests and the same 49-test suite plus these baseline comparisons. It
has the clearest removable cost and smallest change surface. Then evaluate an
explicit capture-batch design for fresh captures, keeping the public per-object
contract intact. The direct removed-call budget is not a promised proportional
speedup: I/O effects and caches can change after optimization.

Implementation was stopped pending approval at the end of this investigation.
The user subsequently approved priority 1; its results are recorded separately.
Directory batching, concurrency and packed storage remain proposals only. No
watcher, incremental tracking, retention/GC or other Phase 3 feature was started.
