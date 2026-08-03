#!/usr/bin/env python3
"""Compare blinker against ld64 on a captured link.

Why this exists rather than a `time` loop
-----------------------------------------

The first three attempts at this measurement were all wrong, in three
different ways, and each looked plausible:

1. Both linkers were passed a malformed argument list, failed instantly, and
   the harness reported the failure times as a 45% win for blinker. Output was
   redirected to /dev/null, so neither said it had failed.
2. Fixed, but the objects alone do not link — the rlibs carry `rust_panic` and
   the allocator shims — so both failed again, identically.
3. Fixed, but run as `cc` rather than `ld`, which measured process creation
   (17 ms of it) as though it were linking.

And once it produced numbers, four runs of the same binary on the same inputs
spread across 5% — enough to hide a 14 ms effect on a 46 ms link, which is
exactly what it did.

So this harness:

- **verifies every run succeeded** and produced a plausible output, and aborts
  rather than reporting a time for a failed link;
- **interleaves** the two linkers so machine drift affects both equally;
- **discards warmup** iterations rather than assuming the first is
  representative;
- **reports spread**, so a difference smaller than the noise is visible as
  such instead of being read as a result.

Usage:

    scripts/bench.py <captured-args-file> [--iterations N]

The args file is one linker argument per line, exactly as the driver passed
them — capture it with a shim linker that dumps "$@".
"""

import argparse
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BLINKER = REPO / "target" / "release" / "blinker"


def load_args(path):
    with open(path) as handle:
        return [line.rstrip("\n") for line in handle if line.strip()]


def real_ld_command(args):
    """The `ld` line `cc` would run, so the driver's spawn is not timed."""
    result = subprocess.run(["cc", "-###"] + args, capture_output=True, text=True)
    for line in result.stderr.splitlines():
        stripped = line.strip()
        if not stripped.startswith('"'):
            continue
        tokens = [t.strip('"') for t in stripped.split('" "')]
        if tokens and os.path.basename(tokens[0]).startswith("ld"):
            return tokens
    return None


def with_output(cmd, flag_index, output):
    out = list(cmd)
    out[flag_index] = output
    return out


def run_once(cmd, output):
    """Run, and insist it worked. A benchmark of a failing program is noise."""
    if os.path.exists(output):
        os.remove(output)
    start = time.perf_counter()
    result = subprocess.run(cmd, capture_output=True)
    elapsed = (time.perf_counter() - start) * 1000

    if result.returncode != 0:
        sys.exit(
            f"FAILED: {cmd[0]} exited {result.returncode}\n"
            f"{result.stderr.decode(errors='replace')[:800]}"
        )
    if not os.path.exists(output) or os.path.getsize(output) < 1024:
        sys.exit(f"FAILED: {cmd[0]} produced no usable output at {output}")
    return elapsed, os.path.getsize(output)


def summarise(name, samples, size):
    median = statistics.median(samples)
    spread = (max(samples) - min(samples)) / median * 100 if median else 0
    stdev = statistics.stdev(samples) if len(samples) > 1 else 0.0
    print(
        f"  {name:<10} {median:6.1f} ms  "
        f"(min {min(samples):.1f}, max {max(samples):.1f}, "
        f"sd {stdev:.1f}, spread {spread:.0f}%)  output {size / 1024:.0f} KB"
    )
    return median


