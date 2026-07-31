#!/usr/bin/env python3
"""Interleaved A/B of two blinker binaries on one captured link.

    scripts/ab.py <argv-file> <before-binary> <after-binary>
    scripts/ab.py <argv-file> <binary> <binary>          # the noise floor

Why not wall clock
------------------

The obvious harness spawns each binary and times the process, and that is what
this script did for its first several findings. On the 61-file workload the
process arm spreads 42% around its median while the *link inside it* spreads
3%. Roughly 20 ms of a 60 ms run is spawn, dyld, and the kernel handing over
59 MB of page cache — real cost, but cost that does not change when the linker
changes, so it is 20 ms of pure variance laid over the thing being measured.

So both are reported: `wall` is what a build feels, `link` is what a change to
the linker can move. A difference that shows up in `link` and not in `wall` is
not a fake result, it is a real one being drowned; a difference that shows up
in `wall` and not in `link` is a change to startup or output size, and worth
knowing under that description rather than as a linker win.

The noise floor
---------------

Passing the same binary as both arms measures the harness rather than the
change, and the answer is the smallest difference this workload can resolve.
Finding 92 established that three consecutive changes had landed under a floor
nobody had measured; run this first, and read every later number against it.

Everything else is the discipline scripts/bench.py earned the hard way:
interleave so machine drift hits both arms equally, discard warmup, verify
every run produced a real binary, and report spread so a difference inside the
noise is visible as one.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def load_argv(path, output):
    argv = [line.rstrip("\n") for line in open(path) if line.strip()]
    if "-o" not in argv:
        sys.exit(f"{path} contains no -o")
    # Both arms write the same path: the output's base name is signed into the
    # image, so differing names would differ by design rather than by change.
    argv[argv.index("-o") + 1] = str(output)
    return argv


def run(binary, argv, output, record):
    if os.path.exists(output):
        os.remove(output)
    cmd = [str(binary), "--blinker-internal", "--blinker-json-diagnostics", str(record)]
    start = time.perf_counter()
    result = subprocess.run(cmd + argv, capture_output=True)
    wall = (time.perf_counter() - start) * 1000
    if result.returncode != 0:
        sys.exit(f"FAILED {binary}: rc={result.returncode}\n"
                 f"{result.stderr.decode(errors='replace')[:800]}")
    if not os.path.exists(output) or os.path.getsize(output) < 1024:
        sys.exit(f"FAILED {binary}: no usable output at {output}")
    with open(record) as handle:
        timings = json.load(handle)["timings"]
    link = timings.get("internal_link_ms")
    if link is None:
        sys.exit(f"FAILED {binary}: it delegated instead of linking internally")
    return wall, link, os.path.getsize(output)


def report(label, before, after):
    a, b = statistics.median(before), statistics.median(after)
    spread = max((max(s) - min(s)) / statistics.median(s) * 100 for s in (before, after))
    print(f"  {label:<6}{a:7.1f} -> {b:6.1f} ms   {b - a:+6.1f} ms  "
          f"({b / a:.3f}x)   sd {statistics.stdev(before):.1f}/"
          f"{statistics.stdev(after):.1f}, spread {spread:.0f}%")
    return b - a


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("argv_file")
    parser.add_argument("before")
    parser.add_argument("after")
    parser.add_argument("--iterations", type=int, default=12)
    parser.add_argument("--warmup", type=int, default=3)
    options = parser.parse_args()

    arms = [("before", options.before), ("after", options.after)]
    for _, binary in arms:
        if not Path(binary).exists():
            sys.exit(f"{binary} does not exist")

    with tempfile.TemporaryDirectory() as scratch:
        output = Path(scratch) / "ab-out"
        record = Path(scratch) / "record.json"
        argv = load_argv(options.argv_file, output)

        wall = {name: [] for name, _ in arms}
        link = {name: [] for name, _ in arms}
        size = {name: 0 for name, _ in arms}
        for iteration in range(options.warmup + options.iterations):
            for name, binary in arms:
                one_wall, one_link, one_size = run(binary, argv, output, record)
                if iteration >= options.warmup:
                    wall[name].append(one_wall)
                    link[name].append(one_link)
                    size[name] = one_size

    same = os.path.realpath(options.before) == os.path.realpath(options.after)
    print(f"\n{options.iterations} iterations after {options.warmup} warmup, "
          f"interleaved" + ("   [NOISE FLOOR: both arms are one binary]" if same else ""))
    print(f"{options.argv_file}\n")
    report("wall", wall["before"], wall["after"])
    report("link", link["before"], link["after"])
    print(f"\n  output {size['before'] / 1024:.0f} KB -> {size['after'] / 1024:.0f} KB"
          f"  ({size['after'] / size['before']:.3f}x)")


if __name__ == "__main__":
    main()
