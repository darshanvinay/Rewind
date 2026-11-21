#!/usr/bin/env python3
"""Release automatic-history measurements with exact historical restore checks."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import signal
import subprocess
import tempfile
import threading
import time
from measure_phase2 import chunks, command_text, state


def counts(work):
    store = work / '.rewind'
    return {**chunks(work), 'snapshots': len(list((store / 'snapshots').glob('*.json'))),
            'manifests': len(list((store / 'manifests').glob('*.json'))),
            'trees': sum(p.is_file() for p in (store / 'trees').glob('*/*'))}


def resources(pid):
    raw = subprocess.check_output(['/bin/ps', '-p', str(pid), '-o', 'time=', '-o', 'rss='], text=True).strip()
    cpu, rss = raw.split()
    total = 0.0
    for value in cpu.split(':'):
        total = total * 60 + float(value)
    return {'cpu_seconds': total, 'rss_bytes': int(rss) * 1024, 'ps_output': raw}


class Watch:
    def __init__(self, binary, work, reconcile_interval=60):
        self.events = queue.Queue()
        self.process = subprocess.Popen([str(binary), 'watch', 'source', '--debounce-ms', '300', '--max-delay-ms', '2000', '--reconcile-interval-secs', str(reconcile_interval)],
                                        cwd=work, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.lines = []
        def reader():
            for line in self.process.stdout:
                self.events.put(json.loads(line))
        threading.Thread(target=reader, daemon=True).start()
        self.wait(lambda v: v.get('status') == 'watching')

    def wait(self, predicate):
        deadline = time.perf_counter() + 600
        while True:
            event = self.events.get(timeout=max(0.01, deadline - time.perf_counter()))
            self.lines.append(event)
            if event.get('kind') == 'error':
                raise AssertionError(event)
            if predicate(event):
                return event

    def outcome(self):
        return self.wait(lambda v: v.get('kind') in ['snapshot_created', 'skipped_identical'])

    def settle(self):
        self.wait(lambda v: v.get('kind') == 'reconciliation' and v.get('status') == 'complete')

    def stop(self):
        self.process.send_signal(signal.SIGINT)
        self.wait(lambda v: v.get('status') == 'stopped')
        code = self.process.wait(timeout=30)
        assert code == 0, self.process.stderr.read()

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait()


def run(binary, work, payload, files, trial, idle_seconds, verify_all=True, reconcile_interval=60):
    source = work / 'source'
    source.mkdir()
    width = len(payload) // files
    names = ['payload.bin'] if files == 1 else [f'part-{i:04}.bin' for i in range(files)]
    for i, name in enumerate(names):
        (source / name).write_bytes(payload[i*width:(i+1)*width])
    target = source / names[files//2]
    expected = state(source)
    subprocess.run([str(binary), 'init'], cwd=work, check=True, capture_output=True)
    start = time.perf_counter()
    watcher = Watch(binary, work, reconcile_interval)
    samples = []
    versions = []
    try:
        baseline = watcher.outcome()
        assert baseline['kind'] == 'snapshot_created'
        samples.append({'phase': 'baseline', 'elapsed_seconds': time.perf_counter()-start,
                        'worker_seconds': baseline['elapsed_ms']/1000, 'growth': counts(work)})
        versions.append((baseline['snapshot_id'], expected))
        watcher.settle()
        before = counts(work)
        os.utime(target, None)
        written = time.perf_counter()
        same = watcher.outcome()
        assert same['kind'] == 'skipped_identical'
        samples.append({'phase': 'unchanged', 'elapsed_seconds': time.perf_counter()-written,
                        'worker_seconds': same['elapsed_ms']/1000, 'growth': counts(work)})
        watcher.settle()
        assert counts(work) == before and not (work/'.rewind/capture.pending').exists()
        for edit in range(3):
            with target.open('r+b') as stream:
                stream.seek(width//2+137)
                stream.write(hashlib.shake_256(f'auto-edit-{edit}'.encode()).digest(4096))
            written = time.perf_counter()
            result = watcher.outcome()
            assert result['kind'] == 'snapshot_created'
            samples.append({'phase': 'edit', 'edit': edit, 'elapsed_seconds': time.perf_counter()-written,
                            'worker_seconds': result['elapsed_ms']/1000, 'growth': counts(work)})
            watcher.settle()
            versions.append((result['snapshot_id'], state(source)))
        burst_start = time.perf_counter()
        for i in range(25):
            with target.open('r+b') as stream:
                stream.seek(width//2+137)
                stream.write(hashlib.shake_256(f'auto-burst-{i}'.encode()).digest(4096))
        written = time.perf_counter()
        result = watcher.outcome()
        assert result['kind'] == 'snapshot_created'
        samples.append({'phase': 'burst_25', 'elapsed_seconds': time.perf_counter()-written,
                        'since_first_write_seconds': time.perf_counter()-burst_start,
                        'worker_seconds': result['elapsed_ms']/1000, 'growth': counts(work)})
        watcher.settle()
        versions.append((result['snapshot_id'], state(source)))
        a = resources(watcher.process.pid)
        idle_start = time.perf_counter()
        time.sleep(idle_seconds)
        b = resources(watcher.process.pid)
        idle_elapsed = time.perf_counter()-idle_start
        idle = {'interval_seconds': idle_elapsed, 'before': a, 'after': b,
                'cpu_delta_seconds': b['cpu_seconds']-a['cpu_seconds'],
                'one_core_percent': 100*(b['cpu_seconds']-a['cpu_seconds'])/idle_elapsed}
        watcher.stop()
        assert counts(work)['snapshots'] == len(versions) == 5
        source.rename(work/'source-moved')
        restored_versions = versions if verify_all else versions[-1:]
        for i, (identifier, expected) in enumerate(restored_versions):
            subprocess.run([str(binary),'restore-snapshot',identifier,f'restore-{i}'],cwd=work,check=True,capture_output=True)
            assert state(work/f'restore-{i}') == expected
        verifier = binary.parent / 'examples' / 'verify_archive'
        archive_verification = [json.loads(subprocess.check_output(
            [str(verifier), str(work/'.rewind'), versions[-1][0]], text=True)) for _ in range(3)]
        # Restart while restoring the same canonical location: no new record.
        (work/'source-moved').rename(source)
        restart_start = time.perf_counter()
        restarted = Watch(binary,work,reconcile_interval)
        try:
            result = restarted.outcome()
            assert result['kind'] == 'skipped_identical'
            restart = {'elapsed_seconds': time.perf_counter()-restart_start, 'worker_seconds':result['elapsed_ms']/1000}
            restarted.settle()
            restarted.stop()
        finally:
            restarted.close()
        assert counts(work)['snapshots'] == 5 and not (work/'.rewind/capture.pending').exists()
        return {'files':files,'source_bytes':len(payload),'trial':trial,'samples':samples,'idle':idle,'restart':restart,
                'outcomes':[v for v in watcher.lines if v.get('kind') in ['snapshot_created','skipped_identical']],
                'shadow_diagnostics':[v for v in watcher.lines if v.get('kind') == 'shadow'],
                'historical_restores_verified':len(restored_versions), 'standalone_archive_verification':archive_verification}
    finally:
        watcher.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--trials',type=int,default=3)
    parser.add_argument('--mib',type=int,default=32)
    parser.add_argument('--many-files',type=int,default=256)
    parser.add_argument('--idle-seconds',type=float,default=2)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    size = args.mib*1024*1024
    if args.trials < 1 or not 1 <= args.mib <= 256 or args.many_files < 2 or size % args.many_files or args.idle_seconds < 1:
        parser.error('Use positive trials, 1..256 MiB, >=2 files dividing total bytes, and idle >=1 s')
    root = Path(__file__).resolve().parents[1]
    subprocess.run(['cargo','build','--release','--bins','--examples','--locked'],cwd=root,check=True)
    binary = root/'target/release/rewind'
    source_hash = {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').glob('*.rs'))}
    report = {'measured_at_utc':datetime.now(timezone.utc).isoformat(),
              'environment':{'platform':platform.platform(),'python':platform.python_version(),
                             'cpu':command_text(['sysctl','-n','machdep.cpu.brand_string']),
                             'memory_bytes':command_text(['sysctl','-n','hw.memsize']),
                             'rustc':command_text(['rustc','--version']),'cargo':command_text(['cargo','--version']),
                             'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()},
              'source_sha256':source_hash,
              'method':{'payload':'SHAKE256(rewind-auto-c-v1); split into equal distinct files','trials':args.trials,
                        'timing':'perf_counter write-close to snapshot/skipped JSON receipt, includes native delivery, 300-ms debounce, worker queue/lock, two full scans, validation and durable publication',
                        'baseline_timing':'process launch to baseline receipt includes native watcher setup and validation',
                        'worker_timing':'reported elapsed_ms covers lock/recovery, read-only probe, second verify/persistent scan, final metadata checks, and commit; excludes debounce',
                        'idle':'ps TIME delta (aggregate process CPU across threads, 0.01-s resolution) / interval; RSS endpoints, not peak or kernel cache',
                        'cache':'warm/uncontrolled; generated and SHA256-read before watcher; no eviction; APFS local temporary data',
                        'archive_verification':'separate warm full archive pass on final snapshot using verify_archive example; includes nodes, manifests, chunk and whole-file hashes; not a timed subcomponent of worker and must not be subtracted from worker timings',
                        'fast_mode':'not implemented; no strict/fast speedup can be measured',
                        'limitations':'small local dataset; idle windows short; no syscall tracing, power-loss tests or atomic source-state guarantee'},
              'samples':[]}
    payload = hashlib.shake_256(b'rewind-auto-c-v1').digest(size)
    with tempfile.TemporaryDirectory(prefix='rewind-auto-c-benchmark-') as name:
        for files in [1,args.many_files]:
            for trial in range(1,args.trials+1):
                work = Path(name)/f'{files}-{trial}'
                work.mkdir()
                report['samples'].append(run(binary,work,payload,files,trial,args.idle_seconds))
                print(f'PASS {files} files, trial {trial}: baseline, unchanged, 3 edits, burst, idle, 5 restores, restart dedup',flush=True)
    assert source_hash == {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').glob('*.rs'))}
    report['engine_source_unchanged']=True
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(f'Wrote {args.output}',flush=True)


if __name__ == '__main__':
    main()
