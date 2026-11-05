# Architecture

## Scope and evolution

The library owns chunking, identities, immutable object storage, and file recipes.
The CLI only parses commands and connects these APIs. Phase 1 captures individual
regular files manually. Planned later layers are filesystem event ingestion,
immutable directory Merkle trees, snapshot roots, diffs and selective restore,
transactional snapshot commits, retention and coordinated mark-and-sweep GC.
Those layers are deliberately absent here.

## Data flow and chunking

`Read -> FastCDC 2020 -> BLAKE3 -> ChunkStore -> FileManifest v1`

The maintained `fastcdc` crate avoids implementing the subtle boundary algorithm
ourselves. Its v2020 streaming iterator reads into a maximum-chunk-sized buffer
and returns one owned chunk at a time. There is some per-chunk allocation; no
whole-file buffer is retained. Defaults are 16/64/256 KiB. Library callers can
configure sizes within crate bounds and our 16 MiB safety cap. The chosen algorithm
and parameters are persisted for reproducibility. EOF can yield a short final
chunk; empty files have no chunks. Reconstruction concatenates verified chunks,
checks lengths and checks the whole-file digest (including order). Manifest memory
is O(number of chunks); streaming data memory is O(max chunk size).

## Identity and layout

BLAKE3's full 256-bit digest is encoded as 64 lowercase hexadecimal characters.
Gear hashes used for boundary selection are never storage identities. Hashes are
validated before becoming path components.

```
.rewind/
  chunks/ab/<remaining 62 hex characters>
  manifests/<BLAKE3 of serialized manifest bytes>.json
```

A two-digit shard limits directory fanout. Objects are raw, uncompressed bytes.
Manifest v1 JSON rejects unknown fields, unsupported versions/algorithms, invalid
hashes, zero or excessive lengths, and inconsistent total lengths. Its whole-file
hash detects incorrect ordering. JSON paths supplied to verify/restore need not
have an identity filename: verification checks their recipe and content, not
provenance or authenticity. Future versions require explicit migration/dispatch.

## Persistence and failures

For each new object: create a uniquely named tempfile in the destination directory,
write all bytes, flush, `sync_all`, and use `tempfile::persist_noclobber`. This API
publishes without replacing an existing path (on Unix via hard-link semantics),
unlike ordinary rename. On an existing chunk, read and hash it before accepting
successful deduplication; existing manifest bytes must match exactly.

After publication sync the parent directory on Unix. Newly created shard and store
directories and their parents are also synced. Filesystem errors propagate. A sync
failure can mean publication already happened: callers may retry, but must not
interpret an error as proof of rollback. Hardware/filesystem durability depends on
honoring sync; network filesystems are outside the guarantee. Non-Unix has file
sync and no-clobber publication, but no portable directory-sync guarantee. macOS
`sync_all` does not request Apple's stronger F_FULLFSYNC drive-cache barrier.
These are filesystem-level durability measures, not a universal power-loss promise.

Named temporary files clean up on ordinary failure via RAII. A process crash can
leave temporary files, including a second hard link to a published object. Names
outside the deterministic object paths are never treated as committed objects.
They are left alone to avoid racing active operations. No temp scavenger, concurrent
GC, cross-object transaction, or fault-injection crash harness exists yet.

A manifest is published only after all its chunks. A crash during store can leak
unreferenced chunks but cannot publish a recipe before its writes have completed.
There is no transactional snapshot root. Restore stages the complete result beside
its destination, verifies it, syncs it, publishes without replacement, and syncs
the directory. Generic `reconstruct` streams to a caller's writer and may emit a
verified prefix before a later error; filesystem callers should use `restore_file`.

## Concurrency and trust

Single writer is a caller contract, not an enforced lock. Future GC must coordinate
publication, reader liveness and reachability before deletion; this API does not
promise safety with concurrent removal. Source files can change during capture:
the manifest describes the bytes observed, not a point-in-time source snapshot.
The store is trusted local storage; this phase does not defend against symlink
races or malicious filesystem changes. Corruption checks are not authentication.

## Phase 2 proposal

Introduce versioned immutable directory nodes with sorted names and references to
file manifests, child nodes and explicitly specified metadata. Hash canonical node
encodings. First build full directory capture with a defined symlink policy and
stable deterministic roots; then reuse unchanged subtrees incrementally. Publish
snapshot records only after all reachable objects, with an atomic durable root
commit protocol. Test traversal safety, deterministic roots, subtree reuse, diffs,
selective restoration and crash boundaries before adding watchers or retention.
