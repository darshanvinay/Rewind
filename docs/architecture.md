# Rewind architecture — Phase 3 C and D shadow foundation

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
| `transaction` | Advisory writer lock, pending-marker recovery and capture directory barriers. |
| `manifest` | File recipes, ingestion, identity loading, reconstruction, file verify/restore. |
| `tree` | Sorted directory nodes, canonical bytes, validation and content-addressed persistence. |
| `snapshot` | Shared descriptor-relative traversal and durable history publication. |
| `snapshot::scan` | Read-only probe, verified/persistent second pass and temporary stability observations. |
| `shadow` | Bounded metadata/reference cache, typed dirty scopes, non-publishing candidates and diagnostics. |
| `watch::automatic` | One capture worker, bounded controller work, generation-aware reconciliation and retries. |
| `snapshot_restore` | Private staging-tree reconstruction and atomic no-replace publication. |
| `diff` | Merkle pruning and relative-path content-change reports. |
| `watch` | Native recursive intake, source validation/exclusion, bounded channel, timed receive loop and shutdown. |
| `debounce` | Bounded change accumulation, quiet/maximum deadlines, rename hints and sticky reconciliation state. |
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
  writer.lock                     permanent advisory lock inode
  capture.pending                 present during a write / after interruption
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
2. Acquire the exclusive OS writer lock, recover any interrupted write, and durably
   publish `capture.pending` before changing object namespaces. Read/sort child names from open directory handles. Inspect children with
   non-following `statat`. Open children relative to those handles, with NOFOLLOW;
   directory opens require DIRECTORY and regular-file opens also use NONBLOCK.
   The opened file must still be regular. Reject symlinks, dangling links and
   special files rather than silently dropping them.
3. Exclude the actual nested store by directory device/inode. Other `.rewind`
   directories remain ordinary data. A parent that becomes empty is preserved.
4. Stream file bytes through Phase 1 and save manifests. Recurse into directories
   and save canonical nodes only after their children are stored. Root depth is
   zero; traversed directories deeper than 256 are rejected.
5. Sync every dirty object directory once, deepest first, including newly created
   shards, their namespaces and the store root. Only after this complete barrier
   create/publish the snapshot record, sync its history directory, remove the pending
   marker, and sync the store root. No global history index is required.

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
`sync_all`, then `tempfile::persist_noclobber`. This preserves atomic no-clobber
publication (native no-replace rename when supported, with the dependency's
hard-link fallback). Every new object's file sync completes **before** its final
hash name appears. Existing chunks/manifests/nodes are still verified before reuse.

Capture alone defers object **directory** syncs. Its private owned session holds
`writer.lock` for recovery, ingestion, the object barrier, snapshot publication,
and pending-marker cleanup. A set deduplicates dirty directory paths; syncing
children before parents makes both object names and newly created ancestor links
durable before any snapshot name appears. The session is sealed after the barrier;
it rejects new object publication and refuses snapshot publication before sealing.
Standalone chunk puts, manifest saves and node saves retain immediate per-object
file and directory syncs. They also acquire the same writer lock and use the marker,
so a failed standalone publication cannot leave an untracked reuse hazard.
`store_file` coordinates each object mutation; it is not a whole-file transaction.

`writer.lock` is a permanent regular file, locked through `File::lock` on an owned
file descriptor. It must never be deleted/replaced while a process uses the store.
Waiting writers block. Locks are advisory: external programs can ignore them.
Separate handles/processes coordinate; read-only commands do not acquire a writer
lock or perform recovery. No concurrent GC/deletion protocol exists. The pending
marker is **not** a lock, and is inspected/recovered only under the real lock.

The marker contains exactly `rewind-pending-v1\n` (bounded read, version checked).
It is published with a synced temporary and root-directory sync before write work.
On the next mutation, recovery syncs all surviving directories in chunks, manifests,
trees and snapshots, deepest first, followed by the root and its canonical
ancestors. This also repairs surviving links from interrupted initialization.
Lost shards are absent and cannot be reused; surviving ancestors are synced and
missing objects are safely recreated when needed. Recovery rejects malformed or
symlink markers/namespaces, unsupported special entries and unexpected directory
depth. It is a namespace barrier, not a full content scan: reuse still checks bytes.

