# Phase 2 final validation — Milestone E

**Current follow-up:** Capture directory batching, exclusive writer coordination and
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
