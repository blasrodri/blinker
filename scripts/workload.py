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


def force_final_link(project):
    """Make sure the build actually performs the link being captured.

    The build directory is shared between captures on purpose (finding 106), and
    the consequence is that a capture with no source change relinks *nothing* —
    cargo has a current binary and leaves it alone. The recorder then holds only
    the incidental links, a build script or a proc-macro dylib, and
    `largest_record` picks one of those and calls it the workload. It is the
    same failure as finding 106 wearing different clothes: the harness quietly
    measuring a different link from the one it claims.

    Touching the binary crate's own source is enough. It costs one small
    recompile, changes no content, and guarantees the final link happens.

    *Which* source has to come from cargo, not from a glob. `**/src/main.rs`
    misses `src/bin/main.rs`, which is where rust-analyzer's entry point lives
    — so the capture touched `xtask`, relinked only `xtask`, and produced a
    workload named after a program it had not linked (139). cargo already
    reports the exact path of every binary target; the glob was guessing at
    something that was available for the asking.
    """
    touched = [path for _, path in binary_targets(project)]
    for path in touched:
        if path.exists():
            path.touch()
    if not touched:
        fail(f"no binary crate found under {project} to force a link")


def install(source, destination):
    """Put `source` at `destination`, replacing what is there — by rename.

    Never by writing over the file in place. macOS invalidates the code
    signature of an executable that is modified while a process is executing
    it, and the invalidation attaches to the *inode*: every later `exec` of
    that path is killed with SIGKILL before any of its code runs. The bytes are
    fine, the signature verifies on disk, and a copy of the same file at
    another path runs — which is what makes it so hard to read. `rustc` reports
    it as `linking with ... failed: signal: 9 (SIGKILL)`.

    This mattered the moment blinker started a resident linker by default,
    because a capture now leaves a process running from this exact path and the
    next capture used to overwrite it. Renaming gives a new inode, so the
    running daemon keeps its own file and the new one is untouched.
    """
    staged = destination.with_suffix(destination.suffix + ".incoming")
    shutil.copy2(source, staged)
    os.replace(staged, destination)


def capture(project, records, linker, target_dir, profile):
    """Build `project` with blinker recording every link it is asked to do."""
    force_final_link(project)
    flags = f'["-C", "link-arg=--blinker-record-invocation={records}"]'
    cmd = [
        "cargo",
        "build",
        f"--target-dir={target_dir}",
        f"--config=target.{TARGET}.linker='{linker}'",
        f"--config=target.{TARGET}.rustflags={flags}",
        # The captured build must not use fat LTO, whatever the project's own
        # profile says. Under `lto = "fat"` rustc merges every crate itself and
        # hands the linker one object: blinker's self-link went from 62 inputs
        # and 692 objects to 3 inputs and 378. That is a real link, but it is
        # not the link this benchmark is about, and a linker benchmark whose
        # workload has three inputs measures almost nothing about linking.
        #
        # blinker's own release profile *does* use fat LTO — it is worth 7% on
        # the link — so without this the shipped binary's profile silently
        # reshapes the fixture that judges it.
        "--config=profile.release.lto=false",
        "--config=profile.release.codegen-units=16",
    ]
    if profile == "release":
        cmd.append("--release")
    print(f"  building {project} (this is the slow part)")
    run(cmd, cwd=project)


def binary_targets(project):
    """Every binary target of the workspace, as `(name, source path)`.

    One question to cargo, answering both "which links matter" and "which files
    to touch to make them happen". Asking it twice, in two different ways, is
    how those two answers came to disagree (139).
    """
    meta = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version=1"],
        cwd=project, capture_output=True, text=True,
    )
    if meta.returncode != 0:
        return []
    targets = []
    for package in json.loads(meta.stdout).get("packages", []):
        for target in package.get("targets", []):
            if "bin" in target.get("kind", []):
                targets.append((target["name"], Path(target["src_path"])))
    return targets


def package_binaries(project, preferred=None):
    """The project's binary targets, most-wanted first.

    A workspace usually has more than one, and they are not interchangeable:
    `blinker` is the linker and `blinker_corpus` is a diagnostic tool that
    happens to depend on more crates. Choosing by size picked the second one.
    """
    names = sorted(name for name, _ in binary_targets(project))
    # The one named like the project comes first unless told otherwise.
    head = preferred or project.name
    return sorted(names, key=lambda n: (n != head, n))


