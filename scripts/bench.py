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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("args_file")
    parser.add_argument("--iterations", type=int, default=15)
    parser.add_argument("--warmup", type=int, default=3)
    options = parser.parse_args()

    args = load_args(options.args_file)
    if "-o" not in args:
        sys.exit("captured arguments contain no -o")

    ld = real_ld_command(args)
    if ld is None:
        sys.exit("could not find the ld line in `cc -###` output")

    ld_out_index = ld.index("-o") + 1
    ld_cmd = with_output(ld, ld_out_index, "/tmp/bench_ld64")

    blinker_args = [str(BLINKER), "--blinker-internal"] + args
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
