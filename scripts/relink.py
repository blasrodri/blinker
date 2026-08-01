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

The pair is the fixture
-----------------------

What this measures is not a workload, it is a *pair* of them: the build before
an edit and the build after it. Capturing only the second half and pairing it
with whatever `target/workloads/<name>` happened to hold made every run compare
against a different baseline — the base capture was taken at some earlier hour,
against some earlier state of the shared build directory, and the blast radius
came out 2 on one run and 3 on the next. That is not noise in the linker, it is
noise in the fixture, and it is the kind that looks like a result.

So the pair is captured consecutively — base, edit, edited — and then kept.
Later runs reuse it, which is what makes two measurements comparable at all;
`--recapture` takes a fresh one when the source has moved on.
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
sys.path.insert(0, str(Path(__file__).resolve().parent))
from workload import objects_in  # noqa: E402  one definition, two callers (139)
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

# A body-only edit: the constant inside an existing, live function.
#
# Two earlier versions of this were both wrong, in opposite directions, and both
# looked plausible:
#
#   appended `#[allow(dead_code)] fn`      0 of 6842 addresses changed
#   appended `#[used]`-kept private fn  8498 of 6877 addresses changed
#   appended `pub fn` (TOUCH, above)     252 of 6877 addresses changed
#
# The first was deleted again by dead-strip, so the edit never reached the
# output. The second repartitioned rustc's codegen units, which renames symbols
# across the whole crate — an edit far *larger* than adding a public function.
# Appending an item to a Rust file is not a body edit, whatever the item is.
#
# So this edits a body in place: `blinker_diagnostics::relink_seam` exists in
# the source for exactly this purpose, and what changes is one literal inside
# it. No item is added, no partition moves, no symbol is renamed.
SEAM = "std::hint::black_box(relink_seam("
SEAM_FILE = "crates/diagnostics/src/lib.rs"


def body_edit(project):
    """Rewrite the seam constant, returning (path, original bytes, edited bytes)."""
    path = project / SEAM_FILE
    text = path.read_text()
    at = text.find(SEAM)
    if at < 0:
        fail(f"{SEAM_FILE} has no relink seam — see LinkRecord::delegated")
    start = at + len(SEAM)
    end = text.index(")", start)
    edited = text[:start] + "0x0000_0000_0000_0001" + text[end:]
    if edited == text:
        fail("the seam was already edited; the pair would measure nothing")
    return path, text, edited



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


def pair_descriptor(options, project, source, profile):
    """What a captured pair has to agree with to be reusable.

    A `--body-only` pair and an exported-symbol pair are different experiments
    (see TOUCH_PRIVATE), and so are two pairs that edited different files. Reuse
    is only sound when all of that matches, so it is written down and compared
    rather than assumed.
    """
    return {
        "workload": options.workload,
        "project": str(project),
        "profile": profile,
        "edit": str(source),
        "mode": "private" if options.body_only else "exported",
    }


def capture_pair(options, project, source, profile, descriptor):
    """Capture the before and after builds back to back, and keep them.

    Consecutive matters. Both captures share one build directory, so the second
    is the incremental rebuild the edit actually causes (finding 106); a base
    captured hours earlier, against a build directory that has since moved,
    pairs the edit against a different program.
    """
    edited = Path(options.out) / f"{options.workload}-edited"
    if options.body_only:
        # In place, in the seam: see the comment above `SEAM`.
        target, before, after = body_edit(project)
        what = f"{target.relative_to(project)} (seam)"
    else:
        target, before = source, source.read_text()
        after = before + TOUCH
        what = str(source.relative_to(project))
    print(f"  capturing the pair: {what}, unedited then edited")
    capture(options.workload, project, profile, options.out)
    try:
        target.write_text(after)
        capture(edited.name, project, profile, options.out)
    finally:
        target.write_text(before)
    (edited / "pair.json").write_text(json.dumps(descriptor, indent=2) + "\n")
    return edited