def profile(args, options):
    """Per-stage medians, from blinker's own record.

    The stage numbers in findings 35 and 36 came from single runs. A single
    run cannot distinguish a stage from the noise around it, which is the
    mistake that produced a 1.6x ratio from two unlucky samples — so the
    breakdown that decides *what a cache should store* is measured the same
    way as the comparison that decides whether one is worth building.
    """
    import json
    import tempfile

    blinker_args = [str(BLINKER), "--blinker-no-daemon", "--blinker-internal"] + args
    out_index = blinker_args.index("-o") + 1
    blinker_args[out_index] = "/tmp/bench_profile"

    # Every stage the record reports. A stage missing from this table is not
    # absent from the link, it is silently folded into `unmeasured` — which is
    # where dead-strip sat for as long as it took someone to notice that 23% of
    # the link was unaccounted for.
    stages = {
        "read+parse": "link_read_and_parse_ms",
        "resolve": "link_resolve_ms",
        "layout": "link_layout_ms",
        "dead-strip": "link_dead_strip_ms",
        "relocate": "link_relocate_ms",
        "emit+sign": "link_emit_ms",
    }
    collected = {name: [] for name in stages}
    totals = []

    with tempfile.TemporaryDirectory() as scratch:
        record = os.path.join(scratch, "record.json")
        cmd = blinker_args + ["--blinker-json-diagnostics", record]
        for _ in range(options.warmup):
            run_once(cmd, "/tmp/bench_profile")
        for _ in range(options.iterations):
            run_once(cmd, "/tmp/bench_profile")
            with open(record) as handle:
                timings = json.load(handle)["timings"]
            for name, key in stages.items():
                if timings.get(key) is not None:
                    collected[name].append(timings[key])
            if timings.get("internal_link_ms") is not None:
                totals.append(timings["internal_link_ms"])

    if not totals:
        sys.exit("no internal-link timings recorded")

    total = statistics.median(totals)
    print(f"blinker internal link: {total:.1f} ms median "
          f"({options.iterations} iterations)\n")
    accounted = 0.0
    for name in stages:
        samples = collected[name]
        if not samples:
            continue
        median = statistics.median(samples)
        accounted += median
        sd = statistics.stdev(samples) if len(samples) > 1 else 0.0
        print(f"  {name:<12}{median:6.1f} ms  {median / total * 100:5.1f}%  sd {sd:.1f}")

    gap = total - accounted
    print(f"\n  {'accounted':<12}{accounted:6.1f} ms  {accounted / total * 100:5.1f}%")
    print(f"  {'unmeasured':<12}{gap:6.1f} ms  {gap / total * 100:5.1f}%")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("args_file")
    parser.add_argument("--iterations", type=int, default=15)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument(
        "--profile",
        action="store_true",
        help="report blinker's per-stage medians instead of comparing linkers",
    )
    options = parser.parse_args()

    args = load_args(options.args_file)
    if "-o" not in args:
        sys.exit("captured arguments contain no -o")

    if options.profile:
        profile(args, options)
        return

    ld = real_ld_command(args)
    if ld is None:
        sys.exit("could not find the ld line in `cc -###` output")

    ld_out_index = ld.index("-o") + 1
    ld_cmd = with_output(ld, ld_out_index, "/tmp/bench_ld64")

    blinker_args = [str(BLINKER), "--blinker-no-daemon", "--blinker-internal"] + args
    bl_out_index = blinker_args.index("-o") + 1
    bl_cmd = with_output(blinker_args, bl_out_index, "/tmp/bench_blinker")

    if not BLINKER.exists():
        sys.exit(f"{BLINKER} not built — run: cargo build --release")

    inputs = [a for a in args if a.endswith((".o", ".rlib", ".a"))]
    total = sum(os.path.getsize(p) for p in inputs if os.path.exists(p))
    print(f"inputs: {len(inputs)} files, {total / 1024 / 1024:.1f} MB")
    print(f"iterations: {options.iterations} (after {options.warmup} warmup)\n")

    for _ in range(options.warmup):
        run_once(ld_cmd, "/tmp/bench_ld64")
        run_once(bl_cmd, "/tmp/bench_blinker")

    # Interleaved: any drift in machine state hits both linkers equally.
    ld_samples, bl_samples = [], []
    ld_size = bl_size = 0
    for _ in range(options.iterations):
        elapsed, ld_size = run_once(ld_cmd, "/tmp/bench_ld64")
        ld_samples.append(elapsed)
        elapsed, bl_size = run_once(bl_cmd, "/tmp/bench_blinker")
        bl_samples.append(elapsed)

    ld_median = summarise("ld64", ld_samples, ld_size)
    bl_median = summarise("blinker", bl_samples, bl_size)

    ratio = bl_median / ld_median
    noise = max(
        (max(s) - min(s)) / statistics.median(s) for s in (ld_samples, bl_samples)
    )
    print(f"\n  blinker/ld64: {ratio:.2f}x   output ratio: {bl_size / ld_size:.2f}x")
    if abs(ratio - 1.0) < noise:
        print("  NOTE: the difference is smaller than the observed spread.")


if __name__ == "__main__":
    main()
