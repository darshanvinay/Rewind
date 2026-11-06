# Rewind architecture — Phase 2

## Components and data flow

The storage engine is independent of the CLI. `main.rs` parses commands and handles
stdout/stderr; library APIs own capture, validation, persistence and reconstruction.

```text
regular file -> FastCDC -> BLAKE3 chunks -> ordered file manifest
                                            |
                   sorted directory entries + child directory hashes
                                            |
                                  directory Merkle root
                                            |
                     snapshot record (ID, timestamp, source path)
```

| Module | Responsibility |
| --- | --- |
| `chunking` | Validate configurable min/target/max chunk sizes. |
| `hashing` | Full BLAKE3 digest and canonical lowercase 64-hex identity validation. |
| `storage` | Bounded regular-object reads, durable directories, no-clobber publication, chunk storage. |
| `manifest` | File recipes, ingestion, identity loading, reconstruction, file verify/restore. |
| `tree` | Sorted directory nodes, canonical bytes, validation and content-addressed persistence. |
| `snapshot` | Descriptor-relative capture and validated history loading/listing. |
| `snapshot_restore` | Private staging-tree reconstruction and atomic no-replace publication. |
| `diff` | Merkle pruning and relative-path content-change reports. |
| `error` | Structured `Result` errors for I/O, metadata, corruption and chunking. |

The public file APIs from Phase 1 remain. Capture passes opened files to a small
crate-private ingestion helper, avoiding a path reopen after a no-follow lookup.
Private fields/read-only getters protect directory nodes and snapshot records;
private wire structs are validated before returning them. `EntryTarget` is an enum
so a file reference cannot accidentally carry directory-only fields. Borrowed
`&File` handles keep parent directories alive during recursive operations.
There is no unsafe Rust in Rewind and no new runtime framework or storage database.

## Chunking and identities

The maintained `fastcdc` 5 crate supplies the streaming FastCDC 2020 implementation;
Rewind does not reimplement its boundary algorithm. Default sizes are 16 KiB minimum,
64 KiB target and 256 KiB maximum. Sizes must satisfy crate bounds, be even, and
obey min <= target <= max with a 16 MiB cap. The current crate defaults use
normalization level 1 and seed 0. Dependencies are pinned by `Cargo.lock`.

The iterator retains a maximum-sized buffer and yields one owned chunk at a time.
There is per-chunk allocation/copying, not a whole-file buffer. EOF may yield a
short final chunk; empty files have no chunks. Only the full 256-bit BLAKE3 digest
is an object identity; FastCDC's Gear hash is used only for boundary selection.

## On-disk formats

Paths are relative to the initialized store root. `init` creates `chunks` and
`manifests`; other namespaces are lazy. There is no separate repository-format
marker or migration system yet.

```text
.rewind/
  chunks/ab/<remaining 62 hex>
  manifests/<64 hex>.json
  trees/ab/<remaining 62 hex>
  snapshots/<opaque 64-hex ID>.json
```

- **Chunks:** raw immutable bytes without a version header, keyed by BLAKE3.
- **File manifest v1:** compact JSON with `version`, `algorithm` (currently
  `fastcdc-2020-blake3`), `chunking` (`min`, `avg`, `max`), `length`, `file_hash`,
  and ordered `chunks` (`hash`, `length`). Identity is BLAKE3 of saved JSON bytes.
  Unsupported versions/algorithms, unknown/duplicate fields, invalid hashes,
  zero/excessive chunk lengths and inconsistent totals are rejected. Empty-file
  whole hashes are checked during reconstruction just like every other file.
- **Directory node v1:** `version`, `object: "rewind-directory"`, and `entries`.
  Each entry has `name` and `target`; file targets contain `kind: "file"`,
  `manifest`, `length`; directories contain `kind: "directory"`, `tree`.
  Strictly unique entry names sort by exact UTF-8 bytes. Identity is BLAKE3 of
  compact JSON in that exact schema order, standard serde_json escaping, unsigned
  decimal integers, no whitespace/final newline. Loading must match reserialization
  exactly; alternate key order/escaping/whitespace is not canonical. Tests pin
  canonical bytes. Encoding changes require explicit format-version decisions.
- **Snapshot record v1:** `version`, `id`, `root_hash`, `source_path`,
  `created_unix_seconds`, `created_nanoseconds`. Timestamp is taken after traversal
  and before publication. ID is a domain-separated BLAKE3 digest of a uniquely
  allocated random tempfile basename, root hash and timestamp. It is opaque, not a
  hash of record bytes. Source is a canonical absolute UTF-8 Unix path at capture
  time. Record loading validates schema/version, filename/ID match, root identity
  syntax, nanosecond range and absolute source path without NUL/dot components.
  JSON order/whitespace need not be canonical for history records.

