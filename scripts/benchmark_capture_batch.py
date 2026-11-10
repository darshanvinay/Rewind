#!/usr/bin/env python3
"""Paired native release measurements with restore validation; Python 3.11+."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import tempfile
from measure_phase2 import command_text
from profile_capture import fingerprint, run_case


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference-binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--trials', type=int, default=3)
    args = parser.parse_args()
    if args.trials < 1:
        parser.error('trials must be positive')
    root = Path(__file__).resolve().parents[1]
    before = fingerprint(root)
    reference = args.reference_binary.resolve(strict=True)
    optimized = root / 'target/release/rewind'
    if reference == optimized:
        parser.error('reference must be a separately saved executable')
    subprocess.run(['cargo', 'build', '--release', '--locked'], cwd=root, check=True)
    report = {
        'measured_at_utc': datetime.now(timezone.utc).isoformat(),
        'environment': {
            'platform': platform.platform(), 'python': platform.python_version(),
            'cpu': command_text(['sysctl', '-n', 'machdep.cpu.brand_string']),
            'memory_bytes': command_text(['sysctl', '-n', 'hw.memsize']),
            'rustc': command_text(['rustc', '--version']),
            'cargo': command_text(['cargo', '--version']),
            'baseline_sha256': hashlib.sha256(reference.read_bytes()).hexdigest(),
            'optimized_sha256': hashlib.sha256(optimized.read_bytes()).hexdigest(),
        },
        'source_sha256': before,
        'method': {
            'source_bytes': 32 * 1024 * 1024, 'trials': args.trials,
            'dataset': 'SHAKE256(rewind-capture-profile-v1); one file or 256 distinct 128 KiB files',
            'patch': 'XOR 4096 bytes at file midpoint +137; middle file for many-files',
            'cache': 'warm/uncontrolled; generated and SHA-256 read before fresh capture; no eviction',
            'ordering': 'alternate baseline-first and optimized-first by trial; separate fresh stores',
            'timing': 'Python perf_counter including native /usr/bin/time launcher; native user/system CPU and peak RSS',
            'validation': 'each root and chunk growth compared; identical roots unchanged; diff and both restores verified with source moved',
            'limitations': 'local APFS; not power-loss testing, no kernel I/O tracing, background load uncontrolled',
        }, 'samples': [],
    }
    payload = hashlib.shake_256(b'rewind-capture-profile-v1').digest(32 * 1024 * 1024)
    with tempfile.TemporaryDirectory(prefix='rewind-capture-batch-') as tmp:
        tmp = Path(tmp)
        for files in [1, 256]:
            for trial in range(1, args.trials + 1):
                binaries = {'baseline': reference, 'optimized': optimized}
                order = list(binaries) if trial % 2 else list(reversed(binaries))
                results = {}
                for name in order:
                    work = tmp / f'{files}-{trial}-{name}'
                    work.mkdir()
                    results[name] = run_case(binaries[name], work, payload, files, False)
                for a, b in zip(results['baseline'], results['optimized']):
                    assert a['root'] == b['root'] and a['chunk_store'] == b['chunk_store']
                report['samples'].append({'files': files, 'trial': trial, 'execution_order': order, **results})
                print(f'PASS {files} files trial {trial}: captures, diff, restores and growth match', flush=True)
    assert before == fingerprint(root), 'engine changed during benchmark'
    report['engine_source_unchanged'] = True
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(f'Wrote {args.output}', flush=True)


if __name__ == '__main__':
    main()
