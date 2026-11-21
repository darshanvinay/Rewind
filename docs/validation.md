# Current release validation

The current v1.0 candidate is documented in [release-1.0.md](release-1.0.md).
The milestone reports below are historical and retain their original test counts
and measurements. Strict is the only publisher; fast remains disabled. New records
use v2 validation provenance and watch includes periodic full reconciliation.

---

# Validation — Phase 3 C and D shadow foundation

**Current milestone:** The D shadow foundation adds bounded cache structures,
typed dirty scopes and non-publishing candidate comparisons. **119 tests pass**,
with zero failures/ignored tests. Milestone C remains the only publisher. See
[shadow-only measurements](shadow-merkle.md); earlier C/A/B reports below are
historical evidence, not end-to-end incremental speedup claims.

**Phase 2 follow-up:** Capture directory batching, exclusive writer coordination and
pending-marker recovery supersede the earlier performance follow-up. See
[the protocol, current validation and paired measurements](capture-batching.md).
The verified-existing-object fast path previously passed 57 tests; its
[implementation and results](verified-reuse.md) remain a historical reference.
The original 49-test Milestone E run and its measurements below are retained as a
historical baseline, not current optimized capture timings.

Completed on 2026-11-06 (America/Toronto). Phase 2 is ready within its documented
manual, single-writer, trusted-store scope on the tested macOS platform. No Phase 3
features were added. Earlier milestone counts are superseded by this final run.

## Environment and reproduction

- Apple M4 MacBook Air (`Mac16,12`), arm64, 16 GiB RAM.
- macOS 15.7.4 (24G517); local APFS Data volume `/dev/disk3s5`, 4096-byte allocation
  blocks. Both repository and temporary data were on this volume.
- Rust 1.99.1 (`58cf8f9c3`, 2026-10-13), Cargo 1.99.1, edition 2024.
- Python 3.13.13; native macOS `/usr/bin/time -l` for CPU and maximum RSS.
- Release executable built with `cargo build --release --locked`, default release
  profile, no custom optimization flags. The raw report records its SHA-256.

The session used an isolated toolchain without changing shell startup files:

```sh
export PATH=/tmp/rewind-cargo/bin:$PATH
export CARGO_HOME=/tmp/rewind-cargo
export RUSTUP_HOME=/tmp/rewind-rustup
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --locked
python3 scripts/measure_phase2.py --binary target/release/rewind --trials 3 --sizes-mib 8 32 --output docs/measurements-phase2.json
```

The `/tmp` exports are session-specific and may expire; use a normal stable Rust
installation if those paths are absent. The harness needs Python 3.11+ and native
`/usr/bin/time`; Linux timing parsing exists but was not validated here. It uses
isolated temporary stores/outputs, verifies results and removes them afterward.
Run it with the intended storage device as `--work-parent` to measure another
filesystem. No user source files are modified by these tests/measurements.

## Final automated results

| Check | Actual result |
| --- | --- |
| `cargo fmt --check` | Passed. |
| `cargo clippy --all-targets -- -D warnings` | Passed, no warnings. |
| `cargo test` | 49 passed; 0 failed; 0 ignored. |
| `cargo build --release --locked` | Passed. |
| Release CLI walkthrough | Passed verbatim from README. |
| Measurement scenarios | All six trials passed all verification checks. |

The final suite has four unit tests and 45 integration tests:

