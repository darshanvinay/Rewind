# Milestone D foundation — incremental candidates in shadow mode

This is the approved foundation only. **Milestone C remains the sole publisher.**
No event-assisted publication, incremental durability protocol, daemon, GC,
retention or Phase 4 functionality was added.

## Implementation

`shadow::MerkleCache` is a bounded worker-owned index of relative paths, source
stamps, file manifest references, directory hashes, and an inode-to-path alias
index. It contains no file payloads and holds no per-entry file descriptors.
Directory child references are represented by the path index; directory JSON is
reconstructed with the existing `DirectoryNode` constructor/encoding when needed.

Limits are clamped to 131,072 entries and a 128 MiB logical allocation charge.
The charge includes fixed per-entry/index overhead, path copies, and directory
encoding allowance. It is **not an exact allocator/RSS cap**. Entry counts,
variable-length payload charges, existing encoding limits and bounded work prevent
unbounded cache growth. A new seed and old baseline may coexist until replacement;
C observations, manifests/chunk buffers and native watcher resources have separate
costs. Budget exhaustion discards the optional seed rather than truncating a tree.
No LRU, persistent cache format or payload/verification cache was introduced.
The cache is process-local and tied to the current binary's v1/default-chunking rules.

Typed `DirtyScope` values distinguish a file path, immediate-directory relist and
recursive subtree rescan. `DirtyPlan` caps scopes at 4,096 and path bytes at 1 MiB;
invalid relative components or plan overflow promote to full reconciliation.
Paired rename endpoints become subtree scopes. Unpaired/ambiguous renames,
reported loss and other reconciliation reasons conservatively skip speculative
construction. C still does its full scan in every case.

Construction relists affected ancestors, checks actual entry types using no-follow
operations, and compares identity/size/mode/link-count/mtime/ctime where visited.
Dirty files are always read, even when length/mtime match. Other visited entries
with changed metadata are inspected as additional suspicious work. Unaffected
subtrees can retain their cached hashes. Directory names sort exactly as in C;
file ingestion, FastCDC, BLAKE3, manifest encoding and canonical node hashing reuse
the existing implementations. Files are read once for the speculative candidate,
with local before/after checks; this is not C's two-pass source validation.

Known hard-link aliases are expanded together. If cached link count exceeds known
in-root aliases, the candidate requires reconciliation instead of guessing outside
alias coverage. Inode identity is an invalidation aid, not proof of unchanged bytes.
Create/delete and type replacement use observed directory contents, not event replay.
Directory rename rescans the moved subtree; it is not an O(depth) shortcut.

The builder neither mutates the baseline nor installs candidate state. It retains
only a root/statistics result. It has a bounded logical work budget, including per-chunk reference charges before
manifest accumulation, and checks cancellation during traversal and chunk ingestion. Rejected paths, source errors,
budget errors and cancellation cannot authorize publication. Errors invalidate the
optional cache. Explicit loss/identity/budget invalidations have typed reasons and
cache epochs; epoch is diagnostic, not a watcher completeness guarantee.

## Connection to the existing worker

The existing one-job/one-result channels remain bounded. The controller merges
bounded dirty plans for future work, and the one capture worker:

1. Computes a read-only shadow candidate, or reports why reconciliation is required.
2. Calls the unchanged C full-scan publication algorithm, with an optional bounded
   metadata/reference collector in its first probe.
3. After C succeeds, compares the shadow root with C's root and reports a match or
   `mismatch_discarded`.
4. Installs **only the seed from C's successful two-pass attempt**, never a candidate.
   Failed C attempts invalidate the cache; oversized seeds leave it disabled.

Collection adds memory/encoding work, but cannot select a root, skip C validation,
or authorize a record. Comparison happens after C succeeds; a mismatch does not
require a second fallback capture because C already performed authoritative work.
C still owns writer locking, pending-marker recovery, verified object reuse,
BLAKE3 checks, file syncs, no-clobber publication, directory barriers and cancellation.
No shadow code calls an object or snapshot publisher. Manual commands are unchanged.
The public `capture_automatic` path can run without a shadow collector.

Additive JSON records have `kind: "shadow"`, `publisher: "milestone_c"`, status,
`candidate_ms`, `full_validation_ms`, cache counters and optional root/statistics.
`matches_authoritative` is emitted only when a candidate can be compared against a
successful C result. `full_validation_ms` includes C verification/publication and
seed collection; worker `elapsed_ms` also includes the extra shadow construction.

## Correctness boundary

Metadata equality and absence of events do not prove source equality. The missed
notification test modifies a file under an otherwise untouched subtree and sends
only an unrelated dirty scope. It deliberately obtains a stale shadow root, while
C independently publishes the correct full-source root. This negative test is part
of the design, not an incremental publication guarantee.

Likewise, cached archive references do not prove stored data is intact. A warmed
shadow cache cannot bypass C verification: the live CLI regression corrupts a chunk,
triggers an identical-source check, and requires a fatal error with no new record.
Two-pass source checking remains non-atomic and subject to the existing TOCTOU and
native-event limitations. No stronger history completeness or power-loss claim is
made. Source/cache changes during shadow construction may cause mismatch/error;
C either independently validates/publishes or fails under its existing policy.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Final results: **119 passed, 0 failed, 0 ignored**, comprising 39 library and 80
integration entries (including the two existing subprocess helpers). Twelve new
shadow tests cover edits, missed events, moved directories/empty directories, hard
links within/outside the tree, explicit/identity/budget invalidation, excessive dirty
scopes, atomic saves, deletions, file/directory replacements, symlinks, store identity
exclusion, cancellation, unchanged mtime/length, no storage mutation and 80 seeded
randomized operation steps. Differential comparisons use quiescent real filesystem
states and the shared authoritative full-source root algorithm.

