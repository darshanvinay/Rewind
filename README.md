# Rewind

Rewind is a local-first file-history engine under construction. It aims to make
previous file contents recoverable without manual backup copies or Git commits.
Phase 1 provides the storage foundation: streaming FastCDC chunking, immutable
BLAKE3-addressed chunks, deduplication, versioned JSON file manifests, verified
reconstruction, and a minimal CLI. Capture is manual in this phase.

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
* No watchers, directory trees, snapshots, retention, FUSE, or benchmarks yet.
  No performance advantage is claimed.

Tests use isolated temporary directories and cover arbitrary bytes, large binary
input, empty/repeated/text input, deduplication, corrupt/missing chunks, malformed
manifests, stale temporary writes, output errors, and atomic failed restoration.

Validation: Rust 1.99.1 on macOS aarch64; formatting, Clippy with warnings denied,
and all 7 integration tests passed (including 32 property cases and a CLI BLAKE3
round trip). See [validation details](docs/validation.md).
