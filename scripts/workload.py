#!/usr/bin/env python3
"""Materialise a durable, replayable link workload.

Why this exists
---------------

Every performance number in FINDINGS was taken on a workload that no longer
exists. The `corpus/` directory holds thirteen recorded invocations and each
one names an inputs directory under `/private/tmp/.../scratchpad/`; all
thirteen are gone. The records survived because they are small text; the 89 MB
of object files they describe did not, because they were written where the
operating system reclaims them.

That is not a filing accident, it is the reason finding 92 could not be
answered. Three consecutive changes measured at or under the noise floor of a
60-input link, and the obvious next question — does the 921-object workload
still see them? — needed a workload that could be *rebuilt*, not one that
happened to still be lying around.

So this script builds one from nothing but the repository and cargo:

    scripts/workload.py self                    # blinker linking itself
    scripts/workload.py rg --project ~/src/ripgrep

It writes `target/workloads/<name>/` holding an `argv.txt` that
`scripts/bench.py` consumes directly, an `inputs/` directory with a copy of
every object and archive the link reads, and a `manifest.json` recording what
was captured and how big it is. `target/` is gitignored and lives in the
repository rather than in a temporary directory, so a workload survives
sessions and can be regenerated in one command when it does not.

How it captures
---------------

blinker occupies the `linker=` position (D4), and already knows how to archive
the inputs of an invocation — `--blinker-record-invocation` exists precisely
because rustc deletes the object files the instant the link returns. This
script drives that machinery rather than reimplementing it with a shell shim:
a cargo build with blinker configured as the linker, then the record with the
most inputs is the final binary's link.

Three details that are not incidental:

- **The linker is a copy.** Building blinker with `target/release/blinker` as
  the linker would have cargo rewrite the binary it is currently executing.
  The copy is taken first and never touched again.
- **The recording directory is the same string for every capture**, and the
  results are moved into place afterwards. The path travels in `RUSTFLAGS`, and
  rustflags feed the crate metadata hash — so recording two builds into
  differently-named directories renames every rlib and object between them, and
  two captures of the same project come out looking entirely unrelated. That is
  what `scripts/relink.py` needs them not to do.
- **The build uses its own target directory**, so capturing a workload cannot
  invalidate the repository's build or be invalidated by it.
- **The captured workload is verified to link** by both ld64 and blinker
  before it is written. A workload that does not link is worse than none: it
  produces timings (bench.py catches this, and did once already) and it
  produces them fast.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BLINKER = REPO / "target" / "release" / "blinker"
TARGET = "aarch64-apple-darwin"


def fail(message):
    sys.exit(f"workload: {message}")


def run(cmd, **kwargs):
    result = subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    if result.returncode != 0:
        fail(
            f"{cmd[0]} exited {result.returncode}\n"
            f"  {' '.join(str(c) for c in cmd[:6])} ...\n"
            f"{result.stdout[-2000:]}{result.stderr[-4000:]}"
        )
    return result


def capture(project, records, linker, target_dir, profile):
    """Build `project` with blinker recording every link it is asked to do."""
    flags = f'["-C", "link-arg=--blinker-record-invocation={records}"]'
    cmd = [
        "cargo",
        "build",
        f"--target-dir={target_dir}",
        f"--config=target.{TARGET}.linker='{linker}'",
        f"--config=target.{TARGET}.rustflags={flags}",
    ]
    if profile == "release":
        cmd.append("--release")
    print(f"  building {project} (this is the slow part)")
    run(cmd, cwd=project)


def largest_record(records):
    """The final binary's link, which is the one with the most inputs.

    A cargo build records every link it performs — build scripts, proc-macro
    dylibs, each test binary. They are all real invocations and the corpus
    tooling wants them; a *benchmark* wants the biggest one, because the
    question a benchmark answers is about scale.
    """
    best, best_count = None, -1
    for path in sorted(Path(records).glob("*.json")):
        with open(path) as handle:
            record = json.load(handle)
        count = len(record.get("inputs") or [])
        if count > best_count:
            best, best_count = record, count
    if best is None:
        fail(f"no records under {records} — did the build link anything?")
    return best


def replay_argv(record, output):
    """The recorded argument vector, pointed at the archived inputs.

    `replay_argv` is written by the recorder and already names the copies. It
    is `argv` that names the originals, and the originals are what vanish.
    """
    argv = record.get("replay_argv") or record.get("argv")
    if not argv:
        fail("the record has no argument vector")
    argv = list(argv)
    if "-o" not in argv:
        fail("the recorded invocation has no -o")
    argv[argv.index("-o") + 1] = str(output)
    return argv


def measure(cmd, output):
    """One timed run that must succeed and must produce a real binary."""
    if os.path.exists(output):
        os.remove(output)
    start = time.perf_counter()
    result = subprocess.run(cmd, capture_output=True)
    elapsed = (time.perf_counter() - start) * 1000
    if result.returncode != 0:
        return None, result.stderr.decode(errors="replace")[:600]
    if not os.path.exists(output) or os.path.getsize(output) < 1024:
        return None, "produced no usable output"
    return elapsed, os.path.getsize(output)


def ld64_command(argv):
    """The `ld` line `cc` builds from these driver arguments."""
    result = subprocess.run(["cc", "-###"] + argv, capture_output=True, text=True)
    for line in result.stderr.splitlines():
        stripped = line.strip()
        if not stripped.startswith('"'):
            continue
        tokens = [token.strip('"') for token in stripped.split('" "')]
        if tokens and os.path.basename(tokens[0]).startswith("ld"):
            return tokens
    return None


def verify(argv, scratch):
    """Both linkers must accept the workload before it is recorded as one."""
    blinker_out = scratch / "verify-blinker"
    elapsed, detail = measure(
        [str(BLINKER), "--blinker-internal"] + replay_argv_with(argv, blinker_out),
        blinker_out,
    )
    if elapsed is None:
        fail(f"the captured workload does not link with blinker: {detail}")
    blinker_ms, blinker_size = elapsed, detail

    ld = ld64_command(argv)
    ld64_ms = None
    if ld is not None and "-o" in ld:
        ld_out = scratch / "verify-ld64"
        ld = list(ld)
        ld[ld.index("-o") + 1] = str(ld_out)
        ld64_ms, _ = measure(ld, ld_out)
        if ld64_ms is None:
            fail("the captured workload does not link with ld64")
    return blinker_ms, blinker_size, ld64_ms


def replay_argv_with(argv, output):
    out = list(argv)
    out[out.index("-o") + 1] = str(output)
    return out


def count_objects(argv):
    """Objects, counting archive members — the unit the linker actually reads.

    "79 inputs" and "921 objects" are the same link. Which number a finding
    quotes changes what it appears to say about scale, so the manifest records
    both.
    """
    files = [Path(a) for a in argv if a.endswith((".o", ".rlib", ".a"))]
    objects = 0
    for path in files:
        if path.suffix == ".o":
            objects += 1
            continue
        listing = subprocess.run(
            ["/usr/bin/ar", "-t", str(path)], capture_output=True, text=True
        )
        objects += sum(1 for line in listing.stdout.splitlines() if line.strip())
    size = sum(p.stat().st_size for p in files if p.exists())
    return len(files), objects, size


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("name", help="what to call this workload")
    parser.add_argument(
        "--project",
        default=str(REPO),
        help="crate to build and capture the link of (default: this repository)",
    )
    parser.add_argument("--out", default=str(REPO / "target" / "workloads"))
    parser.add_argument("--profile", choices=["debug", "release"], default="release")
    parser.add_argument(
        "--keep-build",
        action="store_true",
        help="keep the cargo target directory (gigabytes) instead of removing it",
    )
    options = parser.parse_args()

    if not BLINKER.exists():
        fail(f"{BLINKER} not built — run: cargo build --release")

    destination = Path(options.out) / options.name
    if destination.exists():
        shutil.rmtree(destination)
    destination.mkdir(parents=True)

    # Copied, not referenced: the capture build rewrites target/release/blinker
    # when the project being captured is this repository. One shared path
    # rather than one per workload, for the same reason the staging directory
    # is shared — cargo fingerprints the linker it was told to use.
    linker = Path(options.out) / ".linker"
    linker.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(BLINKER, linker)

    # One path for every capture, then moved: see the header. The linker copy
    # is per-workload and does not travel in rustflags, so it may stay put.
    staging = Path(options.out) / ".capture"
    shutil.rmtree(staging, ignore_errors=True)
    build = destination / "build"
    capture(Path(options.project).resolve(), staging, linker, build, options.profile)

    records = destination / "records"
    shutil.move(str(staging), str(records))

    record = largest_record(records)
    argv = replay_argv(record, destination / "link-output")
    argv = [a.replace(str(staging), str(records)) for a in argv]

    files, objects, size = count_objects(argv)
    if files == 0:
        fail("the largest recorded link reads no objects")

    print("  verifying the workload links")
    blinker_ms, blinker_size, ld64_ms = verify(argv, destination)

    (destination / "argv.txt").write_text("\n".join(argv) + "\n")
    manifest = {
        "name": options.name,
        "project": str(Path(options.project).resolve()),
        "profile": options.profile,
        "output_name": Path(record.get("output_path", "?")).name,
        "input_files": files,
        "objects": objects,
        "input_bytes": size,
        "output_bytes": blinker_size,
        "verify_blinker_ms": round(blinker_ms, 1),
        "verify_ld64_ms": round(ld64_ms, 1) if ld64_ms else None,
    }
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

    if not options.keep_build:
        shutil.rmtree(build, ignore_errors=True)

    print(f"\n  {options.name}: {files} files, {objects} objects, "
          f"{size / 1024 / 1024:.1f} MB")
    print(f"  one link: blinker {blinker_ms:.0f} ms"
          + (f", ld64 {ld64_ms:.0f} ms" if ld64_ms else ""))
    print(f"\n  scripts/bench.py {destination / 'argv.txt'}")


if __name__ == "__main__":
    main()
