#!/usr/bin/env python3
"""Strict publication at supported file counts; fast remains explicitly disabled."""
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import tempfile
from datetime import datetime, timezone
from benchmark_automatic import run

root = Path(__file__).resolve().parents[1]
subprocess.run(['cargo', 'build', '--release', '--bins', '--examples', '--locked'], cwd=root, check=True)
binary = root / 'target/release/rewind'
binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
report = {'measured_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
          'binary_sha256': binary_hash,
          'method': 'One trial per scale, unique SHAKE256 payloads split into 4-KiB files in one directory; 300-ms debounce, 86400-second reconciliation interval to isolate event workloads from long-baseline overdue timer work; warm uncontrolled APFS cache; same automatic workload, latest-version restoration, and separate archive verification as benchmark_automatic.py',
          'fast': 'unfinished: no fast publication or strict/fast speedup measurement',
          '100000_files': 'not run: authoritative observations are capped at 100000 entries including root/directories, so 100000 files exceed the supported limit',
          'samples': []}
with tempfile.TemporaryDirectory(prefix='rewind-release-scale-') as name:
    for files in [1000, 10000]:
        work = Path(name) / str(files)
        work.mkdir()
        payload = hashlib.shake_256(b'rewind-release-scale-v1').digest(files * 4096)
        # run() writes 4096 bytes at width/2+137, extending the edited file;
        # this is intentional and historical restore checks validate that state.
        report['samples'].append(run(binary, work, payload, files, 1, 2, verify_all=False, reconcile_interval=86400))
        (root / 'docs/release-1.0-scale-benchmark.json').write_text(json.dumps(report, indent=2) + '\n')
        print(f'PASS {files} files', flush=True)
(root / 'docs/release-1.0-scale-benchmark.json').write_text(json.dumps(report, indent=2) + '\n')

assert hashlib.sha256(binary.read_bytes()).hexdigest() == binary_hash, "binary changed during measurement"
