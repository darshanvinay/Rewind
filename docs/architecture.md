# Architecture

## Scope and evolution

The library owns chunking, identities, immutable object storage, and file recipes.
The CLI only parses commands and connects these APIs. Phase 1 captures individual
regular files manually. Planned later layers are filesystem event ingestion,
immutable directory Merkle trees, snapshot roots, diffs and selective restore,
transactional snapshot commits, retention and coordinated mark-and-sweep GC.
Directory nodes and manual snapshot capture are implemented through Phase 2
Milestone B. Snapshot listing/restoration, diffing and later layers remain
unimplemented.

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

## Directory Merkle nodes (Milestone A)

The new `tree` module adds immutable recipes above Phase 1:

```
snapshot record (ID, source path, timestamp)
  -> root directory-node hash
     -> child directory-node hash
     -> file-manifest hash -> ordered chunk hashes
```

A node contains `version: 1`, `object: "rewind-directory"`, and an `entries` array.
Each entry has `name` followed by `target`. A file target has `kind: "file"`,
`manifest` (the hash of exactly the bytes saved by `FileManifest::save`), and
`length` (logical bytes). A directory target has `kind: "directory"` and `tree`.
These typed references prevent confusing a raw chunk with a manifest or directory.
Only file bytes and hierarchy are represented; there are no permissions, times,
ownership, ACLs, xattrs, sparse extent maps or hard-link relationships. Restored
files will be ordinary independent files; filesystem defaults determine metadata.

Version 1 canonical encoding is compact `serde_json` UTF-8 with fields in the
schema order above, entries sorted by exact UTF-8 name bytes, no insignificant
whitespace or final newline, and the serializer's standard JSON escaping. Integer
values use unsigned decimal JSON numbers. Canonical-byte tests pin this contract;
changing it requires a new format version. Input must match reserialization
exactly, so alternate key order, duplicate keys, alternate escapes and whitespace
cannot introduce multiple stored encodings. Unknown fields/types/versions,
unsorted or duplicate names, and invalid hashes are rejected. A node is limited
to 16 MiB serialized bytes. Nodes use no unordered maps or timestamps.

`DirectoryNode::new` takes ownership of a vector and sorts it once. Fields stay
private, and `entries()` borrows a read-only slice rather than cloning the vector.
The `EntryTarget` Rust enum carries different fields for each child kind. Errors
use the existing `Result` type so callers must handle failed validation or I/O.
Node parsing uses a private wire structure, then validates it before returning a
node. Node hashing and saving revalidate all entries as defense in depth.

Nodes live at `.rewind/trees/<first 2 hex>/<remaining 62 hex>`. The namespace is
created lazily on first save, so old stores remain compatible. Saving reuses
`storage::publish`: synced temporary file, no-clobber publication, Unix directory
syncs. Existing nodes are hash- and schema-verified before accepting deduplication.
Loading reads at most 16 MiB plus one byte, checks the requested BLAKE3 identity,
and validates the canonical schema. Phase 1's platform durability and single-writer
limits still apply. `DirectoryNode::save` does not verify descendants: this permits
bottom-up construction. Milestone B capture ensures its reachable graph is stored
before publishing a snapshot record.

If a leaf changes, its manifest hash changes, which changes its parent node and
each ancestor up to the root. Sibling subtrees retain their identities and reuse
the same stored nodes. A future diff can compare hashes to skip equal subtrees.
This is structural reuse, not incremental filesystem scanning: Milestone B
traverses and reads the complete source tree. Snapshot IDs and capture times
will belong in separate records, so identical states can have equal root hashes
while captures remain distinct history entries.

### Names and filesystem policy

Names are exact UTF-8 single components, at most 255 bytes. Validation rejects
empty names, dot/dot-dot, both slash types, control characters (including NUL),
Windows drive/alternate-stream syntax, reserved punctuation, trailing dots/spaces,
and Windows device names. Non-UTF-8 filenames will be unsupported. No case folding
or Unicode normalization is applied: future restoration must detect destination
collisions and fail without overwriting. Valid names alone do not guarantee a
name is representable on every destination filesystem.

The Phase 2 capture policy is to reject symlinks (including a symlink root), FIFOs,
sockets and devices, rather than following or silently omitting them. Milestone A
models only files and directories. Milestone B now implements descriptor-relative
traversal and the capture policy described below. Future restore must verify referenced manifest identities and lengths,
verify nodes recursively, and use fresh destination paths without following
symlinks. This milestone does not claim completed snapshot safety or crash safety.