A two-digit shard reduces per-directory object fanout. Reads cap chunks at 16 MiB,
manifests at 64 MiB, nodes at 16 MiB and records at 64 KiB. A shared reader rejects
non-regular/symlink objects, checks the opened descriptor and length, then bounds
bytes read if a file grows. Unix final opens use NOFOLLOW/NONBLOCK/CLOEXEC; non-Unix
has metadata checks without equivalent final-open guarantees and is unvalidated.
Ancestor paths still depend on the trusted-store contract.

`FileManifest::load(path)` validates a user-supplied recipe. `load_by_hash` additionally
checks the identity against exactly the bytes parsed and is used for snapshot
references. Duplicate writes verify existing objects and never silently repair them.
The shared `publish_if_missing` policy verifies first and returns early only on
success. Chunks use the existing `get` BLAKE3 check; manifests compare bounded
existing bytes with validated, freshly hashed bytes; nodes use their hash/schema
loader. Only an I/O NotFound selects the unchanged durable publication path.
Permission errors, corruption, invalid types and oversize objects are errors,
not cache misses. If no-clobber publication loses a race, the winner is verified
again. There is no existence-only shortcut, trusted-filename cache, automatic
repair, or source mtime-based skip. All source bytes are still read/hashed.
Saving a standalone node does not verify its descendants; snapshot creation builds
and persists the reachable graph bottom-up before committing history.

## Snapshot capture and Merkle behavior

1. Open the source directory without following a symlink root. Normalize trailing
   `/` or `/.`, canonicalize its path and check that it matches the opened device/inode.
   Ancestor symlinks in the user-supplied path can resolve normally. Reject a source
   equal to or inside the store.
2. Read/sort child names from open directory handles. Inspect children with
   non-following `statat`. Open children relative to those handles, with NOFOLLOW;
   directory opens require DIRECTORY and regular-file opens also use NONBLOCK.
   The opened file must still be regular. Reject symlinks, dangling links and
   special files rather than silently dropping them.
3. Exclude the actual nested store by directory device/inode. Other `.rewind`
   directories remain ordinary data. A parent that becomes empty is preserved.
4. Stream file bytes through Phase 1 and save manifests. Recurse into directories
   and save canonical nodes only after their children are stored. Root depth is
   zero; traversed directories deeper than 256 are rejected.
5. Create a unique snapshot record only after the root is saved. Publish it through
   the same synced no-clobber helper. No global history index is required.

A file change alters its manifest, parent node, and affected ancestors. Unchanged
sibling subtrees retain their identities and reuse stored nodes. Independent
captures with identical relative contents have identical roots, while IDs and
capture metadata remain distinct. This is structural reuse, not incremental
filesystem scanning: every capture still reads the entire source tree.

Concurrent edits can produce a graph of bytes observed at different times; there
is no filesystem transaction or consistent source view. Disappearing entries and
read errors abort, but other concurrent edits need not be detected. Renaming an
opened directory does not redirect its handle. Quiesce a source when logical
point-in-time consistency is required. Mounts can be traversed; arbitrary hostile
mount/device changes are outside the contract.

## History, restoration and diff

History scans recognized `<64 lowercase hex>.json` filenames only. Temporary and
unrelated filenames are ignored. A missing history namespace is empty history;
non-directory/symlink namespaces and malformed recognized records are errors.
Results sort by (seconds, nanoseconds, ID), with source paths quoted to escape
control characters. Listing validates records only, not the content graph.

Snapshot restore resolves the user-selected existing parent and store, rejects
outputs inside the store/recorded source location, and refuses every existing
output path—including symlinks and empty directories. It creates a private sibling
`.rewind-restore-*` stage. Names are validated single components; directory creation
never reuses an existing entry. File creation is exclusive and no-follow. This
also catches case-folding/Unicode-normalization collisions on the destination.
Directories/files use 0700/0600, subject to umask; original metadata is not restored.

Every directory node is hash/schema-verified before use; every file manifest is
identity/schema-verified and its declared length checked. Phase 1 reconstructs
ordered chunks, checks each chunk identity/length and the whole-file hash. Files
are synced and directories synced bottom-up. Only then is the complete stage
published using `renameat_with(NOREPLACE)`: macOS `renameatx_np(RENAME_EXCL)` or
Linux `renameat2(RENAME_NOREPLACE)`. There is no ordinary-rename fallback. Unsupported
kernels/filesystems fail. The destination parent is synced after publication.
The live source is never used to reconstruct data and may no longer exist.

Ordinary restore failure explicitly closes/removes staging, reporting cleanup
errors too. A `Result`-returning closure centralizes that cleanup even when `?`
propagates a deep traversal error. User-supplied parent-path aliases are resolved
before staging; the parent must remain trusted and stable. Creation/cleanup still
use paths, even though child writes and final rename use anchored handles. A saved
source path is a location guard, not a permanent identity after arbitrary renames.