| Suite | Tests | Coverage |
| --- | ---: | --- |
| Library unit tests | 4 | Relative parent sync/error propagation; pre-publication injected failure; late destination collision; non-UTF-8 parser rejection. |
| Phase 1 storage | 7 | Empty/text/large binary/repeated bytes, dedup, corrupt/missing chunks, malformed manifests, stale temp writes, write failures, CLI BLAKE3 round trip. |
| Directory nodes | 7 | Canonical bytes, ordering, hashes, empty nodes, subtree reuse, unsafe names, corruption/missing nodes, manifest linkage. |
| Snapshot capture | 8 | Nested/empty trees, unique IDs/equal roots, store exclusion, symlinks/special files, failed publication paths, depth limits, source byte maps. |
| Snapshot restoration | 9 | Historical bytes, deleted live source, binary data, existing outputs, malicious metadata/names, corrupt/missing metadata/chunks, case collisions, cleanup. |
| Snapshot diff | 9 | Add/delete/modify, directory expansion, type changes, equal roots/subtree pruning, rechunking, corrupt visited metadata, sorted output, oracle comparisons. |
| Final workflow/regressions | 5 | All CLI commands with add/delete/modify and replacements; deeper tree; source move; strict object reads; oversized duplicate manifest; history symlink; closed stdout. |

Five property suites run 92 configured cases (32 + 32 + 8 + 8 + 12). All executed
on this host; no tests were disabled to obtain a passing result. Some suites are
compile-time platform-gated, so this is not evidence for Windows/other targets.
The macOS filesystem itself rejects invalid UTF-8 filename bytes, so rejection is
also tested directly at the parser boundary. The full run used the approved
execution environment outside Codex's filesystem sandbox for isolated Unix socket
fixtures; this did not require running as root. The native timing tool similarly
needed access denied by the restricted sandbox to obtain CPU/RSS counters.

### End-to-end evidence

`complete_cli_workflow_and_merkle_ancestor_behavior_without_original_source`:
initializes a store inside a temporary project; captures A and an identical copy;
modifies, adds and deletes files/directories; performs both file-to-directory and
directory-to-file replacements; captures B; checks affected ancestors and unchanged
siblings; moves the project/store so its recorded source path is absent; diffs and
lists history; restores both complete states; compares exact byte/directory maps;
checks invalid IDs and existing outputs; and runs single-file store/verify/restore.
It includes a 24-level nested tree, a 300,000-byte binary file and empty directories.
Invalid depth >256 is covered separately for capture and a forged diff graph.

The README walkthrough was also executed verbatim with `/bin/sh -eu` against the
release binary. Its A/B restores matched complete expected maps, including binary
bytes, deleted/added files and empty directories; the live source stayed in B and
no staging leftovers remained. Actual diff rows were:

```text
A	file	"added.txt"
D	file	"remove.txt"
M	file	"src/note.txt"
```

The demo directory was removed after validation. The release executable digest
was checked against the executable recorded by the measurement harness.

## Review findings and focused fixes

| Finding | Fix and regression evidence |
| --- | --- |
| A newly created relative top-level store did not sync its current-directory entry. | Directory creation now syncs `.` as the parent. A private sync callback test checks the actual call path and injects a parent-sync failure. |
| Duplicate manifest persistence read an existing corrupt file without a size bound. | Duplicate validation uses the bounded regular-object reader; an oversized sparse existing manifest is rejected without replacement/allocation proportional to its claimed size. |
| Chunk/tree/manifest readers could follow final symlinks or attempt special-file reads, unlike snapshot records. | One shared reader rejects non-regular/symlink files, checks opened metadata and bounds growth. Unix NOFOLLOW/NONBLOCK prevents final-component symlink/FIFO redirection/blocking. Regression tests cover symlink and directory objects. |
| Capture could write through a symlinked history namespace that history listing rejected. | Directory validation rejects symlink leaf namespaces. A regression test confirms capture fails and the external directory receives no record/tempfile writes. |
| CLI `println!` could panic on stdout failure. | Output uses `Write` results and an explicit flush; a closed-output regression returns status 1 without a panic. |
| Tree property-test names could randomly hit unsupported reserved device names. | The generator now prefixes names, keeping generated test inputs within the intended valid-name domain. No production name-policy relaxation. |

