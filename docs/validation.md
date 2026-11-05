# Phase 1 validation

Validated on macOS aarch64 using Rust 1.99.1, edition 2024. The environment had
no installed Rust toolchain; a stable minimal toolchain with rustfmt and Clippy
was installed into `/tmp/rewind-cargo` and `/tmp/rewind-rustup`, without modifying
shell startup files. Dependencies are pinned in Cargo.lock.

* `cargo fmt --check`: passed.
* `cargo clippy --all-targets -- -D warnings`: passed.
* `cargo test`: 7 integration tests passed, including 32 proptest cases.
* `cli_smoke_real_file_blake3`: invokes the built CLI in an isolated temp directory,
  initializes, stores a real Cargo.lock fixture, verifies, restores, compares bytes,
  and independently hashes original and restored bytes with BLAKE3.
  Both digests: `d68967eb2e163bd3c224d34c928b5159ce21c6391f99a17aa5d82968f6f5bcc9`.

The suite covers normal failure paths and simulates a stale partial temporary
write. It does not simulate power loss or certify crash consistency on other
filesystems/platforms. No benchmark results are claimed.

## Phase 2 Milestone A

Validated using the same isolated Rust 1.99.1 toolchain on macOS aarch64:

* `cargo fmt --check`: passed.
* `cargo clippy --all-targets -- -D warnings`: passed.
* `cargo test`: 14 integration tests passed (7 existing storage tests and 7 tree
  tests), including 32 byte-round-trip cases and 32 tree-ordering property cases.
* The existing Phase 1 CLI store/verify/restore smoke test still passes.

New tests cover exact canonical bytes, empty nodes, sorting, duplicate/unsafe
names, invalid references, unsupported/malformed/noncanonical metadata, node size
limits, missing/corrupt stored nodes, no-clobber deduplication, subtree reuse,
ancestor hash changes, and references to real Phase 1 file manifests.

The complete snapshot CLI smoke test is deferred until Milestones B–D add those
commands. No filesystem traversal, snapshot records, or snapshot restoration is
implemented in Milestone A.

## Phase 2 Milestone B

Validated with Rust 1.99.1 on macOS aarch64:

* `cargo fmt --check`: passed.
* `cargo clippy --all-targets -- -D warnings`: passed.
* `cargo test`: 23 passed (one unit test, eight snapshot integration tests,
  seven Phase 1 integration tests, seven Merkle-node integration tests).
  Property suites run 32 byte-round-trip cases, 32 tree-ordering cases, and
  eight arbitrary file-map capture cases.
* Separate CLI smoke run: initialized a store, captured a nested directory with
  binary data and an empty directory, changed one file and added another, then
  captured again. Both committed graphs were independently walked and their
  reconstructed bytes compared against the expected historical states. Every
  referenced file manifest passed the existing CLI `verify` command.
  Snapshot A: `7be4e6de2e1f3a5d7a21a27ef7f526fa0ab5e642e47eb7b02ad207f952d4da00`.
  Snapshot B: `3219e3db08dbc51861a74da49c4cd95ea5830d60ae5723a0750ba93e6032a91e`.
  These test snapshots lived only in an isolated temporary directory.

Coverage includes unique records for identical roots, complete nested graphs,
empty directories, exact binary bytes, stable subtree references, actual nested
store exclusion (including a custom store name), preservation of unrelated
`.rewind` directories, root/store rejection, symlink roots and descendants,
dangling symlinks, special files, malformed names, missing source paths, corrupt
chunk rejection without a new record, record publication errors and depth limits.

The initial sandboxed run could not create the Unix socket fixture; the full
passing run used the approved execution environment with socket creation enabled.
This macOS filesystem rejects invalid UTF-8 filename bytes with EILSEQ before
capture can inspect them, so the parser rejection is tested directly with a
non-UTF-8 C string rather than relying on an impossible filesystem fixture.
No tests were silently skipped to obtain a passing result. Only macOS was tested;
non-Unix capture deliberately returns unsupported. Power-loss fault injection
and the complete snapshot restore/diff CLI smoke test remain outside Milestone B.
