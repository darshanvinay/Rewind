#!/usr/bin/env python3
"""Profile a disposable instrumented copy; never patch repository engine files."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
from measure_phase2 import command_text, measured, state, chunks, record


def fingerprint(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted((root / "src").glob("*.rs"))}


def run_case(binary, work, payload, files, instrumented):
    source = work / "source"
    source.mkdir(parents=True)
    width = len(payload) // files
    names = ["payload.bin"] if files == 1 else [f"part-{i:04}.bin" for i in range(files)]
    for i, name in enumerate(names):
        (source / name).write_bytes(payload[i * width:(i + 1) * width])
    del payload
    before = state(source)
    measured(binary, work, ["init"])
    stages = []
    ids = []
    for phase in ["fresh", "unchanged", "modified"]:
        if phase == "modified":
            target = source / names[files // 2]
            offset = width // 2 + 137
            with target.open("r+b") as stream:
                stream.seek(offset)
                old = stream.read(4096)
                assert len(old) == 4096
                stream.seek(offset)
                stream.write(bytes(value ^ 0x5A for value in old))
        out = work / f"profile-{phase}.json"
        prior = os.environ.pop("REWIND_PROFILE_OUT", None)
        if instrumented:
            os.environ["REWIND_PROFILE_OUT"] = str(out)
        try:
            identifier, metric = measured(binary, work, ["snapshot", "source"])
        finally:
            os.environ.pop("REWIND_PROFILE_OUT", None)
            if prior is not None:
                os.environ["REWIND_PROFILE_OUT"] = prior
        item = {"phase": phase, "metrics": metric,
                "root": record(work, identifier)["root_hash"], "chunk_store": chunks(work)}
        if instrumented:
            detail = json.loads(out.read_text())
            assert detail["unclosed_scopes"] == 0
            timings = detail["timings"]
            assert timings["capture_total"]["inclusive_ns"] == sum(v["exclusive_ns"] for v in timings.values())
            assert timings["file_ingest"]["calls"] == files
            assert detail["counters"]["source_bytes_read"] == width * files
            # On the optimized serial path, every temporary belongs to a newly
            # published object; verified hits perform no data writes/syncs.
            if detail["counters"].get("verified_reuse.chunk", 0):
                writes = sum(v["calls"] for k, v in timings.items() if k.startswith("data_write."))
                synced = sum(v["calls"] for k, v in timings.items() if k.startswith("file_fsync."))
                published = sum(v for k, v in detail["counters"].items() if k.startswith("published."))
                assert writes == synced == published
                if phase == "unchanged":
                    assert writes == 1 and detail["counters"]["verified_reuse.chunk"] == detail["counters"]["chunks"]
            item["profile"] = detail
        stages.append(item)
        ids.append(identifier)
    assert stages[0]["root"] == stages[1]["root"] != stages[2]["root"]
    assert stages[0]["chunk_store"] == stages[1]["chunk_store"]
    after = state(source)
    # Remove the recorded source so neither verification nor restore can rely
    # on source file reads. The expected maps remain in this Python process.
    source.rename(work / "source-moved")
    output, _ = measured(binary, work, ["diff", ids[0], ids[2]])
    assert output == f'M\tfile\t"{names[files // 2]}"'
    for suffix, identifier, expected in [("a", ids[0], before), ("b", ids[2], after)]:
        measured(binary, work, ["restore-snapshot", identifier, f"restore-{suffix}"])
        assert state(work / f"restore-{suffix}") == expected
    assert state(work / "source-moved") == after
    return stages


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--reference-binary", type=Path,
                        help="Optional separately saved pre-change release executable for paired comparisons")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--mib", type=int, default=32)
    parser.add_argument("--many-files", type=int, default=256)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    size = args.mib * 1024 * 1024
    if args.trials < 1 or args.mib < 1 or args.mib > 256 or args.many_files < 2 or size % args.many_files or size // args.many_files < 16384:
        parser.error("Use positive trials, 1..256 MiB, and a file count dividing the size with >=16 KiB/file")
    original = fingerprint(root)
    subprocess.run(["cargo", "build", "--release", "--locked"], cwd=root, check=True)
    optimized = root / "target/release/rewind"
    baseline = args.reference_binary.resolve(strict=True) if args.reference_binary else optimized
    if args.reference_binary and baseline == optimized:
        parser.error("The reference must be a separate saved executable, not the build output")
    report = {
        "measured_at_utc": datetime.now(timezone.utc).isoformat(),
        "environment": {"platform": platform.platform(), "python": platform.python_version(),
                        "cpu": command_text(["sysctl", "-n", "machdep.cpu.brand_string"]),
                        "rustc": command_text(["rustc", "--version"]),
                        "cargo": command_text(["cargo", "--version"]),
                        "baseline_sha256": hashlib.sha256(baseline.read_bytes()).hexdigest(),
                        "optimized_sha256": hashlib.sha256(optimized.read_bytes()).hexdigest(),
                        "reference_binary_supplied": args.reference_binary is not None},
        "source_sha256": original,
        "method": {
            "dataset": "SHAKE256(rewind-capture-profile-v1), split into one or many equal-sized distinct files; root directory only",
            "source_bytes": size, "many_files": args.many_files,
            "trials": args.trials, "patch": "XOR 4096 bytes at chosen file midpoint +137; middle file in many-file dataset",
            "cache": "warm/uncontrolled; generated then SHA-256 validated before each fresh capture; no cache eviction",
            "timers": "Instant wall timers in a disposable source copy; exclusive children subtracted; inclusive high-level stages overlap",
            "io_counts": "Rust read/write/sync_all calls and publication attempts; not an OS-wide syscall or physical-disk-write trace",
            "cpu_rss": "native /usr/bin/time; elapsed includes launcher/startup; instrumented elapsed also includes profile export",
            "validation": "unmodified and instrumented roots/chunk-store sizes equal; A/B restore maps verified after source renamed",
        },
        "samples": [],
    }
    with tempfile.TemporaryDirectory(prefix="rewind-capture-profile-") as temporary:
        temporary = Path(temporary)
        copy = temporary / "instrumented"
        subprocess.run([sys.executable, str(root / "scripts/capture_profile/instrument.py"),
                        "--root", str(root), "--output", str(copy)], check=True)
        subprocess.run(["cargo", "build", "--release", "--locked", "--manifest-path", str(copy / "Cargo.toml")], check=True)
        instrumented = copy / "target/release/rewind"
        report["environment"]["instrumented_sha256"] = hashlib.sha256(instrumented.read_bytes()).hexdigest()
        payload = hashlib.shake_256(b"rewind-capture-profile-v1").digest(size)
        for files in [1, args.many_files]:
            for trial in range(1, args.trials + 1):
                results = {}
                binaries = {"baseline": baseline, "instrumented": instrumented}
                if args.reference_binary:
                    binaries["optimized"] = optimized
                names = list(binaries)
                shift = (trial - 1) % len(names)
                order = names[shift:] + names[:shift]
                for name in order:
                    work = temporary / f"files-{files}-trial-{trial}-{name}"
                    work.mkdir()
                    results[name] = run_case(binaries[name], work, payload, files, name == "instrumented")
                for name, stages in results.items():
                    for a, b in zip(results["baseline"], stages):
                        assert a["root"] == b["root"] and a["chunk_store"] == b["chunk_store"]
                report["samples"].append({"files": files, "source_bytes": size, "directories": 1,
                                          "trial": trial, "execution_order": order, **results})
                print(f"PASS {files} files / {args.mib} MiB trial {trial}: fresh, unchanged, modified; roots and restores match", flush=True)
    assert original == fingerprint(root), "Engine source changed during profiling"
    report["engine_source_unchanged"] = True
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Wrote {args.output}", flush=True)


if __name__ == "__main__":
    main()