No API/format rewrite, unsafe Rust or new dependency was introduced. Private
failure injection tests stop after tempfile sync but before publication: history
keeps only its existing valid record and the incomplete temporary is removed.
Existing blocked-history and corrupt-object capture tests also prevent a new
record. Directory-sync failure is propagated, and a destination appearing after
restore preflight is preserved by the real no-replace rename primitive. These are
not abrupt-process-kill or power-loss tests, and post-publication errors remain
ambiguous as documented in the architecture.

## Reproducible local measurements

Raw samples, native timing output, commands, dataset metadata, binary digest and
summary statistics: [measurements-phase2.json](measurements-phase2.json).
Harness: [measure_phase2.py](../scripts/measure_phase2.py).

Each size has three serial trials with a fresh store. The dataset is one deterministic
SHAKE256 payload (`rewind-phase2-e-payload-v1` seed), sixteen distinct 1024-byte
small files, one empty file and four directories including the root. Total logical
source sizes are 8,404,992 and 33,570,816 bytes. Each trial captures A, captures an
identical state, XOR-replaces 4096 bytes at payload midpoint +137, captures B,
diffs changed/equal roots, and restores B. It additionally restores A and validates
SHA-256/length/directory maps for both states and the unchanged live source.
Those generator/validation steps and the second restore are outside the summarized
timed commands. The native tool measures the CLI process, not the Python generator.

Elapsed time below is the high-resolution Python timer around the native time
launcher plus CLI process, including startup overhead. User/system CPU time and
peak resident set size (RSS) come from native time. CPU numbers are printed at
0.01-second precision: `0.00` is rounded, not evidence of no CPU work. RSS is
normalized to bytes in the raw report and shown in MiB here; it is not virtual
memory, total machine RAM, filesystem-cache size or allocator accounting.

All rows show medians over three trials; elapsed ranges show min–max. MiB is 2^20
bytes, seconds are elapsed/CPU seconds.

| Payload | Operation | Elapsed median (range), s | CPU user / system, s | Peak RSS median / maximum, MiB |
| ---: | --- | ---: | ---: | ---: |
| 8 MiB | Fresh capture | 1.6870 (1.6773–1.7111) | 0.04 / 0.18 | 4.516 / 5.344 |
| 8 MiB | Identical capture | 0.7325 (0.7261–0.7388) | 0.06 / 0.11 | 5.844 / 6.031 |
| 8 MiB | 4 KiB-edit capture | 0.7448 (0.7339–0.7555) | 0.06 / 0.11 | 5.609 / 5.828 |
| 8 MiB | Changed-root diff | 0.0109 (0.0106–0.0112) | 0.00 / 0.00 | 1.484 / 1.484 |
| 8 MiB | Equal-root diff | 0.0074 (0.0072–0.0075) | 0.00 / 0.00 | 1.453 / 1.453 |
| 8 MiB | Restore B | 0.1148 (0.1066–0.1160) | 0.01 / 0.01 | 2.172 / 2.578 |
| 32 MiB | Fresh capture | 4.9385 (4.8899–4.9629) | 0.19 / 0.58 | 5.734 / 6.297 |
| 32 MiB | Identical capture | 2.3030 (2.2784–2.3190) | 0.25 / 0.36 | 6.859 / 7.266 |
| 32 MiB | 4 KiB-edit capture | 2.3003 (2.2770–2.3177) | 0.23 / 0.34 | 7.453 / 8.000 |
| 32 MiB | Changed-root diff | 0.0081 (0.0078–0.0087) | 0.00 / 0.00 | 1.609 / 1.609 |
| 32 MiB | Equal-root diff | 0.0061 (0.0060–0.0061) | 0.00 / 0.00 | 1.453 / 1.453 |
| 32 MiB | Restore B | 0.1408 (0.1405–0.1528) | 0.04 / 0.02 | 2.406 / 2.828 |

### Chunk-store growth

All three trials of each size produced the same object counts and payload growth.
Counts below exclude manifests, directory nodes, history and temporary files.

