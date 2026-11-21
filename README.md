# Rewind

Local file history in Rust: capture a directory, keep deduplicated historical
versions, compare snapshots, and restore verified contents without Git commits.
Rewind runs on demand or watches a directory in the foreground.

**v1.0 release scope:** strict automatic capture, manual snapshots, verified
restoration, and metadata diffs. Incremental Merkle construction is diagnostic
shadow mode only. **Fast publication is unfinished and disabled.** Strict capture
reads the whole source twice; this release does not claim atomic filesystem
snapshots or incremental end-to-end speedups.

## Install from source

Use Rust 1.89 or newer (edition 2024); validation used 1.99.1. macOS is the only
validated platform. Unix capture and macOS/Linux restore implementations exist;
Linux and Windows releases have not been validated. No prebuilt binaries or
crates.io release are assumed to exist.

```sh
git clone https://github.com/darshanvinay/Rewind.git
cd Rewind
cargo install --path . --locked
rewind --version
rewind --help
```

For development, run `cargo build --release --locked` and use
`./target/release/rewind`. Every command uses `.rewind` in the **current working
directory**; Rewind does not search parent directories for a store.

## Demonstration

Run in a new directory with `rewind` installed:

```sh
mkdir rewind-demo && cd rewind-demo
mkdir -p project/src project/empty
printf 'before\n' > project/src/note.txt
printf 'delete me\n' > project/remove.txt
printf '\000\377binary\n' > project/data.bin
rewind init
snapshot_a="$(rewind snapshot project)"
printf 'after\n' > project/src/note.txt
printf 'new\n' > project/added.txt
rm project/remove.txt
snapshot_b="$(rewind snapshot project)"
rewind snapshots
rewind diff "$snapshot_a" "$snapshot_b"
rewind restore-snapshot "$snapshot_a" recovered-a
rewind restore-snapshot "$snapshot_b" recovered-b
printf 'before\n' | cmp - recovered-a/src/note.txt
cmp project/src/note.txt recovered-b/src/note.txt
cmp project/data.bin recovered-a/data.bin
```

Automatic history, in the same terminal:

```sh
rewind watch project --mode strict --debounce-ms 300 --max-delay-ms 2000 \
  --reconcile-interval-secs 60
# Edit project from another terminal; Ctrl+C stops monitoring.
```

Wait for a `snapshot_created` or `skipped_identical` JSON line before assuming
baseline capture is complete. Restart performs a fresh full validation and avoids
a duplicate record when the latest source-specific Merkle root matches.

## Commands

| Command | Behavior |
| --- | --- |
| `init` | Create or reopen the local store. |
| `snapshot <directory>` | Capture a directory; print an opaque snapshot ID. Manual captures always create a new record. |
| `watch <directory> [options]` | Baseline, debounced automatic captures, periodic reconciliation; identical roots skip new records. |
| `snapshots` | List validated history records, oldest first. Does not verify referenced objects. |
| `diff <before-id> <after-id>` | Sorted additions, deletions and content modifications. Renames appear as deletion/addition. |
| `restore-snapshot <id> <new-directory>` | Verify and restore the historical tree. Parent must exist; destination must not exist. |
| `store <file>` | Store a regular file; print its manifest path. |
| `verify <manifest>` | Verify chunks and whole-file digest. |
| `restore <manifest> <new-file>` | Verify and restore a file without replacing an existing destination. |
| `--help`, `--version` | Show usage or version without requiring a store. |

For a single file:

```sh
manifest="$(rewind store project/src/note.txt)"
rewind verify "$manifest"
rewind restore "$manifest" recovered-note.txt
```

Watch defaults: `--mode strict`, `--debounce-ms 300`, `--max-delay-ms 2000`,
`--reconcile-interval-secs 60`. Timing requires integer milliseconds satisfying
`1 <= debounce <= max-delay <= 60000`. Reconciliation requires integer seconds
`1..86400`, cannot be disabled, and is scheduled from the preceding successful
full attempt's start. Each flag can appear once. `--mode fast` fails explicitly
before starting monitoring; it never silently substitutes another mode.

Watch stdout is newline-delimited JSON: startup, `batch`, `shadow`,
`reconciliation`, `snapshot_created`, `skipped_identical`, `error`, and shutdown.
Outcomes include `capture_mode: "strict"`, `validation_scope: "full_source"`,
`consistency: "two_pass_validated_observations"`, and the referenced record's
`record_source_validation`. A skipped legacy/manual record keeps its original
provenance even though the current attempt performed strict validation.
Errors return status 1; successful commands return 0, including nonempty diffs.
Diff rows use tabs: `A`/`D`/`M`, entry type, quoted relative path. Paths sort by
UTF-8 bytes; type replacements emit deletion then addition.

## Architecture and durability

```text
source bytes -> streaming FastCDC -> BLAKE3 chunks
             -> file manifest (ordered chunks + whole-file hash)
             -> canonical sorted directory Merkle nodes
             -> snapshot record (root + source + timestamp + validation provenance)
```

FastCDC defaults to 16/64/256 KiB minimum/target/maximum chunks. Unchanged content
shares objects; changing a leaf changes its ancestors, preserving unchanged
sibling hashes. Storage layout:

```text
.rewind/
  writer.lock                    permanent advisory writer lock
  capture.pending                present while recovery may be required
  chunks/<2 hex>/<62 hex>         raw chunks
  manifests/<64 hex>.json         manifest v1
  trees/<2 hex>/<62 hex>          directory node v1
  snapshots/<snapshot-id>.json    record v2; legacy v1 remains readable
```

