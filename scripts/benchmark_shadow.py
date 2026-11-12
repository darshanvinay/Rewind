#!/usr/bin/env python3
"""Time read-only shadow construction separately from full source-root scans."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import re
import subprocess
import tempfile
from measure_phase2 import command_text

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output',type=Path,required=True)
args=parser.parse_args()
root=Path(__file__).resolve().parents[1]
subprocess.run(['cargo','build','--release','--locked','--example','benchmark_shadow'],cwd=root,check=True)
binary=root/'target/release/examples/benchmark_shadow'
fingerprints={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').glob('*.rs'))}
with tempfile.TemporaryDirectory(prefix='rewind-shadow-time-') as temp:
    log=Path(temp)/'native-time.txt'
    result=subprocess.run(['/usr/bin/time','-l','-o',str(log),str(binary)],text=True,capture_output=True,check=True,timeout=900)
    print(result.stderr,end='',flush=True)
    raw=log.read_text()
    times=re.search(r'([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys',raw)
    rss=re.search(r'(\d+)\s+maximum resident set size',raw)
    assert times and rss
    report=json.loads(result.stdout)
    report.update({'measured_at_utc':datetime.now(timezone.utc).isoformat(),
                   'environment':{'platform':platform.platform(),'cpu':command_text(['sysctl','-n','machdep.cpu.brand_string']),
                                  'memory_bytes':command_text(['sysctl','-n','hw.memsize']),'rustc':command_text(['rustc','--version']),
                                  'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()},
                   'source_sha256':fingerprints,
                   'whole_process_metrics':{'elapsed_seconds':float(times.group(1)),'cpu_user_seconds':float(times.group(2)),
                                            'cpu_system_seconds':float(times.group(3)),'peak_rss_bytes':int(rss.group(1)),
                                            'includes':'dataset generation, all warmups/candidates/oracles, and temporary-directory cleanup',
                                            'native_output':raw}})
assert fingerprints=={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').glob('*.rs'))}
args.output.write_text(json.dumps(report,indent=2)+'\n')
print(f'Wrote {args.output}')