**Recovery fails closed:** scanning or barrier failure returns an error with the
marker retained. It is removed only after the barrier succeeds; root sync then
makes removal durable. File objects published by this protocol had already been
file-synced, so recovery does not need to sync their payloads again. This assumption
requires a trusted store and no older untracked failed writes. The marker cannot
retroactively detect incomplete writes made by older releases without a journal.
Do not run older writers concurrently: they do not honor the lock or journal.

Before the object barrier finishes, capture failure leaves no new history record.
It may leave synced but unreferenced objects, incomplete tempfiles and a marker.
After the barrier, all referenced object names/data are durable under the existing
sync contract. A crash after snapshot publication but before history-directory
sync may preserve or lose that record, but any surviving record references the
completed graph. A later sync/cleanup error can return failure with a valid record
visible. Dropping a failed session releases its lock and leaves its marker; Drop
never attempts fallible commit/cleanup. Successful marker unlink followed by a
failed root sync is also safe: the already completed graph/record barriers do not
rely on that unlink, and a reappearing marker causes harmless extra recovery.

Fault-injection, abrupt child exits and SIGKILL tests exercise these boundaries.
They are process-crash tests, **not actual power-loss tests**. In particular, a
process kill leaves the kernel's caches intact. Filesystem/hardware must honor
`sync_all`. macOS still does not request F_FULLFSYNC's stronger drive-cache barrier;
network filesystem semantics and real power-loss behavior are untested. Non-Unix
lacks portable directory sync; its guarantees remain limited and unvalidated.
No scavenger, retention or GC is added; orphan objects and temporary restore stages
can remain. Trusted/stable store paths and destination parents remain required;
these checks do not create a sandbox against hostile ancestor/mount mutations.

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
per-object durability contracts are preserved. Capture directory sync batching is
implemented; worker pools and packed storage are not. Native baseline measurements are in
[validation](validation.md); [verified reuse results](verified-reuse.md) record the
earlier optimization. [Capture batching](capture-batching.md) documents the current
protocol, benchmarks and remaining costs. Small datasets are not proof of an asymptotic bound or a
cross-tool speedup.

Names must be UTF-8, <=255 bytes and portable single components: no dot/dot-dot,
separators, controls/NUL, Windows drive/stream punctuation, reserved device names,
or trailing dot/space. No normalization/case folding is performed. Paths and
filesystems may impose stricter limits. Original timestamps, permissions,
ownership, ACLs, xattrs, sparse extent maps and hard-link relationships are not
preserved; hard-linked sources restore as independent files. Source symlinks,
FIFOs, sockets and devices are unsupported. Only macOS has been validated; capture
is enabled on Unix, restore on macOS/Linux, and other-platform behavior is limited
and unvalidated. Native monitoring and automatic capture are available; retention, GC,
snapshot rename detection and FUSE are absent.

## Phase 3 Milestone A — event logging foundation (historical)

The approved Phase 3 architecture adds layers around the existing capture engine:

```text
OS native notifications -> bounded event channel -> validated normalized hints [A]
    -> debounced logical change batches                                    [B]
    -> stability/retry policy -> existing capture + lock/recovery/barriers   [C]
    -> safe cache/invalidation + authoritative reconciliation               [D]
    -> end-to-end reliability and workload measurements                     [E]
```

A, B and C are implemented; the description below records the A foundation. Later milestones require approval; D additionally
requires reviewing its cache/invalidation design before any implementation.
The watch command never calls snapshot creation, acquires a writer lock, performs
pending-marker recovery, hashes source data or mutates storage. Concurrent manual
writers retain their existing locking and durability protocol. `open` still
requires an initialized store, and read-only monitoring leaves a pending marker alone.

`Monitor::start` opens a no-follow source directory descriptor, resolves canonical
source/store locations, checks the opened source device/inode and rejects a source
inside the store. It registers a `notify::RecommendedWatcher` recursively and then
walks the source through descriptor-relative no-follow directory opens and stats.
The scan enforces the existing portable name policy, root depth zero and maximum
256 directory levels, rejecting symlinks/special entries and skipping the store's
actual device/inode. It retains directory iterators rather than a whole-tree index.
No payloads are opened/read for hashing. There is no point-in-time startup view.