## Manual snapshot creation (Milestone B)

`rewind snapshot <directory>` opens the current working directory's `.rewind`
store. `snapshot::create_snapshot` performs a complete directory traversal:

1. Open the source directory, resolve its canonical absolute UTF-8 display path,
   and check that its device/inode still match the opened directory handle.
   Reject a source equal to or inside the canonical store path.
2. Enumerate immediate child names through the open directory handle and sort
   them. Inspect each entry without following symlinks. Open regular files and
   directories relative to that handle with `NOFOLLOW`; directory opens also
   require `DIRECTORY`. File opens use `NONBLOCK` so replacement with a FIFO does
   not hang capture; the opened file must still be a regular file.
3. Exclude directories matching the actual store's device/inode, regardless of
   their name. Other directories named `.rewind` remain source data. Store parents
   remain in the tree even if excluding the store leaves them empty.
4. Pass opened files to the same chunking/storage pipeline used by `store_file`.
   Save each manifest and use its hash filename and length in the parent entry.
   Recurse through subdirectories, then persist each parent node bottom-up.
5. Only after the root node has been stored, create a version 1 snapshot record
   and publish it with the existing synced no-clobber helper. The record's filename
   is `<id>.json` inside `.rewind/snapshots/`.

The Phase 1 public file API is preserved. A small crate-private `store_open_file`
helper allows secure traversal to reuse that pipeline without reopening a path.
Rust `File` values own their descriptors; ordinary scope exit closes them. Recursive
calls borrow the open parent with `&File`, keeping it alive until its children are
done. `rustix` provides safe descriptor-relative filesystem APIs with no unsafe
Rust in Rewind. Snapshot capture is initially Unix-only (validated on macOS);
non-Unix returns unsupported. Other Unix platforms have not been validated.

Snapshot record fields, in order, are `version`, `id`, `root_hash`, `source_path`,
`created_unix_seconds`, and `created_nanoseconds`. The timestamp is taken after
traversal and before publication, not at capture start. The path is display
metadata, never a restoration destination. IDs are opaque 64-character lowercase
BLAKE3 digests of an ID-specific domain, a uniquely allocated 32-random-character
tempfile basename, root hash, and completion timestamp. They are not content
identities for the record bytes. Thus two captures of identical content share
root nodes but receive distinct history IDs, without relying on clock precision.
A collision refuses publication rather than overwriting a record. Listing and
validated record loading are deferred to Milestone C. Only final `<id>.json`
records will count as history; `.pending-*` and publication tempfiles are not
records and may survive a crash.

### Consistency and limits

The symlink root itself (including a trailing slash or `/.`) is rejected; ancestor
symlinks in the user-supplied source path can resolve normally before the root is
pinned. Inside that root, traversal never follows symlinks: child opens are tied
to already open directories. Symlinks, dangling links, sockets, FIFOs and devices
cause failure instead of being omitted. The trusted store still uses Phase 1's
path-based persistence; hostile mutation of the store is outside the contract.
Mounts are not excluded and mount points may be traversed. Hard links are captured
as independent files. Root depth is zero; directories deeper than 256 levels are
rejected to bound recursion and directory descriptors. Name/node/manifest size
limits from earlier milestones apply. Directory-name vectors and ancestor entry
vectors are held during traversal, so metadata memory grows with tree width/depth.

Source files and directories may change during capture. A disappearing entry or
failed read aborts; other edits may produce a valid graph combining bytes observed
at different times. Renaming an opened directory does not change its handle.
These protections prevent symlink redirection but do not supply a filesystem
transaction or defend against all malicious mount/device changes. Quiesce the
source for a consistent logical state. Every capture still scans and reads all
files: deduplicating stored objects is not O(changed files) incremental capture.

All referenced objects are written/synced before record publication, and duplicate
objects encountered during capture are verified by the existing APIs. This is a
single-writer publication ordering guarantee, not a tested power-loss guarantee.
Before record publication, failure may leak unreferenced objects but exposes no
new history record. During/after publication, a directory sync or nonce-tempfile
cleanup failure may report an error even though the record already exists. The
same filesystem/platform durability limits as Phase 1 apply. No snapshot record
index, cross-object transaction, garbage collection, watchers, or retention policy
has been added.
