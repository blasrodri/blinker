#!/usr/bin/env python3
"""Time every link a real `cargo build` performs, three ways.

    scripts/build-links.py --record ~/src/someproject   # capture a build's links
    scripts/build-links.py --records /tmp/corpus-check  # time what was captured

Why this exists
---------------

Every other harness here measures *one* link. `bench.py` compares one argument
vector against the system linker, `relink.py` times one target's edit loop, and
both were pointed at the largest link that could be found, on the reasoning
that the worst case is where the work is.

A developer does not run one link. `cargo build` on this workspace runs
sixteen, and their input counts are

    22 22 22 22 22 22 22 23 23 23 23 25 44 79 81 132

— a median of 23. The 5,637-object rust-analyzer link that every number in
FINDINGS is taken from is not the typical case; it is the tail, and optimising
against it alone answers a question nobody asked.

It also measures the thing the product is actually substituted for. `rustc`
invokes the C compiler driver, so the comparison is `cc <argv>` against
`blinker <argv>` — the driver's own spawn included on both sides, because a
user replacing one with the other pays it either way.

The three arms
--------------

    ld64 (cc)            what happens without this project
    blinker, no daemon   what happens after `linker = "…/blinker"`, which is
                         all most users will do
    blinker + daemon     what the product is for

The middle arm is the one that matters for adoption and the one no benchmark
here had. The daemon is opt-in and nothing starts it, so it is what a user
gets.
"""

import argparse
import glob
import json
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BLINKER = REPO / "target" / "release" / "blinker"


def fail(message):
    sys.exit(f"build-links: {message}")


def record(project, into):
    """Build `project` through blinker with recording on."""
    os.makedirs(into, exist_ok=True)
    flags = [
        "-C",
        f"link-arg=--blinker-record-invocation={into}",
        "-C",
        "link-arg=--blinker-internal",
    ]
    command = [
        "cargo",
        "build",
        "--config",
        f'target.aarch64-apple-darwin.linker = "{BLINKER}"',
        "--config",
        "target.aarch64-apple-darwin.rustflags = "
        + json.dumps([f for f in flags]).replace('"', '"'),
    ]
    print(f"  building {project} …")
    result = subprocess.run(command, cwd=project)
    if result.returncode:
        fail("the build failed; nothing was recorded")


def load(records):
    """Every replayable link, and the ones blinker refused."""
    replayable, delegated = [], []
    for path in sorted(glob.glob(os.path.join(records, "*.json"))):
        held = json.load(open(path))
        if held.get("fallback_reason"):
            delegated.append((path, held))
        else:
            replayable.append((path, held))
    return replayable, delegated


def timed(run):
    started = time.perf_counter()
    run()
    return (time.perf_counter() - started) * 1000


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--record", help="build this project and record its links first")
    parser.add_argument("--records", default="/tmp/blinker-build-links")
    parser.add_argument("--iterations", type=int, default=2)
    options = parser.parse_args()

    if not BLINKER.exists():
        fail(f"{BLINKER} does not exist; cargo build --release first")
    if options.record:
        record(options.record, options.records)

    replayable, delegated = load(options.records)
    if not replayable:
        fail(f"no recorded links under {options.records} — pass --record first")

    scratch = Path("/tmp/blinker-build-links-out")
    scratch.mkdir(exist_ok=True)

    def with_cc():
        for index, (_, held) in enumerate(replayable):
            argv = list(held["replay_argv"])
            if "-o" in argv:
                argv[argv.index("-o") + 1] = str(scratch / f"cc-{index}")
            done = subprocess.run(["/usr/bin/cc"] + argv, capture_output=True)
            if done.returncode:
                fail("cc failed: " + done.stderr.decode(errors="replace")[:400])

    def with_blinker(extra):
        def run():
            for path, _ in replayable:
                done = subprocess.run(
                    [str(BLINKER)]
                    + extra
                    + ["--blinker-internal", f"--blinker-replay-invocation={path}"],
                    capture_output=True,
                )
                if done.returncode:
                    fail("blinker failed: " + done.stderr.decode(errors="replace")[:400])

        return run

    sizes = sorted(h.get("counters", {}).get("input_count") or 0 for _, h in replayable)
    print(f"\n  {len(replayable)} links, {sizes[len(sizes) // 2]} inputs at the median, "
          f"{max(sizes)} at the largest")
    if delegated:
        print(f"  {len(delegated)} delegated to the system linker and not timed here:")
        for path, held in delegated:
            print(f"    {Path(held['output_path']).name}  ({held['fallback_reason']})")
    print()

    # Warmup discarded on every arm: the first pass pays the page cache for
    # 800 MB of inputs, and attributing that to whichever arm ran first is how
    # a harness reports an ordering as a result.
    arms = [("ld64 (cc)", with_cc), ("blinker, no daemon", with_blinker([]))]
    for label, run in arms:
        run()
        for _ in range(options.iterations):
            print(f"  {label:<22} {timed(run):7.0f} ms")

    daemon = subprocess.Popen(
        [str(BLINKER), "--blinker-daemon-serve"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        time.sleep(1.2)
        run = with_blinker(["--blinker-daemon"])
        run()
        for _ in range(options.iterations):
            print(f"  {'blinker + daemon':<22} {timed(run):7.0f} ms")
    finally:
        daemon.terminate()
        daemon.wait(timeout=5)
    print()


if __name__ == "__main__":
    main()