def largest_record(records, wanted=None):
    """The link this workload is about.

    Picked by *name* when the project's binary targets are known, and by input
    count only as a fallback. Input count is what this used to use, and it chose
    wrong twice: once a proc-macro dylib, once `blinker_corpus` instead of
    `blinker` — both real links, neither the one the benchmark claimed to be
    measuring. "The biggest link" is a proxy for "the link that matters", and a
    proxy that silently substitutes a different program is the failure finding
    106 was about.

    A cargo build records every link it performs: build scripts, proc-macro
    dylibs, each test binary. They are all real invocations and the corpus
    tooling wants them; a benchmark wants a *named* one.
    """
    # Cargo names a binary target `rust-analyzer`; the file rustc links is
    # `rust_analyzer-<hash>`. Comparing them literally meant the preferred name
    # never matched, the fallback picked `xtask`, and the capture announced a
    # rust-analyzer workload built from a 300-line build tool. That is the
    # silent substitution this function's docstring is about, arriving through
    # a hyphen.
    def canonical(name):
        return name.replace("-", "_")

    wanted = [canonical(w) for w in wanted] if wanted else wanted
    best, best_count = None, -1
    for path in sorted(Path(records).glob("*.json")):
        with open(path) as handle:
            record = json.load(handle)
        # A record whose argument vector names a file that no longer exists
        # cannot be replayed, and picking it produces a workload that fails
        # verification with an errno from ld64 rather than a useful message.
        # The recorder archives these now (`archive_side_files`); this stays as
        # the check that it did, because the failure mode is a *silently
        # different* workload rather than an error.
        argv = record.get("replay_argv") or record.get("argv") or []
        if any(a.startswith("/") and "/" in a[1:] and a.endswith("/list")
               and not os.path.exists(a) for a in argv):
            print(f"    skipping {path.name}: it names a file that is gone")
            continue
        # A binary target's link beats any number of inputs elsewhere.
        name = Path(record.get("output_path", "")).name
        stem = canonical(name.rsplit("-", 1)[0] if "-" in name else name)
        count = len(record.get("inputs") or [])
        # Preferred binary first, any binary second, anything else last. Two
        # binary targets in one workspace (`blinker` and `blinker_corpus`) is
        # enough for "is a binary" to still pick the wrong one.
        if wanted and stem == wanted[0]:
            count += 2_000_000
        elif wanted and stem in wanted:
            count += 1_000_000
        if count > best_count:
            best, best_count = record, count
    if best is None:
        fail(f"no records under {records} — did the build link anything?")
    if wanted:
        chosen = Path(best.get("output_path", "")).name
        stem = canonical(chosen.rsplit("-", 1)[0] if "-" in chosen else chosen)
        if stem not in wanted:
            fail(f"the chosen link is {chosen}, which is not one of this "
                 f"project's binaries ({', '.join(sorted(wanted))})")
        if stem != wanted[0]:
            print(f"  note: captured {stem}, not {wanted[0]}")
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


def objects_in(path):
    """How many object files an archive holds, without spawning `ar`.

    An `.rlib` is not a unit of anything: rust-analyzer's large crates are 256
    objects each and its leaf crates are one. "15 of 341 inputs changed" was
    read as a four percent blast radius when it was fifty-seven percent of the
    program (finding 143), and no amount of care with the *linker* would have
    caught that — the harness was reporting the wrong denominator.

    The `ar` format is a global header, then 60-byte member headers each
    followed by their data, padded to even. Both the BSD long-name form
    (`#1/<len>`, name stored in the data) and the plain form appear in rlibs.
    """
    if not path.endswith((".rlib", ".a")):
        return 1
    try:
        with open(path, "rb") as handle:
            if handle.read(8) != b"!<arch>\n":
                return 1
            count = 0
            while True:
                header = handle.read(60)
                if len(header) < 60:
                    return count
                name = header[:16].decode("ascii", "replace").strip()
                size = int(header[48:58].decode("ascii", "replace").strip() or 0)
                if name.startswith("#1/"):
                    extra = int(name[3:])
                    name = handle.read(extra).decode("ascii", "replace").rstrip("\0")
                    size -= extra
                if name.rstrip("/").endswith(".o"):
                    count += 1
                handle.seek(size + (size & 1), 1)
    except OSError:
        return 1