def materialise_pair(options, project, source, profile):
    """The captured pair, reused when it still describes this experiment."""
    edited = Path(options.out) / f"{options.workload}-edited"
    descriptor = pair_descriptor(options, project, source, profile)
    stamp = edited / "pair.json"
    if not options.recapture and stamp.exists():
        if json.loads(stamp.read_text()) == descriptor:
            print(f"  reusing the captured pair ({descriptor['mode']} edit)"
                  f" — --recapture to take a fresh one")
            return edited
        print("  the captured pair describes a different experiment; recapturing")
    return capture_pair(options, project, source, profile, descriptor)


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
    parser.add_argument("--body-only", action="store_true",
                        help="edit a private item, so no exported symbol changes")
    parser.add_argument("--daemon", action="store_true",
                        help="link through a resident linker, if one is running")
    parser.add_argument("--relocations", action="store_true",
                        help="also reuse per-object relocations, to read the hit rate")
    parser.add_argument("--no-cache", action="store_true",
                        help="relink without the incremental cache, for comparison")
    parser.add_argument("--stage-stat", choices=("median", "min"), default="median",
                        help="statistic for the stage table; `min` is stable "
                             "under machine load, `median` reflects it")
    parser.add_argument("--recapture", action="store_true",
                        help="take a fresh pair of captures instead of reusing one")
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

    edited = materialise_pair(options, project, source, manifest["profile"])

    # The two captures must describe the same link. They pick their record by
    # input count, and an incremental build that relinked something else would
    # hand back a different program entirely — which reads as an enormous blast
    # radius rather than as the harness losing its place.
    twin = json.loads((edited / "manifest.json").read_text())
    if twin["output_name"] != manifest["output_name"]:
        fail(f"the pair captured two different links: {manifest['output_name']}"
             f" and {twin['output_name']} — try --recapture")

    argv, before = inputs_of(base / "argv.txt")
    _, after = inputs_of(edited / "argv.txt")
    changed, missing = differing(before, after)
    if not changed:
        fail("the rebuild produced identical inputs — the edit changed nothing")

    # Both denominators, because they disagree by an order of magnitude and the
    # object count is the one that bounds how much work the link can avoid.
    changed_objects = sum(objects_in(path) for path, _ in changed)
    total_objects = sum(objects_in(path) for path in before)
    print(f"  blast radius: {len(changed)} of {len(before)} inputs changed"
          + (f", {len(missing)} unmatched" if missing else ""))
    print(f"                {changed_objects} of {total_objects} objects"
          f" ({100 * changed_objects / max(total_objects, 1):.0f}%)")
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
        cmd = [str(BLINKER)]
        if options.daemon:
            cmd.append("--blinker-daemon")
        cmd.append("--blinker-internal")
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

    # One statistic per stage across every iteration, not the last record.
    #
    # Printing medians for `wall` and `link` and then a breakdown from one
    # arbitrary run means the two disagree whenever that run was an outlier —
    # and under load they are common. A stage table taken from a single slow
    # sample reads as a discovery about the linker.
    #
    # Which statistic is a question about the machine, not about the linker.
    # A median measures the linker plus whatever else the machine was doing;
    # on a busy one it drifts with the load and two runs an hour apart are not
    # comparable. A minimum is the closest this can get to "the machine got out
    # of the way", and it is stable under interference — noise only ever adds
    # time. Stage minima come from different iterations and so do not sum to
    # the link minimum, which is the honest cost of the choice.
    def stage_stat(key):
        pick = min if options.stage_stat == "min" else statistics.median
        merged = {}
        for name in {k for r in records for k in r[key]}:
            values = [r[key][name] for r in records
                      if isinstance(r[key].get(name), (int, float))]
            if values:
                merged[name] = pick(values)
        return merged

    timings = stage_stat("timings")
    counters = records[-1]["counters"]
    total = timings.get("internal_link_ms") or 0.0
    # Stage medians do not sum to the median total — each is the middle of its
    # own distribution. `unmeasured` absorbs that, so it is now a residual and
    # not only unaccounted work; it stays small when the run is clean and grows
    # when it is not, which is worth seeing either way.
    links = [r["timings"]["internal_link_ms"] for r in records]

    print(f"\n  {options.iterations} edit relinks, alternating the changed inputs\n")
    print(f"  wall  {statistics.median(walls):6.1f} ms   "
          f"(min {min(walls):.1f}, max {max(walls):.1f})")
    print(f"  link  {statistics.median(links):6.1f} ms   "
          f"(min {min(links):.1f}, max {max(links):.1f})\n")
    if options.stage_stat == "min":
        print("  stages below are per-stage minima, not medians\n")

    accounted = 0.0
    # `stub_parse` runs *inside* `read_and_parse`, on its own thread. Printed
    # for its own sake and excluded from the sum: adding an overlapped half to
    # the stage containing it double-counts, and the first version of this made
    # `unmeasured` negative — which is at least a number that announces itself.
    overlapped = {"stub_parse", "digest", "atoms", "liveness", "group", "traverse", "strip_build",
                  # Both run inside the relocate timer. Counting them again
                  # made `unmeasured` negative, which is the one way a profile
                  # can announce that it does not add up.
                  "address_table", "cache_plan",
                  "placements", "personality", "unwind_size", "commons",
                  "eh_frame", "tables", "unwind",
                  "emit_layout", "emit_contents", "emit_linkedit",
                  "emit_assemble", "emit_uuid", "emit_sign",
                  "address_map", "contents", "synthetic", "apply"}
    for name in ["read_and_parse", "stub_parse", "resolve", "layout", "dead_strip",
                 "digest", "atoms", "liveness", "group", "traverse", "strip_build",
                 "prepare", "placements", "personality", "unwind_size", "commons", "accounting", "address_table", "address_diff", "relocate", "emit", "emit_layout", "emit_contents",
                 "emit_linkedit", "emit_assemble", "emit_uuid", "emit_sign",
                 "address_map", "contents", "synthetic", "eh_frame", "tables", "unwind", "apply", "symbols", "survey",
                 "cache_load", "cache_plan",
                 "cache_build", "cache_store"]:
        value = timings.get(f"link_{name}_ms")
        if value is None:
            continue
        if name not in overlapped:
            accounted += value
        marker = ""
        if name == "stub_parse":
            marker = "  (inside read_and_parse)"
        elif name.startswith("emit_"):
            marker = "  (inside emit)"
        elif name in ("address_map", "contents", "synthetic", "apply"):
            marker = "  (inside relocate)"
        elif name in ("placements", "personality", "unwind_size", "commons"):
            marker = "  (inside prepare)"
        elif name in ("group", "traverse"):
            marker = "  (inside liveness)"
        elif name in ("atoms", "liveness", "strip_build"):
            marker = "  (inside dead_strip)"
        elif name in ("eh_frame", "tables", "unwind"):
            marker = "  (inside synthetic)"
        elif name in ("address_table", "cache_plan"):
            marker = "  (inside relocate)"
        print(f"    {name:<16}{value:6.2f} ms  {value / total * 100:5.1f}%{marker}")
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

    held = counters.get("inputs_held")
    if held is not None:
        print(f"  session: {held} inputs held, {counters.get('inputs_read')} read;"
              f" extraction {'replayed' if counters.get('replayed_extraction') else 'recomputed'},"
              f" resolution {'held' if counters.get('held_resolution') else 'redone'}")
        changes = counters.get("interface_changes")
        if changes:
            import os as _os
            first = counters.get("first_interface_change") or ""
            print(f"           {changes} interface(s) moved, first: {_os.path.basename(first)}")

    moved = counters.get("reach_moved")
    if moved is not None:
        print(f"  reachability: {moved}/{counters.get('reach_total')} objects' "
              f"projection moved")

    changed_addr = counters.get("changed_addresses")
    if changed_addr is not None:
        total_addr = counters.get("total_addresses") or 1
        print(f"  addresses: {changed_addr}/{total_addr} changed"
              f" ({changed_addr / total_addr * 100:.2f}%)")

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

    # The pair stays: it is the fixture, and re-taking it per run is what made
    # consecutive measurements incomparable.


if __name__ == "__main__":
    main()