Record v2 records `manual_traversal` or `strict_two_pass` source validation.
Version 1 has unknown provenance; old binaries reject v2 records. No existing
record is rewritten. Records are schema checked, not cryptographically authenticated.

All cooperating storage writers take an exclusive advisory lock. Capture recovers
any pending marker, publishes the marker durably, verifies reused objects, writes
new objects through synced temporary files and atomic no-clobber publication,
then synchronizes object directories before publishing/syncing history. Directory
sync batching applies only to capture; public per-object writes retain their
immediate durability protocol. Recovery repairs surviving namespace links before
reuse and retains the marker if a required barrier fails. Never remove the lock
or pending marker to bypass an error. Unreferenced objects can remain after failure.

Automatic capture compares two full source roots and observed metadata, then
revalidates paths before publication. One worker holds the writer lock while the
controller retains bounded future work. Overflow, ambiguous input and restart
require full reconciliation; every strict attempt already scans the full tree.
Periodic scans also discover stable changes lacking usable events. A mismatch or
transient source failure triggers bounded retry cycles and backoff. Corrupt stored
objects, failed durability/recovery barriers and root replacement fail closed.
Cancellation preserves required recovery state; a late Ctrl+C can allow a complete
commit. Shutdown does not guarantee capture of the final edit.

**Source freshness and stored-object integrity are different.** A watcher can miss
an edit without corrupting any stored object. BLAKE3 detects stored bytes that do
not match their content identity; it cannot prove that every source change was
observed. Shadow candidates can miss unrelated source changes and never publish
or skip authoritative validation. Future fast publication requires a validated
object overlay, full referenced-archive verification, provenance and a sealed
commit protocol; a cached root hash is insufficient.

Restore verifies node/manifest identities, lengths, chunk hashes and whole-file
hashes before no-replace publication. It does not require the original source.
**Diff is not an integrity scan:** it checks visited metadata, skips equal
subtrees, and never reads chunks. Corruption in skipped data can remain undetected.

## Benchmarks

See [current release validation](docs/release-1.0.md) for actual release timings,
commands, environment, archive-verification costs and limitations, plus the raw
[32 MiB measurements](docs/release-1.0-benchmark.json) and
[file-count measurements](docs/release-1.0-scale-benchmark.json).
Local release results (Apple M4, macOS/APFS, warm uncontrolled cache):

| Dataset | Fresh baseline | Small-edit outcome latency | Separate archive verification |
| --- | ---: | ---: | ---: |
| 32 MiB, 1 file | 2.88 s | 531 ms | 35 ms |
| 32 MiB, 256 files | 4.90 s | 517 ms | 40 ms |
| 3.91 MiB, 1,000 files | 10.04 s | 472 ms | 34 ms |
| 39.06 MiB, 10,000 files | 95.25 s | 1,383 ms | 376 ms |

Outcome latency includes event delivery and 300-ms debounce. The 32-MiB rows use
three trials (nine edits per layout); scale rows use one trial (three edits).
Archive verification is a separate warm workload, not a subtracted component of
capture. Fresh many-file performance remains a limitation. The 100,000-file case
exceeds the current authoritative entry limit and was not measured end-to-end.

No strict-versus-fast comparison is available because fast publication is disabled.
Earlier [shadow construction measurements](docs/shadow-merkle.md) measure candidates
only and must not be described as end-to-end speedups.

## Limits

- Two scans and final metadata checks are observations, **not an atomic point-in-time
  snapshot**. Source data may change between checks or after final validation.
  Quiesce files when logical consistency matters. Transient versions between
  captures may never enter history; reconciliation cannot recover past missed edits.
- Full capture costs scale with the source. Automatic observations are bounded to
  100,000 entries including directories/root and 16 MiB of absolute path bytes;
  100,000 files exceed this limit. The optional shadow cache has separate bounds.
- Queue/batch memory is bounded; native registrations and represented metadata
  have separate costs. Callback queue: 1,024 notifications; batch: 4,096 paths,
  1,024 rename hints, 1 MiB path bytes. Continuous edits, blocked output or disk I/O
  can delay capture/shutdown. The reconciliation interval is not a staleness SLA.
- Trusted local storage/source paths are required. Advisory locks do not constrain
  external tools or older binaries. Hostile ancestor/mount replacement is outside
  the threat model. BLAKE3 is not encryption or authentication against store edits.
- Symlinks, special files and nonportable/non-UTF-8 names are rejected. Actual nested
  storage is excluded by identity; unrelated directories named `.rewind` remain data.
  Depth is capped at 256; chunks 16 MiB, manifests 64 MiB, nodes 16 MiB, records 64 KiB.
- Restores preserve contents and empty directories, not timestamps, original modes,
  ownership, ACLs, xattrs, sparse extents or hard-link identity. New restored files
  use 0600 and directories 0700, subject to umask. Existing outputs are refused.
- No daemon, retention, garbage collection, compression, encryption or GUI.
- Only macOS/local APFS has been validated. Native events are advisory; Linux,
  network filesystems and actual power-loss behavior are unvalidated. macOS uses
  `sync_all`, not `F_FULLFSYNC`; device caches/hardware can weaken persistence.
  Subprocess interruption tests demonstrate process-crash handling, not power-loss
  guarantees. Errors after sync/publication can leave a complete visible record/output.

## Verify and release

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Native watcher tests need normal OS watcher/signal permissions. See
[release checklist](docs/release-1.0.md), [architecture](docs/architecture.md),
[historical validation](docs/validation.md), and [capture protocol](docs/capture-batching.md).
License: [MIT](LICENSE). Nothing is pushed, tagged or published by these commands.