The native backend owns a callback thread. Its callback ignores access-only hints
and uses `SyncSender::try_send` into a 1,024-message application queue. Each accepted
notification has at most 64 paths / 64 KiB of encoded path bytes; oversized backend
errors are bounded by the same path limit. An atomic overflow flag plus a wake/full
queue makes loss observable without blocking that thread. Library and OS internal
buffers are separate from this application bound; total native watcher memory also
scales with registrations. Native recursive registration can traverse the storage
namespace even though Rewind filters its events out at the consumer.

`Monitor::run(self, output)` owns the watcher, receiver and source descriptor until
it returns. `recv()` sleeps while no events arrive. `StopHandle` shares a stop flag
and sender; the CLI installs one `ctrlc` handler that sets the flag and sends a wake
message. If the queue is full it already provides a wakeup. The loop gives shutdown
priority, emits a stopped status and drops its native watcher. It does not drain
pending hints. The CLI registers the handler after startup validation; an interrupt
during earlier setup follows the process's normal SIGINT behavior. Output writes
remain synchronous: blocked stdout or a long validation scan can delay shutdown.

A channel resembles Python's `queue.Queue`, but Rust makes transfer/ownership
explicit. A `move` closure owns callback sender clones so they survive the setup
function. `Arc` shares a stop/overflow flag across threads with reference counting;
atomics communicate tiny pieces of state without a mutex. `Result` and `?` propagate
errors rather than suppressing them. Keeping the watcher in `Monitor` matters:
returning from setup with only a receiver would drop the watcher and stop events.
Dropping the owned monitor closes native resources; no unsafe Rust was added.

The normalizer checks root/store identity, rejects event-loss flags and ambiguous
empty/outside/parent-traversal paths, and maps kinds into a small stable vocabulary.
Canonical roots anchor lexical suffixes so a removed path can still be reported;
a deleted leaf itself cannot be canonicalized. Existing components are checked via
no-follow descriptor traversal. New directory/coarse root hints are scanned for
unsupported entries; vanished paths are normal deletion/rename hints. Paired rename
paths retain their order; crossing the store boundary keeps only the source-side
path. Backend events need not supply paired renames. Unknown-path rescan events
fail explicitly, rather than inventing a specific file save.

Exclusion uses the canonical store prefix and the store's device/inode, not the
basename `.rewind`. The store is filtered before normal source-name validation, so
its internal names do not become user-file errors. An unrelated `.rewind` directory
is ordinary data. Store/root movement or replacement requires restart when detected.
A coarse ancestor hint may still be logged as `"."`; it cannot be safely attributed
to only storage activity. No capture is triggered by any hint in A. B/C must treat
coarse hints conservatively and compare resulting state, not assume one event is
one save. An unsupported object created and removed between observation points
may no longer be inspectable, and unreported native event loss is not detected.
Trusted/stable ancestors remain required; this is not a hostile-filesystem sandbox.

