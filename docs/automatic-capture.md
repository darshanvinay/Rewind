> v1.0 update: strict is the sole publisher, `--mode fast` is rejected, records
> use v2 provenance (legacy v1 remains readable), and full reconciliation is now
> scheduled every 60 seconds by default. The C checkpoint below is historical.
> Current results: [release validation](release-1.0.md).

# Automatic snapshots — Phase 3 Milestone C

Implemented and validated on 2026-11-10 (America/Toronto). `rewind watch` is now a
foreground automatic-history command. Milestone D incremental processing and
Phase 4 have not begun.

```sh
cargo build --release --locked
# In a directory with an initialized .rewind store:
/path/to/rewind watch project --debounce-ms 300 --max-delay-ms 2000
```

## Architecture and consistency

The native watcher and debounce controller keep receiving hints while **one capture
worker** scans and commits. The event inbox retains its 1,024-message bound; the job
and completion channels each have capacity one. Only one job runs at a time, and
additional batches coalesce into a future-work flag rather than an unbounded task
list. Existing path/rename/byte batch limits remain. There is no daemon or incremental
Merkle cache.

Every startup requests a baseline. Under the exclusive writer lock, recovery runs
if necessary, then history is read using its existing timestamp/ID ordering. The
latest record for the **canonical source path** is compared to a fresh full-source
probe. Across restarts, v1 records identify sources by path, not persisted inode;
monitoring pins directory identity only for the current session.

Probe, verification and persistence share the existing traversal, FastCDC streaming
algorithm, BLAKE3 identities, file-manifest encoding and canonical directory nodes.
Probe calls no object publisher and performs no application storage writes. The
second pass either:

- Verifies source bytes and all corresponding stored chunks/manifests/nodes before
  returning `Unchanged`, with no new marker or record; or
- Persists changed objects through verified reuse, file sync and no-clobber
  publication, requires the same root as the probe, completes the existing object
  directory barrier, and only then publishes the snapshot record.

An existing pending marker can require recovery writes even for an unchanged tree;
that is separate from starting a new transaction. Reads can naturally affect OS
atime/cache state. A bytes/identity/mode/mtime/ctime inventory test confirms the
normal identical path creates/rewrites no storage entries. Corrupt or missing
referenced objects are not silently accepted as an identical root.

For each observed file/directory, automatic scanning checks identity, size, mode,
mtime and ctime (including nanoseconds) around reads/traversal. It compares the two
roots and stamps, then reopens every observed path without following symlinks and
rechecks metadata after the object barrier. Temporary observations are capped at
100,000 entries and 16 MiB of absolute path bytes; exceeding that limit fails with
a diagnostic. Data structures/manifest/native watcher memory have additional costs.

**This is not an atomic point-in-time filesystem snapshot.** Checks cannot eliminate
TOCTOU races, changes between observations, change-and-change-back sequences, or
hostile/low-resolution metadata behavior. A source can change after the last check
and before publication. A deterministic test exercises exactly that window: the
first archive contains valid earlier bytes, and a retained event drives a later
archive containing the new bytes. Quiesce the source when a consistent logical
application state is required. Debouncing groups hints; it cannot preserve every
intermediate version of every save.

The controller advances an input generation for normalized events and observed loss.
A job records its starting generation. Success acknowledges old reconciliation state
only if no newer generation, pending batch/work or out-of-band overflow remains.
Events during successful captures therefore cause follow-up work; success cannot
silently erase them. Every C job is a full scan, including overflow/ambiguity jobs.
Unreported native event loss is still possible; C adds no periodic authoritative
rescan or persistent event journal. Restart always scans a new baseline.

Confirmed store-only events are filtered before queueing. Coarse ancestor hints
remain conservative. They may produce read-only identical checks, but those checks
write no new marker/objects/record, preventing a write-triggered snapshot loop.
Manual commands retain their existing behavior and use the same exclusive lock.

## Errors, cancellation and Rust ownership

`AutomaticCapture` distinguishes `Created(record)` from `Unchanged(record)`. Typed
`Unstable`, `Cancelled` and `SourceIo` errors distinguish observable source problems
from storage failures. The streaming reader records its own I/O error kind locally;
errors from chunk persistence cannot be reclassified as transient source errors.
Only known source NotFound/Interrupted/WouldBlock/PermissionDenied/NotADirectory
errors and observed instability are retried. Three-attempt cycles use 100/300-ms
short delays, then deferred cycles back off from 1 second to a 30-second cap. Event
intake continues during these waits. Unsupported entries, replaced roots, watcher
backend failures, corruption, malformed metadata and failed storage/recovery barriers
fail closed.

