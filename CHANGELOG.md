# Changelog

## 1.0.0 — release candidate

- Streaming FastCDC/BLAKE3 storage, immutable manifests and directory Merkle trees.
- Manual and strict automatic snapshots, bounded debounce and event-loss recovery.
- Exclusive storage writers, verified object reuse, capture directory-sync batching,
  durable pending-marker recovery and no-clobber snapshot publication.
- Verified historical restoration, snapshot history and metadata diffs.
- Bounded incremental shadow candidates and differential diagnostics; fast
  publication is unfinished and explicitly rejected.
- Explicit strict mode and configurable periodic full reconciliation.
- History record v2 source-validation provenance; legacy v1 remains readable.
- Help/version commands, source installation and a verified demonstration.

Validated on macOS/local APFS only. No Linux or actual power-loss validation.
This candidate has not been tagged, pushed, or published.