macOS selects FSEvents; Linux selects inotify. FSEvents can coalesce changes or
report coarse/rescan flags. inotify uses per-directory registrations, kernel watch/
queue limits and rename cookies, and cannot guarantee every event while recursively
registering a newly arrived directory. Neither is an authoritative replay log.
`with_follow_symlinks(false)` configures backends supporting that option; Rewind's
own validation rejects symlinks independently. FSEvents does not use that config
option for recursion. OS permissions and unsupported/network/pseudo filesystems can
limit delivery. No PollWatcher or busy-loop fallback is enabled. macOS alone is
validated here; Linux behavior remains untested. See the
[notify backend limitations](https://docs.rs/notify/latest/notify/) and
[Ctrl+C handler lifecycle](https://docs.rs/ctrlc/latest/ctrlc/fn.set_handler.html).

In A, backend errors, lost-event flags, invalid runtime entries and application
queue overflow stopped with a diagnostic. B supersedes the raw event output and
loss handling as described next. Automatic capture/reconciliation and retries
remain C–E work. Existing Phase 2
formats, synchronization, verified reuse and crash-recovery behavior are unchanged.


## Phase 3 Milestone B — debounced hint batches (historical)

`debounce.rs` is a private, filesystem-independent state machine. `DebounceConfig`
is publicly re-exported through `watch`, and `Monitor::start_with_config` supplements
the unchanged default `Monitor::start` API. The CLI accepts `--debounce-ms` and
`--max-delay-ms`. Constructors validate 1 <= quiet <= maximum <= 60,000 milliseconds;
defaults are 300 and 2,000. No capture/storage code or on-disk format changed.

The states are:

1. **Idle:** no pending accumulator; `recv()` blocks indefinitely without a timer.
2. **Collecting:** first hint fixes the maximum deadline; each hint updates the
   quiet deadline. Receive sleeps until `min(first + maximum, last + quiet)` or a
   message. The main loop checks deadlines before another receive, even if the
   queue is continuously nonempty.
3. **Emitting:** `Option::take()` moves the owned bounded batch out of the accumulator
   before synchronous serialization/output. Native callbacks keep queueing. Once
   output completes, remaining notifications start a separate batch.

`Instant` is a monotonic clock, unlike wall time. Unit tests supply explicit
Instants instead of sleeping. `BTreeMap`/`BTreeSet` resemble Python sorted maps/sets:
they deduplicate while giving deterministic output. An affected path keeps all
observed kinds; create/remove sequences are never optimistically cancelled. These
sets are not chronological operations and must not be replayed against a filesystem.
`recv_timeout` sleeps during collection; it is not filesystem polling. The maximum
bounds consumer accumulation, not native event delivery, validation, blocked writes,
or future capture duration. A quiet window is not proof that file contents are stable.

Rename hints retain kind, ordered endpoints and optional backend tracker ID.
`Both` stays a pair; unpaired halves are never joined by time proximity or tracker
alone, nor across batch boundaries. Tracker IDs are backend hints, not globally
unique identities. A paired rename crossing storage exclusion becomes the correct
source-side from/to half. Renames require full reconciliation conservatively because
directory descendants/scope may be unknown. Atomic saves preserve temporary-path
hints plus the destination; no guessed temp-file rules erase potentially relevant
user changes. Repeated identical hints coalesce, so intermediate version counts
cannot be inferred from the batch.

Memory has two independent bounds: the existing 1,024-message/64-path/64-KiB callback
queue, and an accumulator capped at 4,096 distinct paths, 1,024 distinct rename hints
and 1 MiB of stored path bytes (including copied rename endpoints). Kinds and reasons
come from fixed internal vocabularies. Reaching any cap retains bounded known hints
and records `batch_capacity`; omitted details require a full scan. Data structure
and serialization overhead are additional to path-byte limits. Native library/OS
queues and per-directory registrations have separate costs.

Overflow uses an out-of-band atomic flag, consumed with `swap(false)` so another
concurrent loss cannot be silently cleared. A full queue already wakes the consumer;
an oversized message also attempts a wake. Both become a reconciliation-marked
batch, including when no path detail survives. Backend `need_rescan`, empty or
unresolvable paths also become reconciliation hints, not invented changes. Backend
errors and invalid/unsupported runtime entries still terminate: the loop first
attempts to emit a `backend_error` or `validation_error` marked batch. A failed output
may prevent even that diagnostic batch; the CLI still returns an error.

`reconciliation_required` and its finite reason set are **sticky across batches for
the monitoring session**. Emitting a warning is not acknowledgement of successful
reconciliation. No B code clears it; C must provide an explicit full-scan success
boundary, accounting for events/loss during the scan, before any clearing. Startup does not invent a baseline snapshot/cache.
B's validation scans do not create a Merkle snapshot or implement C reconciliation.
A restart loses in-memory hints/flags; C must establish an authoritative baseline
on restart regardless of the first batch's flag. B is not a durable event journal.

Storage-only events are filtered before the bounded queue using canonical prefix
plus a current directory device/inode check. Rescan flags, mixed source/store
renames, and store identity changes are never swallowed by this filter. Consumer
validation retains the original identity exclusion. Coarse root/ancestor events
cannot be safely distinguished from relevant source changes; they require a full
reconciliation. C must compare resulting Merkle roots to avoid turning such hints
into unnecessary history. B itself never triggers capture.

Ctrl+C retains priority. It flushes the bounded accumulator early, checks a bounded
number of queued messages (ignoring wake-only controls), marks any known pending
hint remainder for reconciliation, and reports stopped. It does not promise a
complete final drain or interrupted-write filesystem state. A synchronous blocked
stdout/validation operation can delay termination; no extra output-worker subsystem
was introduced. Existing writer locking, pending-marker recovery and file/directory
sync ordering remain unchanged. Milestone C and later milestones require approval.


## Phase 3 Milestone C — automatic durable history

The `watch` CLI calls `Monitor::run_automatic`; the event-only `run` method remains
available. `snapshot::capture_automatic` returns `AutomaticCapture::Created(record)`
or `Unchanged(record)`, with typed `Unstable`, `Cancelled` and `SourceIo` errors.
Manual `create_snapshot` keeps its one-pass, always-new-record semantics. No object
format or hash rule changed. Only automatic capture adds the two-pass stability
policy; this is not incremental processing.

The existing `snapshot::unix::capture_directory` traversal is shared through a scan
mode: Manual, Probe, Verify or Persist. File ingestion uses one FastCDC streaming
routine and whole-file hashing pipeline. Manifest `encoded_bytes()` is shared with
save; nodes use the existing canonical constructor/hash. Probe computes identities
without calling put/save/publish. Verify also loads every referenced store chunk,
manifest and node with existing bounded integrity checks. Persist uses the existing
verified-reuse APIs. A leaf helper keeps ingestion locals out of recursive directory
frames, preserving the 256-level limit on ordinary Rust test thread stacks.

Automatic attempt ordering:

1. Open/canonicalize the source with NOFOLLOW and verify its pinned identity for
   monitoring. Open the permanent existing writer lock and wait interruptibly with
   `try_lock` and 25-ms sleeps, then perform required pending-marker recovery.
   Missing lock files require initialization; the automatic lock path does not
   create a namespace entry. The store remains trusted/stable.
2. Under that exclusive lock, select the latest validated history record for the
   canonical source path using existing timestamp/ID history ordering. Malformed
   recognized records fail closed, even if unrelated to this source. Source identity
   is pinned for a monitoring session; records retain only their v1 source path,
   not an across-restart filesystem inode identity.
3. Probe all source bytes and build the first root, collecting identity, mode,
   length, mtime/ctime with nanoseconds for source files/directories. Check metadata
   around each read/traversal. Observation storage is bounded to 100,000 entries and
   16 MiB of absolute path bytes; it is discarded after the attempt, not cached for
   incremental reuse.
4. If the root matches the latest record, perform a second source traversal in
   Verify mode, checking all stored objects too. Require matching roots and stamps,
   then revalidate every observed path through no-follow relative opens and verify
   that the source path still names the opened root. Return Unchanged without a
   new marker or record. Recovery of an already-present marker can still write.
5. Otherwise durably publish the pending marker under the same lock. Perform the
   second pass in Persist mode, require matching roots/stamps, complete the object
   directory barrier, revalidate observed metadata and check cancellation. Only
   then publish/sync the record through the existing no-clobber protocol and clear
   the marker through normal successful completion. Failed attempts retain any
   marker; retries enter recovery rather than inventing rollback.

The read-only probe and skipped path do not create tempfiles, put chunks, save
metadata or start a new transaction marker. A whole-store bytes/metadata inventory
test pins this property. “Latest” preserves existing timestamp/ID ordering, including
its wall-clock limitations. Comparing only the latest source record allows a source
returned to an older historical root to produce a new history record.

Matching passes and metadata checks **do not establish an atomic point-in-time
snapshot**. Metadata can change between checks; content can change and change back;
permissions/timestamp resolution and hostile mutations can defeat observations.
There remains a final-check/publication TOCTOU window. A deterministic test changes
the source in that window: the first archive contains the valid earlier bytes and
a retained event drives a second archive of the new bytes. Unreported native loss
can prevent a follow-up; no filesystem freeze, watcher replay guarantee or periodic
full reconciliation timer is provided. Quiescent input is necessary when a coherent
logical source state is required.

The controller owns debounce state while a single worker owns each synchronous
capture. Job and completion channels each have capacity one, with at most one job
active; further batches coalesce into bounded pending work. A completion is placed
in its separate channel before a nonblocking wake enters the existing bounded
inbox. Even a full inbox is already sufficient to wake the consumer. The worker
uses the same stop flag and its owned session/lock guard drops on failure. Dropping
the controller cancels and joins the worker; a current fsync/final commit can delay
shutdown and may finish successfully. No incomplete record is intentionally exposed.

The controller increments a checked generation for normalized inputs and consumed
loss indications. A job records its dispatch generation. Success acknowledges old
reconciliation reasons only when that generation still matches, no newer pending
batch/work exists and no overflow flag is set. Otherwise it leaves/schedules full
reconciliation. A racing overflow remains out of band until consumed, so clearing
an older reason cannot erase newer loss. Capture success is not acknowledgement of
all later notifications. Every C job scans the full tree, including overflow jobs;
there is no event-driven “unchanged” shortcut.

Only typed source errors at traversal/read boundaries are retryable; storage put,
metadata decode, verification, synchronization and recovery errors keep their fatal
provenance. NotFound, Interrupted, WouldBlock, PermissionDenied and NotADirectory
source errors are retried alongside detected instability. Up to three attempts use
100/300-ms short delays; after three failures, deferred cycles use capped 1..30-s
backoff. The controller still consumes events while waiting. Unsupported entries,
root/store replacement and backend errors fail closed with JSON diagnostics. An
error after commit/cleanup can mean a complete snapshot is already visible; no
rollback guarantee is added.

Rust channels act like bounded Python queues; ownership keeps worker jobs/results
and lock guards alive for exactly their operation. An enum makes created/unchanged
outcomes explicit rather than overloading a nullable ID. `Cell` records only the
local reader's I/O error provenance, so an error from chunk persistence cannot be
mistaken for a transient read failure. No unsafe Rust or new dependency was added.
Source/store writes remain serialized by the existing advisory lock contract.

Confirmed store-only notifications are filtered before queueing; coarse ancestor
hints are conservatively scanned. Because successful identical checks write no new
marker/objects/records, those hints do not sustain a write/notification feedback
loop. On restart, a baseline is always scanned and compared under the writer lock.
No daemon, retention, GC, incremental cache or Phase 4 work was added. Milestone D
requires a separate cache/invalidation design and approval.


## Milestone D foundation — shadow candidates only

The authoritative pipeline remains C. Its existing `automatic` API delegates to
an internal variant with optional seed collection in the first probe; collection
cannot choose a root or bypass validation. The worker computes a speculative
`shadow::MerkleCache` candidate first, runs C, compares roots after C succeeds and
installs only the bounded C-validated seed. No candidate state is installed. Failed
attempts invalidate the cache; a seed exceeding budget disables it while C continues.

The cache stores relative source paths, stamps (identity, type, size, mode, links,
mtime/ctime), manifest references, directory hashes and bounded hard-link aliases.
It holds no source payloads or per-file descriptors. The path index supplies child
references for canonical directory reconstruction. It is process-local and tied to
the binary's v1/default chunking behavior; there is no persistent-cache trust model.

`DirtyPlan` merges typed file/directory/subtree scopes up to 4,096 scopes and 1 MiB
of paths. Ambiguity, loss, invalid paths or exhausted planning capacity require
reconciliation. Known paired rename endpoints become subtree scopes; uncertain
halves remain conservative. The worker's job/result bounds and controller generation
rules remain unchanged. A pending plan is separate from the active job's owned plan.

Candidates use no-follow operations, relist affected ancestors, read dirty/suspicious
files, expand known inode aliases, recursively inspect new/moved subtrees and rebuild
canonical nodes bottom-up. Matching stamps can reuse speculative references only;
they are not source-unchanged proof. Local stability checks are not a second full
pass or atomic snapshot. Missing events are explicitly tested to produce stale
candidates, which never control publication. Source/storage corruption is still
handled by C's existing verification/recovery rules.

Cache caps: 131,072 entries and 128 MiB logical charge; candidate work and diagnostic
scans have separate bounds. Actual RSS includes allocator/index/temporary/native
resources and can exceed a single cache charge. C's original observation cap is
unchanged; a 100,000-file diagnostic benchmark does not imply C publication support
for that many files plus directories. Directory nodes remain flat sorted child
lists, so ancestor fanout contributes work. Path matching currently scans bounded
scope sets; no O(depth) or constant-time claim is made.

An additive `shadow` JSON result reports candidate time, full C validation/commit
plus collection time, cache counters and optional root match. Mismatches are
`mismatch_discarded`. Candidate errors/skips do not replace C. Manual commands and
all durable publication ordering are unchanged. [Detailed limits, tests, measurements
and reproduction](shadow-merkle.md) distinguish candidate-only potential from actual
end-to-end behavior. This is the stopping point; event-assisted publication remains
unimplemented and requires approval.
