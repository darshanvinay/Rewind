#!/usr/bin/env python3
"""Isolated release CLI walkthrough, with byte-for-byte historical restoration."""
from pathlib import Path
import subprocess
import tempfile
from measure_phase2 import state

binary = Path(__file__).resolve().parents[1] / 'target/release/rewind'
with tempfile.TemporaryDirectory(prefix='rewind-release-smoke-') as name:
    work = Path(name)
    def cli(*args, success=True):
        result = subprocess.run([str(binary), *args], cwd=work, capture_output=True, text=True)
        assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
        return result.stdout.strip()
    cli('--help')
    cli('--version')
    source = work / 'project'
    (source / 'src').mkdir(parents=True)
    (source / 'empty').mkdir()
    (source / 'src/note.txt').write_bytes(b'before\n')
    (source / 'remove').write_bytes(b'delete me')
    (source / 'binary').write_bytes(bytes(range(256)))
    cli('init')
    manifest = cli('store', 'project/src/note.txt')
    cli('verify', manifest)
    cli('restore', manifest, 'recovered-note')
    assert (work / 'recovered-note').read_bytes() == b'before\n'
    expected_a = state(source)
    a = cli('snapshot', 'project')
    (source / 'src/note.txt').write_bytes(b'after\n')
    (source / 'added').write_bytes(b'new')
    (source / 'remove').unlink()
    expected_b = state(source)
    b = cli('snapshot', 'project')
    assert a != b and a in cli('snapshots') and b in cli('snapshots')
    diff = cli('diff', a, b)
    assert all(kind in diff for kind in ['A\t', 'D\t', 'M\t'])
    assert cli('diff', a, a) == ''
    source.rename(work / 'source-removed')
    for snapshot, output, expected in [(a, 'recovered-a', expected_a), (b, 'recovered-b', expected_b)]:
        cli('restore-snapshot', snapshot, output)
        assert state(work / output) == expected
        cli('restore-snapshot', snapshot, output, success=False)
    cli('restore-snapshot', 'invalid', 'invalid-output', success=False)
    cli('watch', 'source-removed', '--mode', 'fast', success=False)
    assert not (work / '.rewind/capture.pending').exists()
print('PASS release CLI walkthrough: file verify/restore, two snapshots, diff, historical restores without source, existing-output and invalid-mode rejection')
