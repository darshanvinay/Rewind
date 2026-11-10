# Rewind

Rewind is a local-first file-history engine written in Rust. It preserves file
contents in immutable, deduplicated storage so earlier manually captured versions
can be recovered without Git commits or separate full copies.

**Phase 2 is complete:** streaming FastCDC chunking, BLAKE3 chunk identities,
versioned file manifests, directory Merkle trees, manual snapshots, history,
verified file/directory restoration, and Merkle-aware diffs. **Phase 3 Milestone A**
adds native recursive event logging with `watch`. It does not create automatic
snapshots yet. Manual capture still reads the full source tree on every invocation.

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
| `watch <directory>` | Recursively log normalized filesystem events as JSON lines; Ctrl+C stops cleanly. No automatic snapshots. |
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

## Watching a directory (Phase 3, Milestone A)

From a directory with an initialized `.rewind` store:

```sh
/path/to/rewind init
/path/to/rewind watch project
# Edit project files from another terminal. Press Ctrl+C to stop.
```

For example, in the walkthrough's shell use `"$REWIND_BIN" watch project`.
The first JSON line reports `status: "watching"`, the canonical root and backend.
Events contain `kind` and root-relative `paths`; a final line reports `status: "stopped"`:

```json
{"kind":"modify","paths":["src/note.txt"]}
```

Kinds are `create`, `modify`, `remove`, `rename`, `rename-from`, `rename-to`, `any`
and `other`. Paths retain order for a paired rename; native backends can instead
report separate rename halves or coarse hints. Root-directory hints use `"."`.
These are raw normalized notifications, **not logical saves**, an ordered replay
journal, or proof of a stable filesystem state. Duplicates/coalescing are expected.
Read/access-only events are ignored. Output is flushed after each JSON line.

The actual store is excluded by canonical location and directory identity, even
when named differently through the library API. An unrelated directory named
`.rewind` remains source data. Root symlinks, symlinks/special files found during
startup or later validation, unsupported names and excessive depth are rejected.
Startup registration precedes validation so events during validation are queued.
The source/store must stay at stable locations; detected movement/replacement,
backend errors, unresolvable paths, reported event loss or queue overflow stop the
command with an explicit error. Restart after resolving the cause. There is no
automatic rescan/recovery loop yet, nor protection against unreported OS event loss.

The application queue holds at most 1,024 notifications, with at most 64 paths and
64 KiB of path bytes per notification. It never blocks the native callback on a full
queue or silently treats lost events as complete history. While idle the consumer
blocks on a channel, with no periodic polling. Ctrl+C wakes it and stops monitoring;
queued events are not drained on shutdown. Slow or blocked stdout can delay shutdown
and overload the queue. Native library/kernel buffering has separate limits.

macOS uses FSEvents; Linux uses inotify. Delivery/coalescing and rename detail differ;
Linux recursively installs per-directory watches and is subject to watch/queue limits
and races when new directories arrive. FSEvents can emit coarse/drop/rescan hints and
is subject to macOS permissions. No polling fallback is selected. Network/pseudo
filesystems may not emit useful events. Only macOS has been tested. See
[the native notify documentation](https://docs.rs/notify/latest/notify/) and
[the watcher architecture](docs/architecture.md#phase-3-milestone-a--event-logging).

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
- Event logging is available, but automatic snapshots, debouncing, a daemon,
  retention, GC, snapshot rename detection and FUSE mounts do not exist yet.

## Evidence and next steps

The current suite has 77 passing tests, zero failures/ignored tests on macOS, including
five property suites (92 cases) and a complete CLI workflow. Small release-build
measurements record elapsed time, CPU time, peak RSS and chunk growth, without
comparisons to other tools or universal speed claims.

See [validation and measurements](docs/validation.md), [raw measurements](docs/measurements-phase2.json),
and [architecture, review findings and Phase 3 milestones](docs/architecture.md).
The reproducible measurement harness is [scripts/measure_phase2.py](scripts/measure_phase2.py).
Phase 3 Milestone A is implemented. Milestones B–E require further approval; no
automatic capture or incremental tracking has been implemented.

Verified-existing-object reuse retains BLAKE3 verification. Capture-only directory
sync batching now uses an object durability barrier before snapshot publication,
with exclusive writer coordination and durable pending-marker recovery. Standalone
objects retain immediate durability. See [capture batching and measured results](docs/capture-batching.md)
and [earlier verified reuse results](docs/verified-reuse.md). Persistence concurrency
and Phase 3 Milestones B–E remain unimplemented.
