#!/usr/bin/env python3
"""The safety case: does a DIRECT verdict actually leave dependents unchanged?

The classifier's claim is that a DIRECT edit needs no downstream
recompilation. That claim is checkable, offline, by doing the recompilation
anyway and looking at whether it produced different machine code.

    edit a library crate
        → rebuild every dependent with cargo
        → compare each dependent's `__TEXT,__text` bytes against the pristine build

If a dependent's code changed, the edit was observable downstream and a DIRECT
verdict was **wrong**. This is not a hot-path check — it is far slower than the
rebuild it justifies. It is the evidence that the fast path is allowed to
exist.

The comparison is per object file inside each dependent's rlib, on the text
section only. An rlib is not byte-comparable: it embeds metadata, a
dependency hash, and symbol tables that move for reasons that have nothing to
do with whether downstream code must change. The executable code is the thing
the claim is about.

Note what the oracle can and cannot prove. It shows that on *this* corpus, a
DIRECT verdict did not change any dependent's code. It cannot show that no
such edit exists. That is what makes the adversarial suite and this oracle
complementary: one probes the cases we thought of, the other checks the ones
we did not.
"""

import argparse
import hashlib
import os
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent


# Incremental compilation is off for the oracle's builds. rustc repartitions
# codegen units when a dependency changes, and a repartition moves functions
# between objects without changing what the program means — which shows up in
# a text-section comparison as "every dependent changed" and is not what the
# classifier is being accused of. A from-scratch build of each dependent is
# slow and is the only comparison that answers the question asked.
ENV = dict(os.environ, CARGO_INCREMENTAL="0")


def run(command, cwd=None):
    return subprocess.run(command, cwd=cwd, capture_output=True, text=True, env=ENV)


