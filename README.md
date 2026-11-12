# Rewind

Rewind is a local-first file-history engine written in Rust. It preserves file
contents in immutable, deduplicated storage so earlier captured versions
can be recovered without Git commits or separate full copies.

**Phase 2 is complete:** streaming FastCDC chunking, BLAKE3 chunk identities,
versioned file manifests, directory Merkle trees, manual snapshots, history,
verified file/directory restoration, and Merkle-aware diffs. **Phase 3 Milestones A–C**
add recursive monitoring, debounced batches and automatic durable snapshots with
`watch`. Automatic capture uses two full scans and observable stability checks;
it does not provide atomic point-in-time filesystem snapshots.

## Build and test

Use stable Rust with edition 2024 support. Validation used Rust/Cargo 1.99.1.
Dependencies are pinned in `Cargo.lock`.

```sh
cargo build --release --locked
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The engine is a library; the CLI is a thin caller. Commands use `.rewind` in the
**current working directory**; they do not search parent directories for a store.
`init` creates/reuses it. Quote paths containing spaces.

## CLI walkthrough

Run the build above from the repository, then copy this example into a shell.
All input/output files below are created in a new temporary directory.

```sh
REWIND_BIN="$(pwd)/target/release/rewind"
demo_dir="$(mktemp -d)"
cd "$demo_dir"
mkdir -p project/src project/empty
printf 'before\n' > project/src/note.txt
printf 'delete me\n' > project/remove.txt
printf '\000\377\200binary\n' > project/data.bin

"$REWIND_BIN" init
manifest="$("$REWIND_BIN" store project/src/note.txt)"
"$REWIND_BIN" verify "$manifest"
"$REWIND_BIN" restore "$manifest" recovered-note.txt

snapshot_a="$("$REWIND_BIN" snapshot project)"
printf 'after\n' > project/src/note.txt
printf 'new\n' > project/added.txt
rm project/remove.txt
snapshot_b="$("$REWIND_BIN" snapshot project)"

