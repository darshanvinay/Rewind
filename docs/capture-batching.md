# Capture directory-sync batching

Implemented and validated on 2026-11-10 (America/Toronto), within Phase 2.
This change does not introduce watchers, parallel persistence, packed storage or a
weaker durability mode. It preserves the existing `sync_all` contract, rather than
claiming a new guarantee against power loss.

## Protocol and ownership

`transaction.rs` adds a private capture session to `ChunkStore`. The public API
signatures and chunk/manifest/tree/snapshot format versions remain unchanged.

1. Acquire an exclusive OS lock on the permanent `writer.lock` inode. Recovery,
   captures, initialization and standalone chunk/manifest/node mutations share it.
   Never unlink or replace this file while the store is in use. Locks are advisory;
   older Rewind versions and external programs do not participate.
2. If `capture.pending` exists, validate its exact versioned contents, then sync all
   surviving storage namespace directories, children before parents, including the
   store root and canonical ancestors. Reject invalid markers/types or failed scans
   and syncs. **Do not clear the marker if that barrier fails.** Read-only operations
   never initiate recovery or clear markers.
3. Durably publish `capture.pending` containing `rewind-pending-v1\n`: synced
   temporary, no-clobber publication, root-directory sync. Only then begin mutations.
   The marker is a journal flag; **it is not a lock**.
4. For each referenced chunk/manifest/node, verify an existing object's contents as
   before. Only NotFound permits a new publication. Missing objects still use unique
   temporaries, complete writes, flush, **file sync before publication**, and atomic
   no-clobber publication. A race loser verifies the winning object.
5. Capture defers only object-directory syncs, collecting unique dirty paths and
   ancestors, including links for freshly created shards/namespaces. Sync that set
   once, deepest first through the root. Failure aborts with no new history record
   and leaves the marker. Missing directories after interruption cannot supply
   reachable objects for reuse; recovery flushes surviving ancestors, and subsequent
   capture recreates missing objects normally.
6. Seal the object phase. Snapshot publication is rejected before the barrier, and
   new object publication is rejected after sealing. Publish the synced snapshot
   record without replacement, then sync its history directory. This is the commit
   point for a successfully returned snapshot.
7. Remove the marker, sync the root, and release the lock. A cleanup failure may
   return an error even though a complete snapshot is already visible.

Standalone puts and metadata saves keep their immediate object and directory
syncs, and also use the lock/recovery gate and pending marker. This adds fixed
journal overhead to standalone mutations, including verified hits; they are not
batched or converted into a full-file transaction. `store_file` coordinates each
object mutation separately. Initialization also flushes canonical ancestors to
repair surviving links from an interrupted initialization. An incomplete store
missing its required namespaces must be reopened through `init`, not `open`.

In Rust, the session **owns** the lock's `File`; closing/dropping it releases the
OS lock even when `?` returns early. `Writer<'a>` borrows its store, so the store
cannot disappear while a standalone guard uses it. Explicit `finish(self)` consumes
the guard/session on success. Drop never attempts fallible disk cleanup or silently
commits failed work. The dirty-path set removes duplicate directory syncs and is
protected by a mutex, retaining `ChunkStore`'s Send/Sync properties. Capture itself
remains serial. In-memory phase flags enforce API order; atomic memory operations
are not disk durability barriers—successful file/directory syncs provide those.

## Failure and crash behavior

| Interruption | Possible result and next-writer behavior |
| --- | --- |
| Before marker publication is durable | No batched objects were changed. A surviving valid marker can be conservatively recovered. |
| During mkdir, data write, file sync or object publication | No new snapshot. Temporary files, synced orphan objects and surviving unsynced directory links may remain. Lock release permits the next writer to recover. |
| During object directory barrier | No new snapshot. Marker remains; recovery retries the complete surviving namespace barrier. |
| After object barrier, before snapshot publication | Durable orphan graph, no new history record. |
| After record publication, before history-directory sync | Record may survive or disappear after an actual crash. Any surviving record points to the already completed object graph. |
| After history-directory sync, during marker cleanup | Complete record may be visible despite a returned error. A surviving/reappearing marker causes harmless extra recovery. |

Recovery repairs namespace durability, not corrupted payloads. File contents were
synced before their final names were published; recovery therefore syncs directory
links, and ordinary reuse still re-reads and verifies bytes with BLAKE3. Corrupt
objects are reported and preserved, never silently repaired. Recovery cannot
retroactively identify failures made by older binaries without a pending marker.
It scans surviving namespace entries and can be expensive for a large store. Orphan
objects/tempfiles are not reclaimed. Trusted, stable paths remain a requirement.