| Payload | Initial chunk objects / payload bytes | After identical capture | After 4 KiB edit | Extra chunk payload |
| ---: | ---: | ---: | ---: | ---: |
| 8 MiB | 126 / 8,404,992 | unchanged | 127 / 8,476,253 | +1 object / +71,261 bytes |
| 32 MiB | 447 / 33,570,816 | unchanged | 448 / 33,686,768 | +1 object / +115,952 bytes |

The 4096-byte patch stores a whole affected chunk, not a 4096-byte delta. Old chunks
are retained for A. These are logical payload bytes, not physical disk usage:
directory/inode overhead, allocation rounding and metadata are excluded. No retention
or GC was run.

### Interpretation and limits

- Data was generated and hashed immediately before capture. Cache state was warm
  and uncontrolled; caches were not dropped, and background system load was not
  controlled. No tests/builds ran concurrently with these measurements.
- Launch overhead is substantial for these short diff commands. Three repeats are
  illustrative local samples, not a statistically broad benchmark or comparison
  against other tools. Disk/controller/filesystem sync behavior matters.
- The observed RSS is consistent with bounded chunk buffering for these inputs,
  but does not prove constant total memory. Manifests, recursive directory metadata
  and diff output still grow; serialized caps are not global process-memory caps.
- A repeated capture still scans/reads everything and writes/syncs duplicate
  temporaries before checking existing objects. Metadata files, per-chunk copying
  and many syncs are known costs; they were documented, not prematurely optimized.
- CPU time is time spent executing on CPU; elapsed also includes waiting. A low CPU
  figure relative to elapsed does not identify an I/O bottleneck without profiling.
  No unsupported throughput or asymptotic speedup claim is made.

## Remaining risks and completion decision

Phase 2's manual capture/history/diff/verified-restore scope is complete on the
validated macOS platform. No known blocking correctness issue remains within the
single-writer, trusted-store/stable-parent contract. The limits remain material:

- Source changes during capture are not a point-in-time view; capture fully scans
  files, and automatic monitoring/incremental source tracking is absent.
- Writer exclusion is not enforced by a lock; no reader/GC coordination exists.
- Filesystem ancestor/mount mutation by a hostile actor is outside the contract;
  metadata IDs do not authenticate records. Stored-object checks are not encryption.
- Snapshot restore requires macOS/Linux no-replace rename support. Linux and
  non-macOS platforms were not tested; platform-gated tests are not portability proof.
- F_FULLFSYNC, network filesystem semantics, abrupt interruption and power-loss
  behavior are untested. Successful sync relies on the filesystem/hardware.
  Post-publication errors may leave visible data; crashes may leave unreferenced
  chunks/tempfiles/stages. No scavenger, migration system, retention or GC exists.
- Original file metadata, sparse extents and hard-link identity are unsupported;
  symlinks/special files are rejected. RSS/serialized bounds do not cap all metadata
  construction costs.
- Diff skips equal descendants and all chunks; listing only validates records.
  Neither certifies full restorability. Full restoration verifies stronger content
  invariants, but authenticity requires a future design.

The README and architecture now describe these actual semantics, on-disk v1 formats,
all eight CLI commands, and a proposed Phase 3 sequence. Phase 3 remains unstarted
and requires the user's approval.


## Approved verified-existing-object follow-up

Formatting, Clippy with warnings denied, all **57 tests** (zero failures/ignored),
and the locked release build passed on macOS. Chunks are BLAKE3-verified before
reuse; manifests/nodes retain their byte/schema/identity validation. Only NotFound
selects durable new-object publication, and no-clobber losers verify the winner.
New-object sync policy is unchanged. The eight new regressions cover verified
hits with no publisher call, corruption/type/permission errors, missing objects,
publication errors and races, same-length damage/truncation/empty chunks, invalid
metadata, and dangling symlink/directory rejection without repairs.

The paired old/new/profile harness rotated executable order across three trials
of both 32 MiB layouts. It checked identical roots/chunk growth, source byte counts,
write/sync counts, and both historical restore maps after renaming the original
source. [Verified reuse results](verified-reuse.md) contain the timing/count tables,
limits and reproduction command; [raw results](capture-profile-verified-reuse.json)
retain CPU/RSS and stage data. No directory batching, concurrency, durability-mode
change or Phase 3 feature was implemented.

