# Rewind

Rewind is a local-first file-history engine written in Rust. It preserves file
contents in immutable, deduplicated storage so earlier manually captured versions
can be recovered without Git commits or separate full copies.

**Phase 2 is complete:** streaming FastCDC chunking, BLAKE3 chunk identities,
versioned file manifests, directory Merkle trees, manual snapshots, history,
verified file/directory restoration, and Merkle-aware diffs. There is no automatic
file monitoring yet. Capture reads the full source tree on every invocation.

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
- Single writer is a caller contract; there is no lock or concurrent GC protocol.
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
  scavenger or power-loss guarantee. macOS uses `sync_all`, not F_FULLFSYNC.
- No watchers, daemon, retention, GC, rename detection, or FUSE mounts exist yet.

## Evidence and next steps

The current suite has 57 passing tests, zero failures/ignored tests on macOS, including
five property suites (92 cases) and a complete CLI workflow. Small release-build
measurements record elapsed time, CPU time, peak RSS and chunk growth, without
comparisons to other tools or universal speed claims.

See [validation and measurements](docs/validation.md), [raw measurements](docs/measurements-phase2.json),
and [architecture, review findings and proposed Phase 3](docs/architecture.md).
The reproducible measurement harness is [scripts/measure_phase2.py](scripts/measure_phase2.py).
Phase 3 is planned only; no monitoring or incremental tracking has been implemented.

Verified-existing-object reuse is implemented without changing new-object sync
policy. See [the optimization results](docs/verified-reuse.md) for safety regression
tests, before/after timings and operation counts. Directory-sync batching and
persistence concurrency remain unimplemented proposals.
