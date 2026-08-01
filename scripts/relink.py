#!/usr/bin/env python3
"""Time the metric the product exists for: relinking after a one-crate edit.

    scripts/relink.py self                    # uses target/workloads/self
    scripts/relink.py self --edit crates/diagnostics/src/lib.rs

What makes this hard to measure honestly
----------------------------------------

The first attempt at this number reported 1.38 ms and full reuse, and it was
measuring the *second* link. One edit, then relink: the first run rebuilds, and
every run after it sees inputs that have stopped changing and replays a cached
image. The measurement said the incremental path was near-free because it was
timing a link that did not happen.

So every iteration here must contain a real edit. This captures the workload
twice — once as it is, once after touching a source file — and then alternates
the inputs that actually differ between the two builds. rustc keeps an rlib's
filename stable across source edits (the hash in it comes from the crate's
identity, not its text), so the two captures differ in content at the same
paths, which is exactly what a developer's rebuild produces.

The edit's blast radius is reported rather than assumed: touching one crate
recompiles everything downstream of it, and how many inputs that is decides
what any reuse number means.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BLINKER = REPO / "target" / "release" / "blinker"
# A real item, not a comment. The first version of this appended a comment and
# the rebuild produced byte-identical rlibs: rustc hashes the crate's meaning,
# not its text, so a comment-only edit is not an edit as far as anything
# downstream can tell. `inline(never)` keeps it from evaporating into its
# callers — it has none, and a function nobody calls still has to be compiled,
# placed and then dead-stripped, which is the work an edit creates.
TOUCH = """
#[doc(hidden)]
#[inline(never)]
pub fn blinker_relink_touch() -> u64 {
    0x9e37_79b9_7f4a_7c15
}
"""


def fail(message):
    sys.exit(f"relink: {message}")


def inputs_of(argv_file):
    argv = [line.rstrip("\n") for line in open(argv_file) if line.strip()]
    return argv, [a for a in argv if a.endswith((".o", ".rlib", ".a"))]


def default_edit_target(project):
    """A crate near the bottom of the graph, so the edit has a blast radius.

    Editing a leaf binary crate recompiles one object and reuses everything;
    that is the easy case and it flatters the linker. The default is the
    library everything else depends on.
    """
    for candidate in ["crates/diagnostics/src/lib.rs", "src/lib.rs", "src/main.rs"]:
        if (project / candidate).exists():
            return project / candidate
    found = sorted(project.glob("**/src/lib.rs"))
    if not found:
        fail(f"no source file to edit under {project}")
    return found[0]


def capture(name, project, profile, out):
    result = subprocess.run(
        [sys.executable, str(REPO / "scripts" / "workload.py"), name,
         f"--project={project}", f"--profile={profile}", f"--out={out}"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        fail(f"capturing the edited build failed:\n{result.stdout}{result.stderr}")


def archived_name(path):
    """The input's own name, without the archive's ordering prefix.

    Inputs are archived as `0029-libfoo-<hash>.rlib`. The number is the
    position in the argument vector, and an edit that adds or removes an object
    shifts every one after it — so matching on the whole name would report a
    two-object edit as if the entire link had changed.
    """
    name = Path(path).name
    prefix, _, rest = name.partition("-")
    return rest if prefix.isdigit() and rest else name


def differing(before, after):
    """Inputs present in both captures whose contents changed.

    Matched by name: the two captures archive into different directories, and
    the name is what rustc holds stable across an edit — provided both builds
    saw identical rustflags, which is why `workload.py` records through one
    fixed path.
    """
    index = {archived_name(p): p for p in after}
    changed, missing = [], []
    for path in before:
        twin = index.get(archived_name(path))
        if twin is None:
            missing.append(path)
            continue
        if Path(path).read_bytes() != Path(twin).read_bytes():
            changed.append((path, twin))
    return changed, missing


def link(cmd, output, record):
    if os.path.exists(output):
        os.remove(output)
    start = time.perf_counter()
    result = subprocess.run(cmd, capture_output=True)
    wall = (time.perf_counter() - start) * 1000
    if result.returncode != 0 or not os.path.exists(output):
        fail(f"the link failed: rc={result.returncode}\n"
             f"{result.stderr.decode(errors='replace')[:800]}")
    return wall, json.load(open(record))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("workload")
    parser.add_argument("--edit", help="source file to touch (default: a root crate)")
    parser.add_argument("--out", default=str(REPO / "target" / "workloads"))
    parser.add_argument("--iterations", type=int, default=10)
    parser.add_argument("--warmup", type=int, default=2)
    parser.add_argument("--relocations", action="store_true",
                        help="also reuse per-object relocations, to read the hit rate")
    parser.add_argument("--no-cache", action="store_true",
                        help="relink without the incremental cache, for comparison")
    parser.add_argument("--keep", action="store_true",
                        help="keep the edited capture instead of removing it")
    options = parser.parse_args()

    base = Path(options.out) / options.workload
    manifest_path = base / "manifest.json"
    if not manifest_path.exists():
        fail(f"no workload at {base} — run: scripts/workload.py {options.workload}")
    manifest = json.loads(manifest_path.read_text())
    project = Path(manifest["project"])

    source = Path(options.edit) if options.edit else default_edit_target(project)
    if not source.is_absolute():
        source = project / source
    if not source.exists():
        fail(f"{source} does not exist")

    edited_name = f"{options.workload}-edited"
    original = source.read_bytes()
    print(f"  editing {source.relative_to(project)} and rebuilding")
    try:
        source.write_bytes(original + TOUCH.encode())
        capture(edited_name, project, manifest["profile"], options.out)
    finally:
        source.write_bytes(original)

    argv, before = inputs_of(base / "argv.txt")
    _, after = inputs_of(Path(options.out) / edited_name / "argv.txt")
    changed, missing = differing(before, after)
    if not changed:
        fail("the rebuild produced identical inputs — the edit changed nothing")

    print(f"  blast radius: {len(changed)} of {len(before)} inputs changed"
          + (f", {len(missing)} unmatched" if missing else ""))
    for path, _ in changed[:6]:
        print(f"    {Path(path).name}")
    if len(changed) > 6:
        print(f"    ... and {len(changed) - 6} more")

    # Both versions are copied aside: the loop overwrites the workload's own
    # inputs, and a workload that ends up holding half of each build is worse
    # than one that was never captured.
    with tempfile.TemporaryDirectory() as scratch:
        scratch = Path(scratch)
        versions = []
        for index, (path, twin) in enumerate(changed):
            a, b = scratch / f"{index}.a", scratch / f"{index}.b"
            shutil.copyfile(path, a)
            shutil.copyfile(twin, b)
            versions.append((Path(path), a, b))

        output, record = scratch / "relink-out", scratch / "record.json"
        argv[argv.index("-o") + 1] = str(output)
        cmd = [str(BLINKER), "--blinker-internal"]
        if not options.no_cache:
            cmd.append("--blinker-cache-relocations"
                       if options.relocations else "--blinker-cache")
        cmd += ["--blinker-json-diagnostics", str(record)] + argv

        walls, records = [], []
        try:
            for iteration in range(options.warmup + options.iterations):
                for target, a, b in versions:
                    shutil.copyfile(a if iteration % 2 else b, target)
                wall, produced = link(cmd, output, record)
                if iteration >= options.warmup:
                    walls.append(wall)
                    records.append(produced)
        finally:
            for target, a, _ in versions:
                shutil.copyfile(a, target)

    timings = records[-1]["timings"]
    counters = records[-1]["counters"]
    total = timings.get("internal_link_ms") or 0.0
    links = [r["timings"]["internal_link_ms"] for r in records]

    print(f"\n  {options.iterations} edit relinks, alternating the changed inputs\n")
    print(f"  wall  {statistics.median(walls):6.1f} ms   "
          f"(min {min(walls):.1f}, max {max(walls):.1f})")
    print(f"  link  {statistics.median(links):6.1f} ms   "
          f"(min {min(links):.1f}, max {max(links):.1f})\n")

    accounted = 0.0
    for name in ["read_and_parse", "stub_parse", "resolve", "layout", "dead_strip",
                 "relocate", "emit", "cache_load", "cache_plan",
                 "cache_build", "cache_store"]:
        value = timings.get(f"link_{name}_ms")
        if value is None:
            continue
        accounted += value
        print(f"    {name:<16}{value:6.2f} ms  {value / total * 100:5.1f}%")
    print(f"    {'unmeasured':<16}{total - accounted:6.2f} ms  "
          f"{(total - accounted) / total * 100:5.1f}%")

    retained = counters.get("contributions_retained")
    if retained is not None:
        moved = counters.get("contributions_moved") or 0
        total = retained + moved
        print(f"\n  placement: {retained}/{total} contributions kept their address"
              f" ({retained / total * 100:.0f}%), {moved} moved")
        stale = counters.get("contributions_moved_unchanged")
        if stale is not None:
            verdict = "the invariant holds" if stale == 0 else "INVARIANT BROKEN"
            print(f"  of those, {stale} belonged to inputs that did not change"
                  f" — {verdict}")

    reused = counters.get("reused_inputs")
    if reused is not None:
        objects = reused + (counters.get("changed_inputs") or 0)
        relocations = counters.get("reused_relocations")
        work = counters.get("total_relocations")
        print(f"\n  reused {reused}/{objects} objects", end="")
        if relocations is not None and work:
            print(f", {relocations}/{work} relocations "
                  f"({relocations / work * 100:.0f}%)", end="")
        print()

    if not options.keep:
        shutil.rmtree(Path(options.out) / edited_name, ignore_errors=True)


if __name__ == "__main__":
    main()
