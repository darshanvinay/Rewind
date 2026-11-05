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
