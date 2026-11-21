# Rewind 1.0.0 release candidate

Release preparation date: 2026-11-20 (America/Toronto). No tag, push, GitHub release,
or crates.io publication has been performed. See raw JSON timestamps for actual
measurement times (UTC may fall on the next calendar date).

## Release decision

Ship the strict implementation if its documented limits meet the release scope.
**Fast publication is unfinished and disabled.** `--mode fast` returns a setup
error before opening the store or starting monitoring. No event-assisted snapshot
is published, and no strict/fast performance comparison or speedup is claimed.

Repository inspection found the completed C publisher and D shadow cache, not a
production incremental transaction. The candidate carries only a root and stats;
it does not retain a validated changed-object overlay, affected-path two-pass
proof, full reachable archive verification, or a sealed publication capability.
Connecting that root directly to history would risk missing objects and stale
source data. Completing these contracts and failure tests is deferred rather
than bypassed for a deadline.

## Changes for this candidate

- Version 1.0.0 package metadata, repository link, MIT license, explicit package
  contents (excluding demo files and Python bytecode), help/version commands.
- Explicit strict CLI selection, rejection of unfinished fast mode, configurable
  periodic authoritative reconciliation (default 60 seconds, range 1..86400).
- The timer uses the worker's actual attempt start, including when completion
  consumption is delayed. Waiting uses channel timeouts, not a polling loop.
- Version 2 snapshot records durably include `manual_traversal` or
  `strict_two_pass` provenance. Version 1 remains readable with unknown provenance;
  existing records are never relabelled. Older readers reject new records.
- Structured automatic outcomes identify effective mode and full-source scope,
  separately from the original record's validation strategy on identical skips.
- Focused tests for mode/options, help/version, legacy/v2 provenance, periodic
  no-write identical suppression and fatal idle archive-corruption detection.
- Clean README, installation/CLI walkthrough and reproducible smoke/benchmark tools.

Storage hashing, no-clobber publication, exclusive writer coordination, pending
recovery and object/history barriers are unchanged. New provenance is included in
the existing atomic record publication, never appended after commit. Rust's enum
restricts valid strategies (like a Python Enum), and Option distinguishes unknown
legacy provenance from a known validation strategy.

## Validation

The full native macOS suite passed **124 tests, 0 failed, 0 ignored**. This includes
existing randomized differential roots, missed-event negative tests, aliases and
renames, writer coordination, interruption/recovery injection and historical
restoration. These tests exercise the strict publisher and shadow candidates;
there are no production-fast interruption tests because no fast publisher exists.

The initial sandboxed test invocation failed three FSEvents startup tests because
the sandbox could not start streams. Running with native OS access passed. One
existing unsupported-version fixture used version 2, now valid; it was updated to
version 3 and the suite rerun. These are not hidden skipped tests.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
python3 scripts/smoke_release.py
cargo package --allow-dirty --offline
```

An isolated `cargo install --path . --locked --offline --root <temporary-directory>` also passed and the installed executable reported `rewind 1.0.0`. No global user installation was changed.

The package command verifies a local source archive/build, not publication. The
release smoke verifies file ingestion/verification/restoration, two directory
versions, additions/deletions/modifications, identical diff, restoration without
source, invalid IDs, existing-output refusal and explicit fast rejection.

Subprocess interruption tests are **process-crash tests**, not actual power-cut
experiments. No Linux or power-loss validation was performed. Only the installed
Rust 1.99.1 toolchain was exercised; 1.89 is the declared API minimum, not a second
tested compiler. No comprehensive dependency security audit was performed.

## Reproducible measurements

Environment: Apple M4, 16 GiB, macOS 15.7.4 arm64, local APFS temporary storage,
Rust/Cargo 1.99.1, Python 3.13.13, default release profile. Cache is warm/uncontrolled;
there is no eviction, power-loss test or claim about another platform. Each report
records its executable SHA-256. Run measurements serially without competing tests
or workloads for comparable results.

```sh
python3 scripts/benchmark_automatic.py --trials 3 --mib 32 --many-files 256 \
  --idle-seconds 2 --output docs/release-1.0-benchmark.json
