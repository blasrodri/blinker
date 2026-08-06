#!/usr/bin/env python3
"""Materialise a C link workload, for the half of the corpus rustc cannot reach.

Why a synthetic project
-----------------------

Every workload in `target/workloads/` is a Rust link, and a Rust link has a
shape the linker is measurably sensitive to: most of its inputs are toolchain
`.rlib`s whose *names* carry a content hash, so proving them unchanged costs a
`stat`. A C or C++ link has none of that. Every input is a plain `.o` or `.a`
whose name says nothing about its bytes, so every freshness question is a read
and a BLAKE3 of the whole file.

Finding 232 turned a quarter of a ripgrep relink into a measurement, and the
remedy it left behind — a stamp beside each content hash — could not be
demonstrated on any workload in the corpus, because none of them has enough
content-keyed input for it to matter. pulsevm, the largest, is 294 rlibs and 18
objects. This exists so that claim has something to be true or false against.

It is synthetic, and that is a real limitation: the *shape* is faithful (many
translation units, debug info, one edited file per cycle) but the code is
generated, so nothing here should be read as a statement about a real C++
codebase's link. What it does measure honestly is cost proportional to input
bytes and input count, which is what the freshness probe is.

    scripts/c-workload.py             # 200 units into target/workloads/cproj
    scripts/c-workload.py --units 800

Writes `argv.txt` in the same form `bench.py` and the byte-identity oracle
consume, so it drops into the existing harnesses unchanged.
"""

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def write_sources(src: Path, units: int, body_lines: int):
    """One function per unit, with a body long enough to make a real object.

    A stub-sized `.o` would make the whole workload smaller than a single
    rustc object and measure nothing.
    """
    src.mkdir(parents=True, exist_ok=True)
    for unit in range(units):
        body = "\n".join(f"  acc = acc * 31 + {line} + x;" for line in range(body_lines))
        (src / f"m{unit}.c").write_text(
            f"long f{unit}(long x) {{\n  long acc = {unit};\n{body}\n  return acc;\n}}\n"
        )
    declarations = "\n".join(f"long f{unit}(long);" for unit in range(units))
    calls = " + ".join(f"f{unit}(argc)" for unit in range(units))
    (src / "main.c").write_text(
        f"{declarations}\n"
        f"int main(int argc, char **argv) {{ (void)argv; return (int)(({calls}) & 1); }}\n"
    )


def compile_all(src: Path):
    """Every unit at once; `cc` is the slow part and it parallelises."""
    running = [
        subprocess.Popen(
            ["cc", "-arch", "arm64", "-mmacosx-version-min=11.0", "-g", "-c",
             str(source), "-o", str(source.with_suffix(".o"))]
        )
        for source in sorted(src.glob("*.c"))
    ]
    if any(process.wait() for process in running):
        sys.exit("cc failed on at least one unit")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--units", type=int, default=200)
    parser.add_argument("--body-lines", type=int, default=400)
    parser.add_argument("--name", default="cproj")
    parser.add_argument("--project", default="/tmp/blinker-cproj")
    options = parser.parse_args()

    project = Path(options.project)
    src = project / "src"
    if project.exists():
        shutil.rmtree(project)
    write_sources(src, options.units, options.body_lines)
    compile_all(src)

    sdk = subprocess.run(
        ["xcrun", "--show-sdk-path"], capture_output=True, text=True, check=True
    ).stdout.strip()
    objects = sorted(str(path) for path in src.glob("*.o"))
    argv = (
        ["-o", str(project / "prog")]
        + objects
        + ["-lSystem", "-syslibroot", sdk, "-arch", "arm64",
           "-platform_version", "macos", "11.0", "11.0"]
    )

    out = REPO / "target" / "workloads" / options.name
    out.mkdir(parents=True, exist_ok=True)
    (out / "argv.txt").write_text("\n".join(argv) + "\n")

    # A workload that does not link is not a workload; say so now rather than
    # in whichever harness reads it next.
    linked = subprocess.run(
        [str(REPO / "target" / "release" / "blinker"), "--blinker-internal", *argv],
        capture_output=True,
    )
    if linked.returncode:
        sys.exit(f"the workload does not link:\n{linked.stderr.decode()[-800:]}")

    total = sum(Path(path).stat().st_size for path in objects) / 1e6
    print(f"\n  {options.name}: {len(objects)} objects, {total:.1f} MB")
    print(f"\n  scripts/bench.py {out / 'argv.txt'}\n")


if __name__ == "__main__":
    main()