## Approved capture batching follow-up — 2026-11-10

Formatting, Clippy (`-D warnings`), all **67 tests** (zero failures/ignored), locked
release build and release CLI benchmark/restore checks passed. Capture now batches
only directory syncs, with a complete object barrier before snapshot publication.
An advisory OS lock covers capture, recovery and standalone object mutations.
A durably published versioned pending marker survives failures; recovery fails
closed and flushes surviving directories/ancestors before clearing it or reusing
objects. Existing BLAKE3 verification, file syncs and no-clobber publication remain.

Three-trial fresh medians improved from 5.0897 to 3.1832 seconds (one 32 MiB file)
and 8.3499 to 4.8142 seconds (256 files totaling 32 MiB). Unchanged medians regressed
from 0.0923 to 0.1102 and 0.1092 to 0.1174 seconds; modified medians from 0.1079 to
0.1137 and 0.1137 to 0.1169 seconds. Directory sync counts fell from 844/1,293 to
220/241; required file syncs remain the dominant cost. The sub-second goal was not
met. [Protocol, crash analysis, full timing/CPU/RSS tables and reproduction](capture-batching.md),
[raw paired samples](capture-batch-benchmark.json) and
[diagnostic profile](capture-batch-profile.json) retain the evidence.

Failure injection, abrupt subprocess exit and SIGKILL validate process recovery,
not actual power-loss behavior. macOS still uses sync_all rather than F_FULLFSYNC;
trusted paths, advisory-lock participation and hardware/filesystem sync behavior
remain prerequisites. No GC, packed log, parallel persistence or Phase 3 was added.


## Phase 3 Milestone A — native filesystem watcher (historical)

Validated on 2026-11-10 (America/Toronto): Apple M4, 16 GiB, macOS 15.7.4 / local
APFS, Rust/Cargo 1.99.1. Dependencies pinned by Cargo.lock: notify 8.2.2 and
ctrlc 3.5.2. Linux/inotify and other platforms were not tested. No latency/idle-CPU,
throughput or automatic snapshot benchmarks are claimed at A; the loop blocks on
channel receive and no automatic snapshots exist to benchmark. Phase 2 performance
numbers above remain historical rather than new watcher measurements.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
# Focused real filesystem checks:
cargo test --test watch_tests --locked
```

Formatting, Clippy with warnings denied and the complete suite passed:
**77 tests, 0 failed, 0 ignored** (24 library tests, including the existing subprocess
helper, plus 53 integration tests). All previous 67 tests still pass; five watcher
unit tests and five native integration tests were added. The existing five property
suites retain 92 generated cases. Native/signal/socket checks ran outside the Codex
sandbox. Expected failure cases print diagnostics while their tests pass.

Real filesystem tests exercise creation, existing-file modification, binary data,
file deletion and rename, newly created nested directories, directory rename,
canonical ancestor aliases, custom storage locations and unrelated `.rewind`
directories. CLI tests verify stdout JSON, Ctrl+C exit status 0, startup/runtime
symlink and special-file rejection, root removal, actual nested `.rewind` exclusion,
and a manual capture while monitoring. Monitoring alone creates no snapshot,
chunk data or pending marker. The manual-capture concurrency test creates exactly
the one explicitly requested record. Native notification counts/order vary by OS;
tests wait for affected paths instead of demanding a one-event-per-write model.

Unit tests cover paired rename order, store-boundary filtering, vanished paths,
backend rescan flags, malformed/unknown paths, changed store identity, coarse root
validation, bounded queue overflow/oversized notifications and shutdown wakeup.
Queue-loss checks do not pretend to reproduce a real kernel overflow. During
development a root-symlink fixture was corrected to use an allowed ancestor alias;
the final checks have no failures/skips.

A is complete. The watch command logs normalized raw hints and stops explicitly on
detected loss/errors; it does not debounce, reconcile, retry captures or create
snapshots. Process-crash/power-loss guarantees remain those of the unchanged Phase 2
engine. Milestone B and all later work await approval. The architecture documents
native backend differences, queue bounds and remaining event-delivery limitations.

The locked release build also passed. A separate release CLI smoke test initialized
a temporary store, started `watch project` (backend `Fsevent`), wrote binary
`proof.bin`, received a normalized `create` hint, sent SIGINT, observed `stopped`
and exit status 0, and verified zero snapshots/pending markers. To reproduce
interactively, run the README watch example and edit a file from a second terminal.


## Phase 3 Milestone B — event debouncing and normalization (historical)

Validated on 2026-11-10 (America/Toronto), Apple M4 / 16 GiB / macOS 15.7.4, local
APFS, Rust/Cargo 1.99.1. No dependency or storage-format changes. Linux remains
untested. Milestone C automatic snapshot integration was not started.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
# Real filesystem tests (includes output backpressure and continuous writes):
cargo test --test watch_tests --locked
# Interactive release smoke:
./target/release/rewind watch project --debounce-ms 100 --max-delay-ms 500
```

