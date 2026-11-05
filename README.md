# Rewind

Rewind is a local-first file-history engine under construction. It aims to make
previous file contents recoverable without manual backup copies or Git commits.
Phase 1 provides the storage foundation: streaming FastCDC chunking, immutable
BLAKE3-addressed chunks, deduplication, versioned JSON file manifests, verified
reconstruction, and a minimal CLI. Phase 2 now adds directory Merkle nodes and manual snapshot
creation; snapshot listing, restoration and diffing await later milestones.

## Build and use

Install stable Rust with Cargo (edition 2024; validated toolchain recorded below).

```sh
cargo build --release
cargo test
# Run from the directory in which you want .rewind:
/path/to/rewind/target/release/rewind init
/path/to/rewind/target/release/rewind store example.bin
# store prints a path such as .rewind/manifests/<hash>.json
/path/to/rewind/target/release/rewind verify .rewind/manifests/<hash>.json
/path/to/rewind/target/release/rewind restore .rewind/manifests/<hash>.json recovered.bin
# Unix: capture a directory; prints a unique snapshot ID
/path/to/rewind/target/release/rewind snapshot ./project
```

Commands use `.rewind` in the current working directory. Quote paths containing
spaces. Restore refuses to overwrite an existing destination; its parent directory
must exist. Errors go to stderr with exit status 1; success returns 0. Store prints
only the manifest path on stdout. Chunk sizes are configurable through the library's
`ChunkSizes` API (defaults: 16 KiB minimum, 64 KiB target, 256 KiB maximum).

## Guarantees and limits

* Single writer only; callers must serialize mutations. No locking or garbage
  collection is implemented. Reads assume committed objects are immutable.
* Every retrieved chunk is hashed, including existing chunks encountered on a
  duplicate write. A corrupt object is rejected, never silently repaired.
* Temporary files are synced and published without replacement. Unix directories
  are synced; non-Unix directory durability is weaker. See [architecture](docs/architecture.md).
* Restore uses a synced temporary file and no-clobber publication. A failed
  integrity check does not create the destination.
* Manifest v1 holds ordered hashes, lengths, chunking parameters, total length,
  and a whole-file hash. It is limited to 64 MiB; chunk data to 16 MiB per chunk.
  Chunking memory is bounded by chunk size, but manifest metadata grows with the
  number of chunks. No compression, encryption, metadata preservation, source
  consistency during concurrent edits, or protection against hostile store mutation.
* Crash leftovers are ignored, not automatically deleted. Interrupted ingestion
  may leave unreferenced chunks. Do not delete temporary objects during a writer's run.
* Snapshot capture currently requires Unix. It rejects symlinks, special files,
  non-UTF-8 names/paths, unsupported portable names, and directory depth beyond 256.
  The actual store is excluded if nested inside the source; other `.rewind`
  directories are ordinary source data. Capturing the store itself is rejected.
* Capture traverses and reads the full source tree each time. Content identities
  deduplicate unchanged subtrees; this does not provide incremental scanning or
  point-in-time consistency with concurrent edits. No performance advantage is claimed.
* No snapshot listing/restoration/diff commands, watchers, retention, FUSE, or
  benchmarks yet. Single-file restore remains available.

Tests use isolated temporary directories and cover arbitrary bytes, large binary
input, empty/repeated/text input, deduplication, corrupt/missing chunks, malformed
manifests, stale temporary writes, output errors, and atomic failed restoration.

Validation: Rust 1.99.1 on macOS aarch64; formatting, Clippy with warnings denied,
and all 23 tests passed (22 integration tests and one unit test, including three
property suites totaling 72 cases). See [validation details](docs/validation.md).

## Phase 2 progress: Milestone A

`tree::DirectoryNode` represents an immutable directory's immediate children.
`DirectoryEntry` pairs a validated name with an `EntryTarget`: either a Phase 1
file-manifest hash and file length, or another directory-node hash. Constructors
sort entries; canonical versioned JSON defines the BLAKE3 identity. Empty nodes
represent empty directories. Nodes are stored under `trees/<2 hex>/<62 hex>` using
the existing no-clobber persistence helper and verified when loaded.

The node API is a library foundation. References may be unresolved: saving a node
does not recursively validate or persist descendants. The Milestone B capture API
below constructs and persists the graph bottom-up. See [the node format and policies](docs/architecture.md#directory-merkle-nodes-milestone-a).

## Phase 2 progress: Milestone B

`rewind snapshot <directory>` uses `snapshot::create_snapshot` to store files
through Phase 1, build directory nodes bottom-up, then publish a versioned record
in `.rewind/snapshots/<id>.json`. It prints only the 64-digit snapshot ID. The
record holds the root hash, canonical absolute source path and Unix completion
timestamp (seconds plus fractional nanoseconds). Identical source states yield
identical roots but distinct IDs. Capturing a source does not modify its files;
when the store is nested in the source, only the excluded store receives writes.

Traversal opens children relative to directory handles with `NOFOLLOW` on Unix,
preventing descendant symlinks from redirecting capture. No unsafe Rust is added.
`rustix` supplies safe OS interfaces. This initial implementation returns an
explicit unsupported error on non-Unix platforms; the Phase 1 commands remain
unchanged. Permissions, times, ownership and hard-link identity are not preserved.

Records become visible only after their referenced content has been stored.
Failed capture can leave unreferenced immutable objects or temporary files.
A publication sync/cleanup error may occur after the record is already visible,
so an error is not a rollback guarantee. See [consistency details](docs/architecture.md#manual-snapshot-creation-milestone-b).
Snapshot listing/restoration will be Milestone C; diffing will be Milestone D.
