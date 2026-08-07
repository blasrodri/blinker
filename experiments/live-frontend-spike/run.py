#!/usr/bin/env python3
"""Run the S0 matrix and reduce it to the distributions the decision needs.

Path A is measured here rather than in the driver, because it is *about* the
process. The driver is re-executed with `--once`, and what is timed is the
whole execve-to-exit: that is the number a developer pays today for a compiler
that starts fresh every edit. Path B is the same driver asked for many
iterations in one process, so the difference between them is process startup
and loaded-dylib state and nothing else — the two stop at the identical point
in compilation, which a comparison against `cargo check` could not claim.

    ./run.py --fixtures small medium large --iterations 30
"""

import argparse
import json
import platform
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPIKE = HERE / "target" / "release" / "spike"

EDITS = [
    "body_arith",
    "body_existing_call",
    "body_new_generic",
    "signature",
    "type_layout",
]


def machine() -> dict:
    def ask(*command):
        out = subprocess.run(command, capture_output=True, text=True)
        return out.stdout.strip()

    return {
        "chip": ask("sysctl", "-n", "machdep.cpu.brand_string"),
        "cores": ask("sysctl", "-n", "hw.ncpu"),
        "memory_gb": round(int(ask("sysctl", "-n", "hw.memsize") or 0) / 1e9, 1),
        "macos": platform.mac_ver()[0],
        "rustc": ask("rustc", "+nightly-2026-07-27", "-vV").replace("\n", " | "),
        "filesystem": ask("df", "-T", "apfs", "-h", str(HERE)).splitlines()[-1:],
        "captured_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }


def spike(fixture: Path, edit: str, iterations: int, warmup: int, once=False) -> list:
    command = [
        str(SPIKE),
        "--fixture", str(fixture),
        "--edit", edit,
        "--iterations", str(iterations),
        "--warmup", str(warmup),
    ]
    if once:
        command.append("--once")
    result = subprocess.run(command, capture_output=True, text=True)
    raw = result.stdout
    start = raw.find("[")
    if start < 0:
        print(f"    {edit}: the driver produced no records", file=sys.stderr)
        print(result.stderr[-1500:], file=sys.stderr)
        return []
    return json.loads(raw[start:])


def quantiles(values: list) -> dict:
    if not values:
        return {}
    ordered = sorted(values)

    def at(fraction):
        index = min(len(ordered) - 1, int(round(fraction * (len(ordered) - 1))))
        return round(ordered[index], 3)

    return {
        "n": len(ordered),
        "p50": at(0.50),
        "p95": at(0.95),
        "p99": at(0.99),
        "min": round(ordered[0], 3),
        "max": round(ordered[-1], 3),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", nargs="+", default=["small", "medium", "large"])
    parser.add_argument("--edits", nargs="+", default=EDITS)
    parser.add_argument("--iterations", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--path-a-runs", type=int, default=8)
    parser.add_argument("--out", default=str(HERE / "results"))
    options = parser.parse_args()

    out = Path(options.out)
    out.mkdir(parents=True, exist_ok=True)
    meta = machine()
    (out / "machine.json").write_text(json.dumps(meta, indent=2) + "\n")
    print(f"  {meta['chip']}, {meta['cores']} cores, macOS {meta['macos']}\n")

    summary = []
    for name in options.fixtures:
        fixture = HERE / "fixtures" / name
        if not (fixture / "fixture.json").exists():
            print(f"  {name}: no fixture, skipped")
            continue
        print(f"  {name}")
        for edit in options.edits:
            if not (fixture / "variants" / f"{edit}.rs").exists():
                continue
            records = spike(fixture, edit, options.iterations, options.warmup)
            good = [r for r in records if not r["error"]]
            if not good:
                reason = records[0]["error"] if records else "no records"
                print(f"    {edit:20s} FAILED: {reason}")
                summary.append(
                    {"fixture": name, "edit": edit, "error": reason}
                )
                continue

            # Path A: the same work, one session per process, timed from
            # outside so that execve and dyld are inside the number.
            wall = []
            for _ in range(options.path_a_runs):
                started = time.perf_counter()
                spike(fixture, edit, 1, 0, once=True)
                wall.append((time.perf_counter() - started) * 1e3)

            row = {
                "fixture": name,
                "edit": edit,
                "required_frontend_ms": quantiles(
                    [r["total_required_frontend_ms"] for r in good]
                ),
                "expand_ms": quantiles([r["expand_ms"] for r in good]),
                "analysis_ms": quantiles([r["analysis_ms"] for r in good]),
                "hot_mir_ms": quantiles([r["hot_mir_ms"] for r in good]),
                "path_d_closure_ms": quantiles([r["hot_closure_ms"] for r in good]),
                "path_c_whole_crate_ms": quantiles(
                    [r["whole_crate_mono_ms"] for r in good]
                ),
                "abi_layout_ms": quantiles([r["abi_layout_ms"] for r in good]),
                "path_a_process_ms": quantiles(wall),
                "mono_items_examined": good[0]["mono_items_examined"],
                "whole_crate_mono_items": good[0]["whole_crate_mono_items"],
            }
            summary.append(row)
            print(
                f"    {edit:20s} frontend p50 {row['required_frontend_ms']['p50']:8.1f}"
                f"  p95 {row['required_frontend_ms']['p95']:8.1f}"
                f"   D {row['path_d_closure_ms']['p50']:7.3f} ({row['mono_items_examined']})"
                f"   C {row['path_c_whole_crate_ms']['p50']:8.1f}"
                f" ({row['whole_crate_mono_items']})"
                f"   A {row['path_a_process_ms']['p50']:8.1f}"
            )
            (out / f"{name}-{edit}.json").write_text(
                json.dumps(records, indent=2) + "\n"
            )

    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(f"\n  {out / 'summary.json'}")


if __name__ == "__main__":
    main()