def text_sections(root: Path, skip_crate: str) -> dict:
    """Hash of every object's `__TEXT,__text`, per dependent rlib.

    `skip_crate` is the edited crate itself, whose code is *expected* to
    change; including it would make every run fail for the one reason that is
    not a bug.
    """
    out = {}
    for rlib in sorted(root.rglob("*.rlib")):
        if skip_crate in rlib.name:
            continue
        with tempfile.TemporaryDirectory() as scratch:
            scratch = Path(scratch)
            if run(["ar", "-x", str(rlib.resolve())], cwd=scratch).returncode != 0:
                continue
            digest = hashlib.blake2b(digest_size=16)
            for member in sorted(scratch.glob("*.o")):
                dumped = run(["otool", "-s", "__TEXT", "__text", str(member)])
                digest.update(member.name.encode())
                # Only the hex dump. `otool` prints the object's *path* as its
                # first line, and the objects are extracted to a fresh
                # temporary directory on every call, so hashing the raw output
                # made every rlib differ from itself on every run — including
                # crates that do not depend on the edited one. The first
                # version of this oracle reported 50/50 dependents changed and
                # the tell was that `libahash` was among them.
                for line in dumped.stdout.splitlines():
                    if line.startswith(("\t", " ")) or line[:1].isdigit():
                        digest.update(line.encode())
            out[rlib.name] = digest.hexdigest()
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", nargs="+", default=["blinker-lib", "rg-lib"])
    parser.add_argument("--direct-edits", nargs="+",
                        default=["body_arith", "body_existing_call"])
    # `inline_twin_body` first, and it is the one that carries the argument.
    # `signature` cannot build by construction — changing the injected
    # function's signature breaks the downstream caller that was injected to
    # call it — so an oracle whose only control is `signature` reports "cannot
    # discriminate" on every run and proves nothing. That is what it did, for
    # as long as the default was not updated when the inline twin was added.
    parser.add_argument(
        "--control-edits", nargs="+", default=["inline_twin_body", "signature"]
    )
    parser.add_argument("--out", default=str(HERE / "results" / "s0c-oracle.json"))
    options = parser.parse_args()

    rows, failures = [], []
    for name in options.fixtures:
        fixture = HERE / "fixtures" / name
        described = json.loads((fixture / "fixture.json").read_text())
        source = Path(described["file"])
        project = Path(described["project"])
        variants = fixture / "variants"
        build = ["cargo", f"+{described['toolchain']}", "build",
                 "-p", described["downstream_package"]]
        target = project / "target" / "debug"
        edited_crate = described["crate_name"]

        print(f"\n  {name}  (edited crate: {edited_crate})")
        pristine = (variants / "pristine.rs").read_text()
        source.write_text(pristine)
        if run(build, cwd=project).returncode != 0:
            failures.append(f"{name}: the pristine build failed")
            continue
        baseline = text_sections(target, edited_crate)
        # The oracle must be stable against itself before it can be evidence
        # about anything else: hash twice with no edit in between and require
        # the same answer.
        again = text_sections(target, edited_crate)
        unstable = [k for k in baseline if baseline[k] != again.get(k)]
        if unstable:
            failures.append(
                f"{name}: hashing the same build twice differed on {len(unstable)}"
                f" rlibs ({', '.join(unstable[:3])}) — the oracle measures noise"
            )
            print(f"  X baseline is not stable: {len(unstable)}/{len(baseline)} differ")
            continue
        print(f"    baseline: {len(baseline)} dependent rlibs hashed, stable on re-hash")

        # A control first. If a *signature* change also leaves every dependent
        # byte-identical, the oracle is not measuring anything and every
        # result below it is worthless.
        for edit in options.control_edits:
            if not (variants / f"{edit}.rs").exists():
                continue
            source.write_text((variants / f"{edit}.rs").read_text())
            if run(build, cwd=project).returncode != 0:
                print(f"    control {edit:20s} did not build — cannot discriminate")
                continue
            after = text_sections(target, edited_crate)
            changed = [k for k in baseline if baseline[k] != after.get(k)]
            verdict = "changed" if changed else "IDENTICAL"
            print(f"    control {edit:20s} dependents {verdict}"
                  f" ({len(changed)}/{len(baseline)})")
            rows.append({"fixture": name, "edit": edit, "kind": "control",
                         "changed": changed, "total": len(baseline)})
            if not changed:
                failures.append(
                    f"{name}/{edit}: a control edit left every dependent identical,"
                    " so this oracle cannot detect a downstream change at all"
                )

        for edit in options.direct_edits:
            if not (variants / f"{edit}.rs").exists():
                continue
            source.write_text(pristine)
            run(build, cwd=project)
            baseline = text_sections(target, edited_crate)
            source.write_text((variants / f"{edit}.rs").read_text())
            if run(build, cwd=project).returncode != 0:
                failures.append(f"{name}/{edit}: build failed")
                continue
            after = text_sections(target, edited_crate)
            changed = [k for k in baseline if baseline[k] != after.get(k)]
            rows.append({"fixture": name, "edit": edit, "kind": "direct",
                         "changed": changed, "total": len(baseline)})
            if changed:
                print(f"  X direct  {edit:20s} {len(changed)}/{len(baseline)}"
                      f" dependents CHANGED: {', '.join(changed[:3])}")
                failures.append(
                    f"{name}/{edit}: DIRECT, but {len(changed)} dependent(s) changed code:"
                    f" {', '.join(changed[:5])}"
                )
            else:
                print(f"    direct  {edit:20s} all {len(baseline)} dependents identical")

        source.write_text(pristine)
        run(build, cwd=project)

    Path(options.out).parent.mkdir(parents=True, exist_ok=True)
    Path(options.out).write_text(json.dumps(rows, indent=2) + "\n")
    if failures:
        print(f"\n  {len(failures)} FAILED:")
        for line in failures:
            print(f"    {line}")
        sys.exit(1)
    print(f"\n  oracle passed: every DIRECT edit left all dependents' code identical")


if __name__ == "__main__":
    main()