All checks passed: **87 tests, 0 failed, 0 ignored**, consisting of 30 library tests
(including the existing subprocess helper) and 57 integration tests. A's existing
tests now consume batches; unknown/lost-event policy checks expect explicit
reconciliation hints instead of A's former fatal-loss behavior. All Phase 1/2
storage, integrity, locking and crash-recovery tests still pass. The existing five
property suites retain 92 generated cases. Native FSEvents, socket and signal tests
ran outside the Codex sandbox. A restricted lib-test attempt could not start an
FSEvent stream; the successful complete run was unrestricted. Expected invalid-entry
cases print diagnostics while their tests pass.

Six new library test entries cover deterministic clock-based quiet/max deadlines,
repeated modifications, create/delete preservation, atomic-save endpoints, separate
rename halves, tracker retention, sticky reconciliation without idle heartbeats,
path/rename/byte capacity limits and timing configuration. The native loop test
injects an application-overflow flag and a backend rescan hint, verifies an empty
reconciliation batch is emitted, then verifies later real filesystem changes remain
marked and monitoring continues. This injection does not claim to induce a real
kernel queue overflow.

Four new integration tests cover:

- Twelve rapid writes coalescing one affected path, a temporary-file rename save,
  directory rename and rapid create/delete, with zero automatic snapshots.
- Continuous 30-ms writes lasting over two seconds: a configured 600-ms maximum
  produces a batch while writing remains active, rather than waiting indefinitely
  for the 300-ms quiet period. Precise deadline behavior is tested with synthetic
  Instants; the native test does not assert a universal wall-time latency bound.
- An output writer deliberately blocked during its first batch. A real file change
  during the pause appears in a later batch after output resumes, demonstrating
  independent callback intake and no accidental clearing of the pending queue.
- Invalid/duplicate/unknown timing flags, missing/non-integer values and inconsistent
  windows failing before watcher startup.

Existing real tests retain canonical/custom/nested storage exclusion, root/symlink/
special-file rejection, Ctrl+C and concurrent manual capture. Confirmed storage-only
callbacks are now discarded before queueing; mixed rename endpoints and backend
rescan hints are retained. A coarse ancestor hint cannot be attributed only to
storage and conservatively requires reconciliation.

A separate locked-release smoke used a fresh temporary store and the Fsevent backend
with quiet=100 ms / maximum=500 ms. Twelve writes to binary `note.bin` produced one
batch with a single affected path and create/modify kinds. Replacing it by renaming
`.save-temp` preserved both temporary/destination hints and native rename records,
with `reconciliation_required: true` / `rename_scope`. SIGINT emitted stopped and
exit status 0. A second idle run stopped with no batch. Both runs verified zero
snapshots, zero chunk objects and no pending marker. These observations are smoke
results, not throughput, CPU, memory or event-to-snapshot benchmarks.