"$REWIND_BIN" snapshots
"$REWIND_BIN" diff "$snapshot_a" "$snapshot_b"
"$REWIND_BIN" restore-snapshot "$snapshot_a" recovered-a
"$REWIND_BIN" restore-snapshot "$snapshot_b" recovered-b
cmp recovered-a/src/note.txt recovered-note.txt
cmp recovered-b/src/note.txt project/src/note.txt
cmp recovered-a/data.bin project/data.bin
cmp recovered-b/data.bin project/data.bin
printf 'Demo files: %s\n' "$demo_dir"
```

| Command | Behavior |
| --- | --- |
| `init` | Initialize `.rewind/chunks` and `.rewind/manifests`. |
| `store <file>` | Stream a regular file into storage; print its manifest path. |
| `verify <manifest>` | Validate the recipe, read/hash chunks, check the whole-file digest. |
| `restore <manifest> <output>` | Restore one file without overwriting an existing output. |
| `snapshot <directory>` | Capture the tree; print a unique 64-hex snapshot ID. |
| `watch <directory> [--debounce-ms <n>] [--max-delay-ms <n>]` | Capture an initial baseline, then automatic snapshots from debounced hints; skip identical roots. Ctrl+C stops cleanly. |
| `snapshots` | List validated records oldest first, with IDs, Unix timestamps and quoted source paths. |
| `restore-snapshot <id> <output-directory>` | Verify/reconstruct an entire tree into a new destination. |
| `diff <a> <b>` | Report relative-path additions, deletions and content modifications from A to B. |

Success is exit status 0, including a diff with changes. Errors go to stderr and
return 1. Output write errors also return failure. An empty history prints only
the header; identical snapshots produce no diff rows. Diff columns are tab-separated
`A`/`D`/`M`, `file`/`directory`, and a quoted path. Paths sort by UTF-8 bytes.
Added/deleted directories include descendants. A type replacement produces deletion
then addition at the same path; renames appear as deletion/addition. This is a
report, not an executable patch order.

## Automatic history (Phase 3, Milestone C)

From a directory containing an initialized `.rewind` store:

```sh
/path/to/rewind init
/path/to/rewind watch project --debounce-ms 300 --max-delay-ms 2000
# Edit project files from another terminal. Ctrl+C stops foreground monitoring.
/path/to/rewind snapshots
/path/to/rewind restore-snapshot <snapshot-id> recovered-project
```

The command establishes a baseline immediately, then captures changed directory
contents after debounced batches. Defaults are 300 ms quiet time and 2,000 ms
maximum batch collection delay. Both must be integer milliseconds satisfying
1 <= quiet <= maximum <= 60,000; flags may be given once each in either order.
The maximum prevents event-collection starvation, not OS latency or slow disk I/O.
There is no background daemon or incremental publication. A bounded Milestone D
shadow candidate is measured and compared, while C still validates and publishes.

Output is newline-delimited JSON. `status: "watching"` means event intake is ready;
wait for `snapshot_created` or `skipped_identical` to know baseline work completed:

```json
{"kind":"snapshot_created","snapshot_id":"<64-hex-id>","root_hash":"<64-hex-root>","reason":"baseline","elapsed_ms":123.4,"consistency":"two_pass_validated_observations"}
```

Other records include `batch`, `reconciliation` (`started`/`complete`),
`skipped_identical`, `error` with `fatal`, `cancelled`, and `status: "stopped"`.
A skipped result refers to the latest record for the same canonical source path.
It follows a read-only probe and a second full pass verifying source bytes and
stored chunks/manifests/nodes; it creates neither a duplicate record nor a new
pending marker. Existing pending recovery may still need filesystem writes.
Manual `snapshot` keeps its existing behavior of creating a distinct record.

Every automatic attempt takes the existing exclusive writer lock, recovers any
pending write, and makes two full source traversals with the existing FastCDC,
BLAKE3 and canonical serialization. Changed candidates use the normal object
file-sync/no-clobber/directory-barrier protocol before publishing history. Matching
roots plus identity/size/mode/mtime/ctime checks detect observable source changes.
Observed instability aborts without a record and retries. **These checks are not an
atomic filesystem snapshot:** changes can occur between observations and after the
last check. Quiesce a source when a consistent logical state is required.

One capture worker runs while the controller continues intake. Work/results are
bounded to one job each, with at most one job active and one coalesced future-work
flag. The existing event queue and batch limits remain. Inputs/loss arriving during
capture remain pending; success cannot acknowledge a newer event generation.
Overflow and ambiguous hints require a full scan—there is no event-based shortcut
that decides unchanged contents. A restart always establishes a fresh baseline.
The event-only `Monitor::run` library API remains available for A/B-style logging;
the CLI now uses `run_automatic`.

Three-attempt retry cycles cover detected instability and explicitly classified
transient source errors; after three failures, deferred cycles use backoff from
1 second up to 30 seconds. Corruption, malformed stored metadata, failed storage/
recovery barriers, root replacement, unsupported source entries and backend errors
fail closed with an error. Cancellation checks occur during scanning and writer-lock
waiting. An in-progress fsync or final commit cannot always stop immediately; a
complete record may be committed even if Ctrl+C arrives late. Required pending
markers are left for recovery. Stopped output can indicate pending reconciliation;
shutdown does not guarantee the final edit was captured.

The actual store is excluded by canonical location and device/inode, not basename.
An unrelated `.rewind` stays source data. Confirmed store-only events are filtered
before queueing. A coarse ancestor hint may still cause a scan; identical checks
perform no new store writes, preventing a snapshot feedback loop. Symlinks/special
files and unsupported names/depth are rejected. Store and source paths must stay
trusted/stable. All original writer locking, verification and durability limits
remain, including macOS `sync_all` rather than F_FULLFSYNC.

The callback queue accepts 1,024 notifications, each at most 64 paths / 64 KiB path
bytes. A batch retains at most 4,096 paths, 1,024 rename hints and 1 MiB path bytes;
excess detail requires reconciliation. Stability observations are capped at 100,000
source entries / 16 MiB of absolute path bytes. Native watcher registrations and
metadata/serialization overhead have separate costs. Slow/blocked stdout or a
validation scan can delay progress and shutdown.

macOS uses FSEvents and Linux uses inotify; coalescing, rename detail, permission
constraints and watch/queue limits differ. Unreported native event loss is not
reliably detectable, and network/pseudo filesystems may emit no useful events.
No periodic reconciliation timer or polling fallback is added in C; idle processing
blocks on channels. Only macOS is validated. See [automatic capture details and
measurements](docs/automatic-capture.md) and [native backend limitations](https://docs.rs/notify/latest/notify/).

## Incremental shadow foundation

`watch` additionally emits `kind: "shadow"` diagnostics with candidate construction
and C full-validation timings. A bounded in-memory cache and typed dirty scopes
rebuild affected paths speculatively, including known hard-link aliases. Shadow
roots are never published or used to skip C validation. Cache refresh comes only
from a successful C scan, and a mismatch is reported/discarded. No persistent cache,
format change or new CLI option was added.

See [shadow behavior, limits, differential tests and separate benchmarks](docs/shadow-merkle.md).
Small balanced-tree edit candidates measured about 1.0 / 2.0 / 17.8 ms at 1,000 /
10,000 / 100,000 files. These are candidate-only diagnostics, not end-to-end speedups
or substitutes for C's two-pass validation and durability protocol.

## Storage and verification

FastCDC finds variable-sized chunks (defaults: 16/64/256 KiB min/target/max).
BLAKE3 identifies each chunk. A file manifest records ordered chunk references,
lengths and a whole-file hash. A sorted directory node references file manifests
and child directories. Its canonical bytes define its Merkle hash; changed leaves
change ancestors while unchanged sibling subtrees keep their identities. Snapshot
records separately hold a root hash, unique ID, completion timestamp and source
path, allowing identical captures to share a root but retain independent history.

```text
.rewind/
  chunks/<2 hex>/<62 hex>       raw chunk bytes
  manifests/<64 hex>.json       file manifest v1
  trees/<2 hex>/<62 hex>        canonical directory-node JSON v1
  snapshots/<snapshot-id>.json  history record v1