Two live CLI regressions check diagnostic/root comparison with exactly C-created
records, and fatal stored corruption despite a warm cache. All prior durability,
concurrency, overflow, cancellation and process-interruption tests remain passing.
Expected fatal cases print diagnostics. Native/signal/socket tests ran outside the
Codex sandbox. Only macOS was validated. No actual power-loss test was performed.

## Separate candidate benchmark

```sh
python3 scripts/benchmark_shadow.py --output docs/shadow-candidate-benchmark.json
# Or run the release diagnostic directly:
cargo run --release --locked --example benchmark_shadow -- 1000 10000 100000
```

Apple M4 / 16 GiB / macOS 15.7.4, local APFS, Rust 1.99.1, default release build.
Unique 4 KiB files are generated from BLAKE3 XOF of each file index, grouped 100
files per directory. Cache warmup is outside candidate timings. Each size measures
five single-file edits, three moves of a 100-file directory and one 100-file burst.
Each candidate is checked against a separate full-root scan; all **27 comparisons
matched**, and all three runs created zero chunks, manifests, nodes, markers or
snapshot records. The example initializes only the empty stores used for the test.

| Files | Single edit candidate median (range), ms | Full-root scan median for edit checks, ms | Cached entries / logical MiB |
| --- | --- | --- | --- |
| 1,000 | 0.972 (0.968–1.016) | 15.430 | 1,011 / 0.855 |
| 10,000 | 2.029 (2.006–2.072) | 183.555 | 10,101 / 8.537 |
| 100,000 | 17.757 (16.238–18.645) | 4,749.656 | 101,001 / 85.364 |

Each single-file candidate read exactly one 4 KiB file, listed two directories and
rebuilt two nodes. Root fanout still grows from 10 to 1,000 groups; unchanged
immediate entries require metadata work. This is not constant time or O(depth).

| Files | 100-file directory rename candidate median (range), ms | 100-file burst candidate, ms |
| --- | --- | --- |
| 1,000 | 2.539 (2.508–2.542) | 2.042 |
| 10,000 | 4.090 (3.968–4.160) | 3.175 |
| 100,000 | 20.496 (19.035–24.969) | 20.128 |

Renames and bursts each read 100 files / 409,600 bytes, listed two directories and
rebuilt two nodes. Cache warmups were 19.150 / 201.011 / 4,930.043 ms. Full-root
oracles in this table are **one read-only source pass**, not C's two source passes,
archive verification, final metadata checks or durable commit. They are not
substitutable validation stages and these ratios are not end-to-end speedups.

The diagnostic oracle is bounded separately to 131,072 observed entries and 32 MiB
of charged names; it omits C's global observation inventory because it cannot
publish. **C's existing 100,000-entry / 16 MiB observation limits are unchanged.**
The 100,000-file case has 101,001 entries and therefore is a shadow diagnostic only,
not a claim that C can publish that tree. Raising/scaling C's observation limit
remains outside this implementation.

Native `/usr/bin/time -l` measured the entire example at 1,115.73 s elapsed, 17.52 s
user CPU, 51.00 s system CPU and 201.83 MiB peak RSS. Those totals include generation,
all warmups/candidates/oracles and cleanup, not just candidate construction. Two
cache generations can coexist during warmup. OS page cache is not included in RSS.
Whole-program wall time varied substantially between runs, while generation,
teardown and off-CPU delay were not separately instrumented; its total cannot be
attributed to candidate construction. Warm caches and uncontrolled host/filesystem
load limit interpretation. Raw samples,
counts, source fingerprints and environment are in
[shadow-candidate-benchmark.json](shadow-candidate-benchmark.json).

## Live C validation remains separate

```sh
python3 scripts/benchmark_automatic.py --trials 1 --mib 32 --many-files 256 \
  --idle-seconds 2 --output docs/shadow-validation-benchmark.json
```

One release trial per layout, same host, exactly 32 MiB of the existing automatic
benchmark payload. Each trial covers baseline, identical touch, three 4 KiB edits,
25-write burst, idle, SIGINT, five historical restores and restart suppression.
All **10 historical restores matched**; each trial produced exactly five C records,
with no duplicate records for the unchanged check/restart. All ten warm candidate
roots matched C's successful roots; cold baselines correctly required reconciliation.

| 32 MiB layout | Candidate for edits, median ms (3 samples) | C full validation/commit + seed collection, median ms | Edit-to-outcome median seconds |
| --- | --- | --- | --- |
| One file | 67.524 | 144.831 | 0.535 |
| 256 files | 9.365 | 183.211 | 0.515 |

The one-file candidate rereads the entire dirty 32 MiB file. The many-file candidate
reads one 128 KiB file, but relists/rebuilds its flat 256-entry parent. C still scans
and verifies the entire tree. End-to-end times include the 300-ms debounce and
**additional** shadow work. This single-trial observation is not a before/after
speedup experiment; shadow mode adds work and no end-to-end improvement is claimed.
The two ~2-second idle windows showed no measurable CPU increase at 0.01-s `ps TIME`
resolution, not proof of zero CPU use. All timings, stage splits, outcomes, growth
and idle observations are in [shadow-validation-benchmark.json](shadow-validation-benchmark.json).

Candidate timing excludes authoritative validation and therefore demonstrates
construction potential only. The approved stopping point is reached. No candidate
publication, validation weakening, permanent cache or subsequent milestone was added.