def count_objects(argv):
    """Objects, counting archive members — the unit the linker actually reads.

    "79 inputs" and "921 objects" are the same link. Which number a finding
    quotes changes what it appears to say about scale, so the manifest records
    both.
    """
    files = [Path(a) for a in argv if a.endswith((".o", ".rlib", ".a"))]
    # Counting every `ar -t` line included `lib.rmeta`, which the linker never
    # reads — three members per rlib, and about ten percent of the number this
    # manifest publishes as "objects".
    objects = sum(objects_in(str(path)) for path in files)
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
    parser.add_argument("--binary", help="which binary target's link to capture")
    options = parser.parse_args()

    if not BLINKER.exists():
        fail(f"{BLINKER} not built — run: cargo build --release")

    # Built beside the real one and moved into place at the end. Clearing the
    # destination first means a capture that fails — and they do; a build that
    # relinks nothing produces an unreplayable record — takes the existing
    # workload with it, and the next run reports "no workload" instead of the
    # error that actually happened.
    final = Path(options.out) / options.name
    destination = Path(options.out) / f".{options.name}.partial"
    shutil.rmtree(destination, ignore_errors=True)
    destination.mkdir(parents=True)

    # Copied, not referenced: the capture build rewrites target/release/blinker
    # when the project being captured is this repository. One shared path
    # rather than one per workload, for the same reason the staging directory
    # is shared — cargo fingerprints the linker it was told to use.
    linker = Path(options.out) / ".linker"
    linker.parent.mkdir(parents=True, exist_ok=True)
    install(BLINKER, linker)

    # One path for every capture, then moved: see the header. The linker copy
    # is per-workload and does not travel in rustflags, so it may stay put.
    staging = Path(options.out) / ".capture"
    shutil.rmtree(staging, ignore_errors=True)
    # One build directory across captures, and deliberately *not* cleared.
    #
    # A capture that builds from scratch re-emits every rlib in the project,
    # and rustc's codegen units come out in a different order — so two captures
    # of a one-line edit differ in thirteen crates the edit never touched. That
    # is not what a developer's edit loop does: cargo recompiles the edited
    # crate and its dependents and leaves the rest alone. Sharing the directory
    # makes the second capture an incremental rebuild, which is the thing being
    # measured.
    build = Path(options.out) / ".build"
    capture(Path(options.project).resolve(), staging, linker, build, options.profile)

    records = destination / "records"
    shutil.move(str(staging), str(records))

    record = largest_record(
        records,
        package_binaries(Path(options.project).resolve(), options.binary),
    )
    argv = replay_argv(record, destination / "link-output")
    argv = [a.replace(str(staging), str(records)) for a in argv]

    files, objects, size = count_objects(argv)
    if files == 0:
        fail("the largest recorded link reads no objects")

    print("  verifying the workload links")
    blinker_ms, blinker_size, ld64_ms = verify(argv, destination)

    # Verified in the staging directory, recorded against the final one: the
    # workload is built beside its own name and moved into place, so every path
    # written down now has to be the path it will have afterwards. Writing the
    # staging paths produced an `argv.txt` naming a directory that no longer
    # existed, and ld64 reported it as `undefined _main` — the objects were
    # simply not there, and an argument vector of missing files still looks like
    # a link.
    final_argv = [a.replace(str(destination), str(final)) for a in argv]
    (destination / "argv.txt").write_text("\n".join(final_argv) + "\n")
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

    # The build directory is shared and outlives this capture; see above.

    # Only now, with both linkers having accepted it, does it replace whatever
    # workload of this name was there before.
    shutil.rmtree(final, ignore_errors=True)
    shutil.move(str(destination), str(final))

    # And the recorded vector is checked against the filesystem it will be
    # replayed on, because "verified, then moved" is exactly the window in which
    # a workload can become unreplayable without anything noticing.
    output = final_argv[final_argv.index("-o") + 1]
    absent = [
        a
        for a in final_argv
        if a.startswith(str(final)) and a != output and not os.path.exists(a)
    ]
    if absent:
        fail(f"{len(absent)} archived path(s) do not exist after the move, "
             f"first: {absent[0]}")

    print(f"\n  {options.name}: {files} files, {objects} objects, "
          f"{size / 1024 / 1024:.1f} MB")
    print(f"  one link: blinker {blinker_ms:.0f} ms"
          + (f", ld64 {ld64_ms:.0f} ms" if ld64_ms else ""))
    print(f"\n  scripts/bench.py {final / 'argv.txt'}")


if __name__ == "__main__":
    main()