Diff validates both records and root nodes (one root load if equal). Equal root
hashes return no changes without descending. Unequal directories merge sorted
child slices, recursing only into unequal directory identities. Equal file
manifest/length pairs are skipped; changed references are loaded by hash and
compared by `(file_hash, length)` after checking lengths against node entries.
Rechunking unchanged bytes therefore need not produce a content modification.
Added/deleted directories expand to all descendants, including empties. Type
replacements emit delete+add; renames are not detected. Paths sort by UTF-8 bytes,
with deletion before addition at a shared path. This report is not a mutation plan.

**Diff does not read chunks or equal descendants to look for corruption.** It
compares recorded byte identities and verifies metadata it actually reads. A clean
diff is not evidence that either archive can be restored. Tests deliberately remove
an unchanged subtree to prove pruning; other tests reject damaged visited nodes.
Results are fully collected before printing, so a later metadata error yields no
partial successful-looking report. Stdout failures propagate as errors too.

## Durability and security boundary

New objects use unique same-filesystem temporary files: write all bytes, flush,
`sync_all`, then `tempfile::persist_noclobber`. On Unix this uses hard-link style
publication rather than replacing an existing object with ordinary rename. Sync
the parent after publication. Newly created directories and their parents are
synced, including `.` for a relative top-level store. File/directory errors propagate.
Symlink leaf directory namespaces are rejected when created/validated.

Before snapshot publication, failure may leave unreferenced immutable objects but
no new history record. Tests inject an error after tempfile sync and before final
publication, and test directory-sync errors and blocked history paths. These are
ordinary fault tests, not abrupt-kill/power-loss simulations. Once publication
occurs, a parent-sync or cleanup failure can return an error with the object/record
or complete restored output already visible. Error is not a rollback guarantee.
Tempfiles/hard links and partial restore stages can survive interruption; no
scavenger, transaction recovery journal, retention or GC protocol exists. Retrying
after a directory-sync error is not a tested durability-recovery protocol; existing
objects/directories are not globally re-synced or repaired on open.

Filesystem/hardware must honor sync. Non-Unix lacks the portable directory-sync
behavior. macOS does not request F_FULLFSYNC's stronger drive-cache barrier.
Network filesystem semantics and power-loss durability are untested. Single writer
is an unenforced caller contract; readers are not coordinated with deletion/GC.
The store and destination parent must be trusted/stable. These checks do not create
a filesystem sandbox against hostile same-user ancestor/mount mutations.

Hashes detect content corruption, not authenticity. Record IDs are opaque, so
plausible edits to capture metadata or another valid root cannot be authenticated.
Phase 1 path-loaded manifests likewise do not authenticate provenance. There is
no encryption, compression or repository migration mechanism.

## Memory, performance and unsupported features

Chunk-data working memory is O(max chunk size), with the FastCDC buffer, current
owned chunk and potentially duplicate-read buffer live together. Total process
memory is not constant: manifests grow with chunk count; recursive capture holds
ancestor name/entry vectors; restore/diff retain ancestor node metadata. Serialized
size caps bound reads but construction/serialization can allocate before rejecting
an oversized new node/manifest. Diff holds O(number of reported paths) results and
sorts them in O(k log k). It must scan a wide changed node even for a one-file edit.

Known costs include per-chunk allocation/copying, many small object files and sync
calls for new objects. Verified duplicate chunks/manifests/nodes now avoid temporary
writes and syncs; a new history record still requires durable publication. Repeated
captures still read/chunk/hash source bytes and read/verify existing objects even
when chunk-store growth is zero. Snapshot publication, format versions and public
per-object durability contracts are unchanged. Directory sync batching, worker pools
and packed storage are not implemented. Native baseline measurements are in
[validation](validation.md); [verified reuse results](verified-reuse.md) record the
approved optimization. Small datasets are not proof of an asymptotic bound or a
cross-tool speedup.

Names must be UTF-8, <=255 bytes and portable single components: no dot/dot-dot,
separators, controls/NUL, Windows drive/stream punctuation, reserved device names,
or trailing dot/space. No normalization/case folding is performed. Paths and
filesystems may impose stricter limits. Original timestamps, permissions,
ownership, ACLs, xattrs, sparse extent maps and hard-link relationships are not
preserved; hard-linked sources restore as independent files. Source symlinks,
FIFOs, sockets and devices are unsupported. Only macOS has been validated; capture
is enabled on Unix, restore on macOS/Linux, and other-platform behavior is limited
and unvalidated. Automatic monitoring, retention, GC, rename detection and FUSE
are absent.

## Proposed Phase 3 — not implemented

Agree the next scope before starting. A reasonable sequence is:

1. Define source-mutation detection/retry policy and enforce single-writer locking;
   broaden abrupt-interruption/commit recovery tests before adding concurrency.
2. Add a watcher frontend with explicit debounce, overflow/error handling and full
   rescan fallback, retaining manual capture as the correctness reference.
3. Track changed paths and rebuild affected Merkle ancestors incrementally; test
   watcher event loss and compare results against full captures.
4. Repeat the same measurements before choosing optimizations. Design reader/GC
   coordination and retention separately before enabling any deletion.

No Phase 3 code was added in Milestone E.