A process exit or SIGKILL releases locks/descriptors but leaves kernel caches alive.
These tests are **not power-loss tests**, and deleting a shard to model a lost link
is not proof of real filesystem crash behavior. Hardware/filesystem sync semantics
remain prerequisites. macOS still uses `sync_all`, not F_FULLFSYNC; network filesystems,
Windows and Linux have not been validated here. Readers do not coordinate with GC;
no GC is implemented. Waiting writers have no timeout/cancellation interface.

## Reproducible paired benchmarks

Host: Apple M4, 16 GiB, arm64 macOS 15.7.4, local APFS Data volume. Rust/Cargo 1.99.1,
release builds, Python 3.13.13. The reference executable was copied **before edits**
from the existing verified-reuse release. SHA-256:

- Baseline: `a774001ac602dec071da07a4550a8ac12261fee86a06cab24ea3cf8bb388899b`.
- Batched: `1befb4edfeb073306458fdd6249ea6a9c6ab79838d8692cf20b42e72536becc6`.

Generate exactly 32 MiB using SHAKE256(`rewind-capture-profile-v1`), either one file
or 256 distinct 128 KiB files. Each of three trials uses separate fresh stores per
executable, alternating execution order. Capture fresh, unchanged, then after XORing
4096 bytes at the selected file's midpoint +137 (middle file for the many-file case).
Each run checks identical roots/chunk growth across executables, checks metadata diff,
renames the source away, restores A/B and compares directory/length/SHA-256 maps.

```sh
# Save before making engine changes; do not overwrite it with the new executable.
mkdir -p /tmp/rewind-batch-before
cp target/release/rewind /tmp/rewind-batch-before/rewind
python3 scripts/benchmark_capture_batch.py \
  --reference-binary /tmp/rewind-batch-before/rewind \
  --trials 3 --output docs/capture-batch-benchmark.json

# Separate one-trial diagnostic run with a disposable instrumented source copy.
python3 scripts/profile_capture.py \
  --reference-binary /tmp/rewind-batch-before/rewind \
  --trials 1 --mib 32 --many-files 256 \
  --output docs/capture-batch-profile.json
```

Timing uses `perf_counter` around native `/usr/bin/time -l`, including launcher
startup. Native CPU times have 0.01-second granularity; RSS is peak process resident
memory, not total filesystem-cache memory. Input generation/validation warms caches;
cache state, background load and SSD/controller state are uncontrolled. This is a
small local experiment, not a cross-platform prediction or statistical confidence
interval. Instrumented wall timers add overhead and are reported separately.
The three-trial benchmark preceded test-only fault-helper corrections; rebuilding
after those corrections produced the **identical release executable hash** above.
Raw files retain source fingerprints and every sample.

| Layout | Capture | Before median seconds (range) | After median seconds (range) | Change |
| --- | --- | --- | --- | --- |
| 1 × 32 MiB | Fresh | 5.0897 (5.0837–5.1371) | 3.1832 (2.9820–3.2108) | 37.5% less time; 1.60× |
| 1 × 32 MiB | Unchanged | 0.0923 (0.0917–0.0962) | 0.1102 (0.1091–0.1112) | +17.9 ms; 19.4% slower |
| 1 × 32 MiB | Modified | 0.1079 (0.1040–0.1954) | 0.1137 (0.1119–0.1141) | +5.8 ms; 5.4% slower |
| 256 × 128 KiB | Fresh | 8.3499 (8.2505–8.4741) | 4.8142 (4.5558–4.8895) | 42.3% less time; 1.73× |
| 256 × 128 KiB | Unchanged | 0.1092 (0.1012–0.1819) | 0.1174 (0.1115–0.1178) | +8.2 ms; 7.5% slower |
| 256 × 128 KiB | Modified | 0.1137 (0.1081–0.1153) | 0.1169 (0.1167–0.1180) | +3.2 ms; 2.8% slower |

The journal/barrier protocol has a fixed cost, visible in unchanged/modified captures.
Small-sample outliers are retained rather than discarded. The under-one-second goal
was **not reached**; no file sync or verification was removed to approach it.