Reconciliation reasons remain sticky for the monitoring session. B neither performs
that reconciliation nor persists the hint stream; C must establish an authoritative
baseline on restart and account for events/loss during any future capture before
acknowledging reconciliation. Maximum delay bounds collection time, not OS delivery,
validation, blocked output or future capture duration. Quiet time is not a guarantee
of stable/atomic filesystem contents. Native unreported event loss remains possible.
Documentation now describes the batch schema, queue and accumulator bounds, rename
semantics, shutdown behavior and platform limitations. Milestone B is complete;
Milestone C awaits explicit approval.


## Phase 3 Milestone C — automatic snapshot integration

Validated 2026-11-10 (America/Toronto), Apple M4 / 16 GiB / macOS 15.7.4, local APFS,
Rust/Cargo 1.99.1. No dependencies or on-disk versions changed. Startup/configuration errors also
emit fatal structured JSON (`watch_setup`) before readiness. Linux is unvalidated.

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`
and the locked release build passed. Final count: **105 passed, 0 failed, 0 ignored**
(39 library + 66 integration entries, including two subprocess helpers). The existing
five property suites retain 92 cases. An initial full-suite depth test caught a
recursive stack overflow; extraction of the file leaf helper fixed it without
reducing the depth limit. A permission-review timeout happened before execution;
the authorized retry succeeded. Final native/signal/socket checks ran outside the
Codex sandbox. Expected fatal test cases print errors while their tests pass.

The event-only library API is retained and tested. The product CLI now captures an
initial baseline, then captures from debounced hints. New tests cover manual/automatic
root parity, zero-write identical checks, changed/missing/corrupt stored objects,
source-scoped restart deduplication, rapid saves, atomic-save renames, deletions,
nested binary data and empty directories, simultaneous watchers/manual writers,
historical restoration and nested storage feedback suppression. Cancellation tests
cover writer-lock waiting and post-object-barrier abort, marker retention and
successful next-writer recovery. Object/recovery barrier failure injection fails
closed. Controlled source changes between passes cause retry without publication.
A controlled change after final checks demonstrates the documented TOCTOU window:
the first complete archive is valid older data and a retained event produces a
second complete archive. Overflow/rescan indications during capture remain pending
until a subsequent authoritative scan. No actual kernel overflow/power cut was induced.

Release CLI validation/benchmarks:

```sh
python3 scripts/benchmark_automatic.py --trials 3 --mib 32 --many-files 256   --idle-seconds 2 --output docs/automatic-capture-benchmark.json
```

All six release trials passed baseline, unchanged skip, three edits, 25-write burst,
idle, Ctrl+C, five independent restores and restart skip. Exactly five records per
trial, all **30 historical restores** matched, zero duplicate records on unchanged
checks or restarts, and no marker remained after normal shutdown.

| Median elapsed seconds | One 32 MiB file | 256 files totaling 32 MiB |
| --- | --- | --- |
| Fresh automatic baseline | 2.7142 | 4.8320 |
| Unchanged touch → skipped | 0.4421 | 0.4549 |
| 4-KiB edit → committed snapshot | 0.4958 | 0.5066 |
| Final write of 25-write burst → snapshot | 0.4906 | 0.4976 |
| Restart → skipped baseline | 0.1074 | 0.1322 |

All event-to-outcome numbers include native delivery, the default 300-ms debounce,
both full source scans, metadata validation, lock waiting and durable commit where
needed. Worker-only edit medians were 0.1791 / 0.1900 seconds. Fresh baselines include
process startup/native setup and do not wait for debounce. These are not comparable
speedups against earlier manual benchmarks with different payloads/timing boundaries.

Every measured edit/final burst added one chunk, manifest and root node: 134,257 /
86,977 chunk payload bytes. Unchanged/restarts added zero objects/records. Idle
`ps TIME` had no measurable CPU increase at 0.01-second resolution across six ~2-s
windows; this is not proof of zero CPU. RSS endpoints were 9.64–11.61 MiB, stable
within each idle window, not peak or kernel-cache memory. See [full method, ranges,
worker times, growth and limitations](automatic-capture.md) and [raw results](automatic-capture-benchmark.json).

Source observations remain non-atomic. Unreported events, final-check/publication
TOCTOU, foreground process termination and full-scan costs remain material. Existing
BLAKE3/no-clobber/file-and-directory sync/recovery guarantees are preserved; macOS
still does not use F_FULLFSYNC. The completed C implementation adds no incremental
cache, daemon, retention, GC or Phase 4 functionality. Milestone D awaits approval.


## Milestone D foundation — bounded incremental shadow construction

Formatting, Clippy with warnings denied and all **119 tests** passed on macOS
(39 library + 80 integration entries, including two existing subprocess helpers).
The release CLI and example builds passed. Existing five property suites retain
92 generated cases; new deterministic randomized differential coverage performs
80 filesystem-operation steps. The native/signal/socket suite ran outside the
Codex sandbox. No dependencies, storage formats, synchronization ordering or
publication guarantees changed. Linux remains unvalidated.

Twelve new shadow tests cover matching edit roots, intentional missed-event
mismatch, directory moves/empties, hard links inside/outside the tree, explicit/
identity/entry/byte/plan invalidation, atomic saves, deletions, type replacements,
symlinks, storage exclusion, cancellation, preserved mtime/length, read-only storage
inventories, per-file manifest work limits and randomized operations. Two new live watcher tests confirm the root
comparison is diagnostic and that a warm cache cannot hide corrupted archive chunks.
All original C durability, interruption, overflow and concurrency tests still pass.

Separate release measurements:

```sh
python3 scripts/benchmark_shadow.py --output docs/shadow-candidate-benchmark.json
python3 scripts/benchmark_automatic.py --trials 1 --mib 32 --many-files 256   --idle-seconds 2 --output docs/shadow-validation-benchmark.json
```

On Apple M4 / 16 GiB / macOS 15.7.4 / APFS / Rust 1.99.1, balanced trees of 4 KiB
files yielded single-edit candidate medians of 0.972 / 2.029 / 17.757 ms at 1,000 /
10,000 / 100,000 files. Corresponding one-pass full-source root-oracle medians were
15.430 / 183.555 / 4,749.656 ms. These cover different work and are not interchangeable
validation stages. Every edit candidate read one file and rebuilt two nodes; moves
and bursts read 100 files. All 27 measured candidate/oracle comparisons matched,
with zero storage object/record/marker publication by the diagnostic benchmark.

The live 32 MiB trials separated candidate construction from mandatory C validation:
three edit medians were 67.524 / 9.365 ms candidate and 144.831 / 183.211 ms C full
validation/commit plus cache collection for one file / 256 files. Measured overall
edit-to-outcome medians were 0.535 / 0.515 seconds, including debounce and added
shadow work. Both release trials passed five historical restores and restart
suppression, with exactly five C records each. No end-to-end speedup is claimed.

C's 100,000-entry observation limit remains unchanged. The 101,001-entry large
benchmark uses a separately bounded read-only diagnostic scanner, not C publication.
Cache charges are not literal RSS limits; the entire diagnostic benchmark peaked
at 201.83 MiB with generation, warmups, old/new seeds and full scans included.
[Full methodology, ranges, CPU/memory limits and risks](shadow-merkle.md),
[raw candidate data](shadow-candidate-benchmark.json) and
[separate C stage data](shadow-validation-benchmark.json) retain the evidence.

Only shadow construction is implemented. A stale shadow root can be plausible;
matching metadata/events are not proof of source or archive integrity. C still
performs its full checks and is the only publisher. No event-assisted publication,
incremental commit/cache transaction, daemon, GC or Phase 4 work was started.
