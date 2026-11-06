#!/usr/bin/env python3
"""Small local CLI measurements, not a comparative benchmark. Python 3.11+."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import tempfile
import time
from datetime import datetime, timezone


def command_text(args):
    result = subprocess.run(args, capture_output=True, text=True, check=False)
    return result.stdout.strip() if result.returncode == 0 else "unavailable"


def measured(binary, work, args):
    log = work / "native-time.txt"
    darwin = platform.system() == "Darwin"
    flags = ["-l"] if darwin else ["-v"]
    command = [str(binary), *args]
    start = time.perf_counter()
    result = subprocess.run(
        ["/usr/bin/time", *flags, "-o", str(log), *command],
        cwd=work, capture_output=True, text=True,
        env={**os.environ, "LC_ALL": "C"}, check=False, timeout=180,
    )
    elapsed = time.perf_counter() - start
    if result.returncode:
        raise RuntimeError(f"{command!r}: {result.stderr}; timing: {log.read_text()}")
    raw = log.read_text()
    if darwin:
        cpu = re.search(r"([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys", raw)
        rss = re.search(r"(\d+)\s+maximum resident set size", raw)
        if not cpu or not rss:
            raise RuntimeError(f"Incomplete native time metrics: {raw}")
        native_real, user, system = map(float, cpu.groups())
        rss_bytes = int(rss.group(1))
    else:
        def value(label):
            found = re.search(re.escape(label) + r":\s*([\d.]+)", raw)
            if not found:
                raise RuntimeError(f"Missing {label}: {raw}")
            return float(found.group(1))
        user = value("User time (seconds)")
        system = value("System time (seconds)")
        rss_bytes = int(value("Maximum resident set size (kbytes)")) * 1024
        native_real = None
    return result.stdout.strip(), {
        "command": ["rewind", *args],
        "elapsed_seconds": elapsed,
        "native_real_seconds": native_real,
        "cpu_user_seconds": user,
        "cpu_system_seconds": system,
        "peak_rss_bytes": rss_bytes,
        "native_time_output": raw,
    }


def state(root):
    result = {"": None}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise RuntimeError(f"Unexpected symlink: {path}")
        relative = path.relative_to(root).as_posix()
        if path.is_dir():
            result[relative] = None
        else:
            with path.open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            result[relative] = {"length": path.stat().st_size, "sha256": digest}
    return result


def chunks(work):
    paths = [p for p in (work / ".rewind/chunks").rglob("*") if p.is_file()]
    return {"objects": len(paths), "payload_bytes": sum(p.stat().st_size for p in paths)}


def record(work, identifier):
    return json.loads((work / ".rewind/snapshots" / f"{identifier}.json").read_text())


def trial(binary, parent, mib, number):
    with tempfile.TemporaryDirectory(prefix="rewind-measure-", dir=parent) as temporary:
        work = Path(temporary)
        source = work / "source"
        (source / "docs").mkdir(parents=True)
        (source / "empty/deep").mkdir(parents=True)
        payload = source / "payload.bin"
        payload.write_bytes(hashlib.shake_256(b"rewind-phase2-e-payload-v1").digest(mib * 1024 * 1024))
        for index in range(16):
            (source / "docs" / f"{index:02}.txt").write_bytes(
                hashlib.shake_256(f"rewind-e-doc-{index}".encode()).digest(1024)
            )
        (source / "empty-file").write_bytes(b"")
        expected_a = state(source)
        measured(binary, work, ["init"])
        a, fresh = measured(binary, work, ["snapshot", "source"])
        chunks_a = chunks(work)
        same, unchanged = measured(binary, work, ["snapshot", "source"])
        chunks_same = chunks(work)
        assert record(work, a)["root_hash"] == record(work, same)["root_hash"]
        assert a != same and chunks_same == chunks_a
        # Replace 4096 bytes without changing length, away from the file edges.
        offset = mib * 1024 * 1024 // 2 + 137
        with payload.open("r+b") as stream:
            stream.seek(offset)
            original = stream.read(4096)
            stream.seek(offset)
            stream.write(bytes(value ^ 0x5A for value in original))
        expected_b = state(source)
        b, changed = measured(binary, work, ["snapshot", "source"])
        chunks_b = chunks(work)
        output, diff = measured(binary, work, ["diff", a, b])
        assert output == 'M\tfile\t"payload.bin"'
        output, equal_diff = measured(binary, work, ["diff", a, same])
        assert output == ""
        _, restore = measured(binary, work, ["restore-snapshot", b, "restored-b"])
        assert state(work / "restored-b") == expected_b
        # Also validate the old version; this second restore is not summarized.
        measured(binary, work, ["restore-snapshot", a, "restored-a"])
        assert state(work / "restored-a") == expected_a
        assert state(source) == expected_b
        assert not list(work.glob(".rewind-restore-*"))
        return {
            "payload_mib": mib, "trial": number,
            "source_files": sum(value is not None for value in expected_a.values()),
            "source_directories_including_root": sum(value is None for value in expected_a.values()),
            "source_bytes": sum(value["length"] for value in expected_a.values() if value is not None),
            "patch_bytes": 4096, "patch_offset": offset,
            "chunks_after_a": chunks_a, "chunks_after_identical": chunks_same,
            "chunks_after_b": chunks_b,
            "chunk_growth_objects": chunks_b["objects"] - chunks_a["objects"],
            "chunk_growth_payload_bytes": chunks_b["payload_bytes"] - chunks_a["payload_bytes"],
            "metrics": {"snapshot_fresh": fresh, "snapshot_identical": unchanged,
                        "snapshot_changed": changed, "diff_changed": diff,
                        "diff_identical": equal_diff, "restore_b": restore},
            "verification": "A/B SHA-256 file maps and empty-directory maps match; source unchanged",
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/rewind"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--sizes-mib", type=int, nargs="+", default=[8, 32])
    parser.add_argument("--work-parent", type=Path, default=Path(tempfile.gettempdir()))
    args = parser.parse_args()
    if platform.system() not in ("Darwin", "Linux"):
        parser.error("These measurements require macOS or Linux and /usr/bin/time")
    if args.trials < 1 or any(size < 1 or size > 256 for size in args.sizes_mib):
        parser.error("Use positive trial counts and payload sizes from 1 to 256 MiB")
    binary = args.binary.resolve(strict=True)
    parent = args.work_parent.resolve(strict=True)
    report = {
        "measured_at_utc": datetime.now(timezone.utc).isoformat(),
        "environment": {
            "platform": platform.platform(), "machine": platform.machine(),
            "python": platform.python_version(), "work_parent": str(parent),
            "cpu": command_text(["sysctl", "-n", "machdep.cpu.brand_string"]) if platform.system() == "Darwin" else platform.processor(),
            "ram_bytes": command_text(["sysctl", "-n", "hw.memsize"]) if platform.system() == "Darwin" else "unavailable",
            "df": command_text(["df", "-P", str(parent)]),
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        },
        "method": {
            "dataset": "SHAKE256 v1 deterministic payload; 16 distinct 1024-byte docs, one empty file, empty/deep dirs",
            "elapsed": "perf_counter around /usr/bin/time plus CLI process; includes launch overhead",
            "cpu": "native time user/system seconds; macOS printed precision 0.01 seconds",
            "memory": "native time per-command peak RSS normalized to bytes; excludes generator Python process and filesystem cache",
            "growth": "chunk object count and logical payload bytes; excludes metadata, directory/inode and physical allocation overhead",
            "cache": "warm/uncontrolled; data generated and SHA-256 validated before timing; no cache drop",
            "limitations": "small local serial runs; no competing-tool comparison or power-loss tests; sync/cache/system load affect results",
        },
        "samples": [],
    }
    for mib in args.sizes_mib:
        for number in range(1, args.trials + 1):
            sample = trial(binary, parent, mib, number)
            report["samples"].append(sample)
            print(f"PASS {mib} MiB trial {number}: +{sample['chunk_growth_objects']} chunks / +{sample['chunk_growth_payload_bytes']} payload bytes", flush=True)
    summary = []
    for mib in args.sizes_mib:
        samples = [s for s in report["samples"] if s["payload_mib"] == mib]
        for operation in samples[0]["metrics"]:
            metrics = [s["metrics"][operation] for s in samples]
            elapsed = [m["elapsed_seconds"] for m in metrics]
            summary.append({
                "payload_mib": mib, "operation": operation,
                "elapsed_median_seconds": statistics.median(elapsed),
                "elapsed_min_seconds": min(elapsed), "elapsed_max_seconds": max(elapsed),
                "cpu_user_median_seconds": statistics.median(m["cpu_user_seconds"] for m in metrics),
                "cpu_system_median_seconds": statistics.median(m["cpu_system_seconds"] for m in metrics),
                "peak_rss_median_bytes": statistics.median(m["peak_rss_bytes"] for m in metrics),
                "peak_rss_max_bytes": max(m["peak_rss_bytes"] for m in metrics),
            })
    report["summary"] = summary
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Wrote {args.output}", flush=True)


if __name__ == "__main__":
    main()