| Layout | Fresh CPU user/system seconds before → after (medians) | Fresh peak RSS MiB before → after (median; maximum) |
| --- | --- | --- |
| One file | 0.17 / 0.56 → 0.16 / 0.44 | 5.469; 6.109 → 6.188; 6.406 |
| 256 files | 0.17 / 1.03 → 0.16 / 0.80 | 7.062; 7.219 → 7.266; 7.312 |

After-change unchanged CPU medians are 0.06/0.01 and 0.06/0.02 seconds; modified
medians are 0.05/0.01 for both layouts. Other per-sample CPU/RSS values remain in
[raw benchmark data](capture-batch-benchmark.json). Chunk growth is identical:
415 chunks / 33,554,432 payload bytes for one file and 567 / 33,554,432 for many.
Unchanged adds zero chunk bytes; the edit adds one chunk (201,472 or 124,180 bytes).

## Operation counts and remaining costs

Diagnostic results are recorded separately in [raw profile data](capture-batch-profile.json).

| Layout / phase | File sync calls before → after | Directory sync calls before → after |
| --- | --- | --- |
| One file, fresh | 418 → 419 | 844 → 220 |
| 256 files, fresh | 825 → 826 | 1,293 → 241 |
| Either, unchanged | 1 → 2 | 1 → 3 |
| Either, modified | 4 → 5 | 6 → 9 |

Before counts are from the prior verified-reuse profile of the same deterministic
datasets; after counts are directly instrumented in this diagnostic run. The extra
file sync writes the pending marker. Fresh new-object data writes equal file sync
counts: 419 or 826 (415/567 chunks, 1/256 manifests, one node, marker, record).
The empty nonce tempfile and lock-file opens are not payload writes in these counts.
New directories are 210/231 chunk shards, two tree namespace/shard directories and
one history namespace. Namespace creation is still accounted for, not skipped.
Directory sync reductions are 73.9% / 81.4%.

| Instrumented fresh capture | One file | 256 files |
| --- | --- | --- |
| Capture wall time inside engine | 2.9490 s | 4.8690 s |
| File sync elapsed | 1.5171 s | 3.0249 s |
| Directory sync elapsed | 0.8444 s | 0.9207 s |
| Data write elapsed | 0.0586 s | 0.1127 s |

File sync now accounts for about 51% / 62% of instrumented elapsed time; all syncs
account for about 80% / 81%. These are elapsed wall timers around sync calls,
not CPU time or physical-disk traces. Instrumented stage timing is not substituted
for the uninstrumented three-trial benchmark. Remaining required file syncs and
hundreds of separate object files dominate this implementation; directory batching
alone does not make sub-second capture feasible on this measured host. Parallel
persistence or a packed durable log would require a separate design/approval and
new failure analysis. Neither was implemented, and removing file syncs or integrity
checks was not used as an optimization.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

All passed: **67 tests, 0 failed, 0 ignored** (19 library tests, including the
subprocess helper entry, plus 48 integration tests). Existing five property suites
still run 92 generated cases. All eight CLI commands are exercised in integration
smoke tests; paired benchmark runs additionally exercise release init/capture/diff
and both historical restores. The existing socket rejection fixture needs permission
to create Unix sockets; the full successful run was outside the Codex sandbox.

Ten added test entries cover:

- Failures at marker durability, new-directory creation, file sync, publication,
  multiple positions in the directory barrier, sealing, snapshot file sync,
  snapshot directory sync and cleanup. Older snapshots remain restorable; any
  record visible after rename references a complete graph.
- Recovery scan/shape/marker checks and repeated barrier failures, retaining the
  marker; successful retries through an already-open handle and standalone reuse.
- Surviving ancestors and deliberately removed orphan shards/tree namespaces;
  subsequent reuse still rejects corrupt chunk bytes without overwriting them.
- Child-before-parent ordering, unique dirty directories, marker-before-write
  ordering, snapshot-after-barrier ordering and phase-API misuse rejection.
- Abrupt subprocess exit at nine boundaries, a separate SIGKILL during capture,
  automatic lock release and successful next-writer recovery/restoration.
- An active capture blocking child captures, chunk puts, manifest saves, node saves
  and initialization. Recovery runs only after the lock becomes available.

Initial development runs caught path assertions comparing macOS `/var` aliases with
canonical `/private/var` paths and a SIGKILL helper setup error; both test issues were
corrected. Final checks have no failures/skips. No actual power-cut experiment,
filesystem fault emulator or cross-platform durability certification was performed.

Phase 2's approved capture optimization is complete. The one-second performance
target remains unmet, with the costs and small-capture regressions recorded above.
Phase 3 has not begun.