A Rust channel is similar to Python's bounded queue. Owned job/result messages and
session guards make lifetimes explicit: the worker keeps its lock until commit or
failure, and dropping a failed session leaves its marker for recovery. The main
thread joins the worker on exit. `Cell` tracks local read-error provenance without
sharing mutable reader state across threads. `BTreeMap`/`BTreeSet` keep deterministic
ordering. A file leaf helper avoids retaining ingestion locals at every recursive
directory depth; this fixes a debug-stack regression found by the existing depth test.

Writer-lock acquisition uses interruptible `try_lock` with 25-ms sleeps. Scanning
checks cancellation between entries/chunks and before publication. An in-progress
sync/final commit cannot always be interrupted immediately. A complete record can
still appear if Ctrl+C or another error arrives late; error/cancellation is not a
rollback promise. Cancellation never intentionally publishes a partial graph and
never clears a pending marker without its required barrier. Failed attempts can
leave orphan objects and tempfiles; no GC or scavenger was added.

Durability remains the Phase 2 contract: BLAKE3 verification, atomic no-clobber
publication, per-file sync, deepest-first object-directory barrier, then synced
history publication and marker cleanup. Tests inject object/recovery barrier failures
and cancellation before/after the barrier, checking marker retention and subsequent
recovery. Process-interruption tests do not prove power-loss durability. macOS still
uses `sync_all`, not F_FULLFSYNC. Trusted paths, participating advisory-lock writers
and hardware/filesystem sync behavior remain prerequisites. Linux is unvalidated.

## Structured output

The first `status: "watching"` line means event intake is ready, not that the baseline
is committed. Data records include:

- `batch`: bounded debounced hints, not an executable replay plan.
- `reconciliation`: `started` or `complete`, with generation/reason context.
- `snapshot_created`: ID, root, reason, worker elapsed milliseconds and
  `consistency: "two_pass_validated_observations"`.
- `skipped_identical`: the existing latest ID and verified matching root.
- `error`: `fatal`, diagnostic code/message, and retry/deferred timing when applicable.
  Startup/configuration failures use `code: "watch_setup"` before any ready line.
- `cancelled`, followed by `status: "stopped"` during ordinary cancellation.

A stopped command can report pending reconciliation; it does not guarantee that
shutdown captured the final edit. Output is synchronous, so blocked stdout can delay
progress/shutdown and overflow intake. Output failures propagate to a failing exit.
The library's event-only `Monitor::run` remains available; the CLI uses
`Monitor::run_automatic`.

## Tests and release validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

All passed: **105 tests, 0 failed, 0 ignored** (39 library and 66 integration tests,
including the two test-only subprocess helper entries). The existing five property
suites retain 92 generated cases. Native FSEvents, Unix socket and signal checks ran
outside the Codex sandbox. Event-only A/B tests still exercise their library contract
through a test-only subprocess helper; new CLI tests exercise automatic behavior.

Coverage includes baseline/edit/rapid saves, atomic-save renames, deletion, nested
binary files and empty directories, source-scoped restart deduplication, identical
checks without storage writes, corruption/missing metadata, concurrent automatic
and manual writers, historical restoration, nested-storage feedback suppression,
cancellation while waiting for a writer, cancellation after an object barrier,
source instability, failed object/recovery barriers, overflow during capture and
new events during a successful commit. Loss injection is test-only; it does not
pretend to cause a real kernel overflow or power failure.

The first full validation caught a stack overflow at the existing depth-limit test.
Moving file processing into a leaf helper reduced recursive frames while preserving
the 256-level limit. The complete final suite passes. One permission-review timeout
occurred before execution and the authorized check succeeded on retry.

## Reproducible automatic-capture measurements

```sh
python3 scripts/benchmark_automatic.py --trials 3 --mib 32 --many-files 256 \
  --idle-seconds 2 --output docs/automatic-capture-benchmark.json
```