python3 scripts/benchmark_release_scale.py
```

32 MiB measurements use one file or 256 distinct 128-KiB files generated with
SHAKE256. Each of three trials per layout captures a baseline, unchanged touch,
three 4-KiB edits, 25-write burst, idle window and restart, then verifies all five
historical restores. There are 30 verified restores across the six trials.

Timing includes event delivery, 300-ms debounce, queue/lock wait, shadow candidate,
two authoritative scans, verification and durable publication. Worker time excludes
event/debounce delay. Fresh baselines additionally include watcher startup. These
are complete strict measurements, not claimed improvements over a paired old build.

| Workload | Files | Samples | Median outcome latency | Median worker time |
| --- | ---: | ---: | ---: | ---: |
| baseline | 1 | 3 | 2875.9 ms | 2872.0 ms |
| unchanged | 1 | 3 | 493.7 ms | 177.4 ms |
| edit | 1 | 9 | 530.5 ms | 213.6 ms |
| burst_25 | 1 | 3 | 535.0 ms | 219.0 ms |
| baseline | 256 | 3 | 4896.4 ms | 4892.1 ms |
| unchanged | 256 | 3 | 478.6 ms | 162.0 ms |
| edit | 256 | 9 | 516.7 ms | 200.6 ms |
| burst_25 | 256 | 3 | 519.0 ms | 202.8 ms |


Separate archive verification, 1 file(s): median **34.9 ms**, range 34.6–52.5 ms (9 passes).

Separate archive verification, 256 file(s): median **40.5 ms**, range 40.1–54.1 ms (9 passes).

All six two-second idle windows measured 0.00 s aggregate CPU delta at ps TIME resolution; this is not a zero-CPU guarantee. Idle endpoint RSS ranged from 11.7 to 13.2 MiB (not peak RSS).

First 4-KiB edit chunk-payload growth for 1 file(s): 134,257 bytes; object/metadata counts are in the raw report. This excludes manifest/tree/record bytes and APFS allocation overhead.

First 4-KiB edit chunk-payload growth for 256 file(s): 86,977 bytes; object/metadata counts are in the raw report. This excludes manifest/tree/record bytes and APFS allocation overhead.


The standalone `verify_archive` example reads the final snapshot's entire archive
without reading source files: node/manifest hashes and schema, lengths, chunk BLAKE3
and whole-file hashes. Three warm passes per trial are measured separately. This
is a direct archive-verification workload, **not** an isolated subcomponent timing
inside strict capture; subtracting it from worker time would be invalid. No
cross-attempt verification cache skips corruption checks.

Idle CPU uses aggregate `ps TIME` deltas at 0.01-second resolution over two-second
windows. RSS values are endpoints, not peak memory. Short windows do not establish
zero resource usage or long-term behavior. These measurements do not isolate
fsync latency or user/system CPU; prior profiling is historical, not repeated here.

The file-count benchmark uses one flat directory of unique 4-KiB files at 1,000
and 10,000 files, one trial per scale, with the same source operations and one latest-version restore check per scale.
An earlier 10,000-file trial exceeded the harness's 120-second baseline timeout;
that incomplete run produced no retained timings. The measurement deadline was
increased to 600 seconds and the final runs were repeated serially.

The scale run uses a 86400-second reconciliation interval so a long baseline does
not queue overdue periodic work into individual edit latency samples. Periodic
behavior is covered separately by one-second integration tests; these timings
do not amortize periodic reconciliation costs.

Its edit overwrites 4 KiB starting at offset 2185, extending the selected file;
that exact state is verified on restoration. Flat directory sorting/rebuilding is
part of the workload. **100,000-file end-to-end publication is not run/supported:**
the 100,000-entry authoritative observation cap includes root/directories. The
previous 100,000-file shadow-only benchmark does not establish publishing support.

| Files | Source size | Baseline | Median edit latency (3 edits) | Median edit worker | Separate archive verification (3 passes) |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 3.91 MiB | 10.04 s | 471.8 ms | 159.9 ms | 34.4 ms |
| 10,000 | 39.06 MiB | 95.25 s | 1382.7 ms | 1066.0 ms | 375.5 ms |

Both scale trials passed latest-version byte-for-byte restoration and restart suppression. Their two-second idle windows measured 0.00 s aggregate CPU delta at ps resolution; endpoint RSS was 11.6 MiB / 48.4 MiB. These are single trials, not distribution or peak-memory estimates.

## Remaining limitations and release blockers

No known failing test blocks the strict candidate within its tested macOS/local,
trusted-path scope. Fast mode is an unfinished feature, not an available release
mode. If production fast mode or 100,000-file captures are release requirements,
those remain blockers. Fresh-baseline performance remains material for many files.

Both strict passes and final metadata validation have TOCTOU windows; neither
promises atomic point-in-time source state. Stable unreported changes are found at
a later successful reconciliation; vanished intermediate changes cannot be
recovered. Continuous instability, backoff, blocked stdout or I/O can exceed the
reconciliation interval. Slow capture cannot guarantee a maximum history gap.

Stored-object corruption fails closed; an unobserved source edit is a freshness
problem and does not imply corrupted stored bytes. Records themselves are not
authenticated. Recovery cannot undo device write-cache loss, and macOS sync_all
is not F_FULLFSYNC. Format v2 cannot be read by old binaries. No GC, retention,
encryption, daemon or metadata-preserving restore is included.

## Local review and publication commands

First verify the candidate using the commands above and inspect changes:

```sh
git status --short
git diff --check
git diff
cargo package --list --allow-dirty --offline
```

Only after the owner's approval, review/stage the intended files, commit and tag.
The current repository remote is `https://github.com/darshanvinay/Rewind.git` and
branch is `main`. Recheck these before sending anything:

```sh
git remote -v
git branch --show-current
git add -p
# Also add reviewed new release files, docs, examples, scripts and tests:
git add LICENSE CHANGELOG.md docs/release-1.0.md docs/release-1.0-benchmark.json \
  docs/release-1.0-scale-benchmark.json examples/verify_archive.rs \
  scripts/benchmark_release_scale.py scripts/smoke_release.py tests/cli_tests.rs
git diff --cached --check
git diff --cached
git commit -m "Prepare Rewind 1.0.0 strict release"
git tag -a v1.0.0 -m "Rewind 1.0.0: verified strict snapshots"
git push origin main
git push origin v1.0.0
# Optional, with GitHub CLI installed and authenticated:
gh release create v1.0.0 --verify-tag --title "Rewind 1.0.0" --notes-file CHANGELOG.md
```

The GitHub release above is source-only; no binary artifacts are implied. A
crates.io release is separate, requires name availability/account authorization,
and is not part of this release preparation. Do not run `cargo publish` as a
substitute for GitHub publication.