```

Trees/history directories are created lazily. Stored-object reads are bounded and
require regular, non-symlink files. Writes use synced same-filesystem temporaries,
no-clobber publication, and Unix directory syncs. Committed objects are not replaced;
corrupt duplicates are rejected rather than silently repaired. Chunks, manifests
and Merkle nodes are verified before creating a temporary on a cache hit: chunks
must match BLAKE3, manifests must match the validated bytes being saved, and nodes
must pass their identity/schema checks. Only NotFound proceeds to a new write;
other read/validation errors abort. A no-clobber race winner is verified too. Snapshot records
are published only after their referenced graph has been stored.

Single-file restore stages a file; snapshot restore stages a private sibling tree.
It checks node hashes/schemas, manifest identities/lengths, chunk hashes and complete
file hashes before no-replace publication. Existing destinations—including empty
directories and dangling symlinks—are refused. The parent must exist. Snapshot
output must be outside the store and the recorded source tree. Metadata names cannot
request traversal or symlinks; exclusive creation detects destination name collisions.
The original source need not exist. Restored directories/files use 0700/0600 modes,
subject to umask; original permissions and metadata are not preserved.

**Diff is not an integrity scan.** It reads/verifies roots and visited metadata,
compares stored whole-file hashes, and skips equal subtrees and file references.
It never reads chunks. Corruption in skipped data can therefore remain undetected.
Full restoration performs stronger verification. Listing checks records only.

## Limits and platform support

- Capture is Unix-only; snapshot restore is enabled on macOS/Linux and requires
  filesystem support for atomic no-replace directory rename. Only macOS has been
  tested. File commands and metadata/diff APIs use portable interfaces where
  possible; Windows/other platforms have not been validated.
- Writers coordinate through a permanent advisory `writer.lock` file. Capture holds
  it through recovery and commit; standalone mutations use the same lock. Older
  binaries/external tools do not honor it. There is no concurrent GC protocol.
  Store contents and the destination parent must be trusted/stable. Rejecting
  symlink objects is not a complete defense against hostile ancestor/mount changes.
  User-supplied output-parent symlinks are resolved before staging.
- Capture is a full traversal, not O(changed files). Concurrent edits can yield
  bytes observed at different times; disappearing entries/read failures abort.
  Quiesce the source if a consistent logical state is needed.
- The actual nested store is excluded by directory identity. Unrelated directories
  named `.rewind` remain source data. Capturing the store or its descendants is
  rejected. Symlinks (including a symlink root), FIFOs, sockets and devices are
  rejected; mounts may be traversed. Hard links become independent restored files.
- Names must be UTF-8 portable single components; paths and filesystems have their
  own additional limits. Original timestamps, permissions, ownership, ACLs, xattrs,
  sparse extents and hard-link identity are unsupported. No compression/encryption.
- Traversed directory depth is limited to 256, chunks to 16 MiB, manifests to
  64 MiB, directory nodes to 16 MiB, and snapshot records to 64 KiB. Chunk buffering
  is bounded; manifest/tree/diff metadata memory grows with the represented data.
- Snapshot IDs are opaque history IDs, not record checksums. Record schemas are
  validated, but plausible metadata edits are not authenticated.
- Sync failure may be reported after an object or complete output is visible;
  an error does not guarantee rollback. Interrupted capture may leave unreferenced
  objects/tempfiles; restore may leave `.rewind-restore-*` stages. There is no
  scavenger or proven power-loss guarantee. Failed writes leave a durable pending
  marker; the next writer repairs surviving namespace links before reuse. A failed
  recovery barrier retains the marker and blocks writes. Never remove `writer.lock`
  while in use. macOS uses `sync_all`, not F_FULLFSYNC.
- Automatic history runs in the foreground. A daemon, incremental updates,
  retention, GC, snapshot rename detection and FUSE mounts do not exist yet.

## Evidence and next steps

The current suite has 119 passing tests, zero failures/ignored tests on macOS, including
five property suites (92 cases) and a complete CLI workflow. Small release-build
measurements record elapsed time, CPU time, peak RSS and chunk growth, without
comparisons to other tools or universal speed claims.

See [validation and measurements](docs/validation.md), [raw measurements](docs/measurements-phase2.json),
and [architecture, review findings and Phase 3 milestones](docs/architecture.md).
The reproducible measurement harness is [scripts/measure_phase2.py](scripts/measure_phase2.py).
Phase 3 Milestones A–C and the approved D shadow foundation are implemented.
Incremental/event-assisted publication remains disabled; further work requires approval.

Verified-existing-object reuse retains BLAKE3 verification. Capture-only directory
sync batching now uses an object durability barrier before snapshot publication,
with exclusive writer coordination and durable pending-marker recovery. Standalone
objects retain immediate durability. See [capture batching and measured results](docs/capture-batching.md)
and [earlier verified reuse results](docs/verified-reuse.md). Persistence concurrency
and Phase 3 Milestones D–E remain unimplemented.