Host: Apple M4, 16 GiB, arm64 macOS 15.7.4 / local APFS, Rust/Cargo 1.99.1,
Python 3.13.13, default release profile. Binary SHA-256:
`49a9d0eb0413ad270b76422d9696879fb85ee15fad3efcb61d5ec024ad7f2548`.

Each layout has exactly 32 MiB of SHAKE256(`rewind-auto-c-v1`) data: one file, or
256 distinct 128 KiB files. Each of three independent stores runs a baseline,
unchanged metadata touch, three distinct 4-KiB edits, a burst of 25 distinct 4-KiB
writes, idle observation, shutdown, five historical restores with the source moved,
and restart deduplication. Each trial produces exactly five records. All **30
historical restores** match expected directory/length/SHA-256 maps; all six restarts
skip without new records. The harness raises on any unexpected structured error.

Wall time for non-baseline phases is write-close to outcome-JSON receipt, including
native delivery, the 300-ms quiet window, controller/worker waiting, both source
passes, verification, metadata checks and durable commit where needed. Baseline
includes process launch, watcher registration and startup validation. Worker time
is the reported duration inside the worker, including lock/recovery, both scans and
commit, excluding debounce. This is actual automatic latency, not just hashing time.

| Workload | One file median seconds (range) | 256 files median seconds (range) |
| --- | --- | --- |
| Fresh baseline, 3 samples/layout | 2.7142 (2.7051–2.7644) | 4.8320 (4.7861–4.8879) |
| Unchanged touch → skipped, 3/layout | 0.4421 (0.4353–0.4477) | 0.4549 (0.4518–0.4634) |
| Repeated 4-KiB edit → snapshot, 9/layout | 0.4958 (0.4834–0.5084) | 0.5066 (0.4968–0.5146) |
| Final burst write → snapshot, 3/layout | 0.4906 (0.4855–0.4997) | 0.4976 (0.4969–0.5192) |
| Restart → skipped baseline, 3/layout | 0.1074 (0.1069–0.1075) | 0.1322 (0.1276–0.1324) |

| Worker-only median seconds | One file | 256 files |
| --- | --- | --- |
| Baseline | 2.7102 | 4.8277 |
| Unchanged | 0.1255 | 0.1383 |
| Repeated edits | 0.1791 | 0.1900 |
| Burst | 0.1748 | 0.1828 |

These are medians/ranges, not confidence intervals or speed comparisons against
Phase 2. The payload seed, workload and timing boundaries differ from historical
manual benchmarks. C deliberately performs more source validation. Warm/uncontrolled
caches, background load and SSD/controller behavior can materially affect times.
No premature concurrency/storage optimization was made to improve these results.

### Storage growth and idle resources

| Layout | Baseline chunks / payload bytes | Additional bytes for each measured edit or final burst | Records after all workloads |
| --- | --- | --- | --- |
| One file | 412 / 33,554,432 | 1 chunk / 134,257 | 5 |
| 256 files | 567 / 33,554,432 | 1 chunk / 86,977 | 5 |

Each edit/burst adds one manifest and root node. The 25-write burst coalesces to one
record in this workload. Unchanged checks/restarts add no records, manifests, nodes
or chunk bytes. Four changed outcomes add 537,028 / 347,908 chunk payload bytes.
This is workload-specific CDC/dedup behavior, not a universal per-edit storage bound.

Across six idle windows of about 2.01 seconds, `ps TIME` showed **no measurable CPU
increase at 0.01-second resolution**. That does not mean literal zero CPU use; these
short windows have roughly 0.5-percentage-point one-core resolution. Process RSS
endpoints were 9.64–11.61 MiB and unchanged within each idle window. RSS is not peak
memory or filesystem-cache memory. CPU is aggregate process CPU across threads,
not separate user/system measurements. A later non-Unix unsupported-method fallback is excluded from the measured macOS
build; rebuilding confirmed the identical release binary hash above. Raw `ps` observations and every timing,
worker duration, outcome, growth sample and source fingerprint are retained in
[automatic-capture-benchmark.json](automatic-capture-benchmark.json).

Remaining limits: full scans per job, foreground process lifetime, bounded observation
capacity, source TOCTOU, possible unreported native loss, coarse-event extra scans,
no guaranteed capture of every intermediate save, synchronous output, unvalidated
Linux/network behavior, and the existing sync/power-loss assumptions. Milestone C
is complete within this documented scope. Milestone D requires separate approval.
