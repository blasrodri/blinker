#!/usr/bin/env python3
"""Turn a real Rust project into an S0 fixture.

Two problems have to be solved to measure a real crate rather than a generated
one, and neither is optional.

**The invocation.** A crate with dependencies cannot be compiled by a rustc
command line anyone can guess: it needs `--extern` for every dependency, the
right `-L` paths, the edition, the feature cfgs, and the exact metadata hashes
cargo chose. So the invocation is *captured* from `cargo build -v` rather than
constructed, and replayed verbatim. Anything else measures a different
compilation from the one the developer actually waits for.

**The hot root.** The five edit classes of V2 §4 are claims about what the
compiler must redo, and expressing them precisely inside somebody else's
function means understanding that function. So a hot root is *injected* — the
same shape as the generated fixture's — into a real crate, with a real
dependency graph around it. That is not a simplification of the measurement:
`#[blinker_live::hot]` is an annotation on an ordinary function, and what S0
measures is the cost of validating the crate that contains it, which is the
crate's cost and not the function's.

The project is copied before it is touched. A harness that edits the
repository it is being run from will eventually leave an edit behind.

    ./capture.py --name large --project /Users/blas/projects/blinker \\
        --package blinker-cli --bin blinker --file crates/cli/src/lib.rs
"""

import argparse
import json
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# The same hot root as the generated fixture, so the five edit classes mean
# the same thing in both and the numbers are comparable.
HOT_ROOT = """

// ---- injected by capture.py for the S0 frontend spike ----
//
// Every item carries `#[allow(missing_docs)]`: a real crate may `deny` lints
// that an injected fixture knows nothing about, and `grep-matcher` denies
// exactly this one. A fixture that cannot be injected into a strict crate can
// only ever measure lax ones.

/// A type whose layout the hot root depends on, so edit class 5 has something
/// to change.
#[allow(missing_docs, dead_code)]
#[derive(Clone, Copy)]
pub struct SpikeReading {
    pub value: u64,
    pub scale: u32,
}

#[allow(missing_docs, dead_code)]
impl SpikeReading {
    pub fn total(&self) -> u64 {
        self.value.wrapping_mul(self.scale as u64)
    }
}

/// Generic, instantiated at `u64` below; edit class 3 asks for a second one.
#[allow(missing_docs, dead_code)]
pub fn spike_convert<T: Into<u64> + Copy>(x: T) -> u64 {
    x.into().wrapping_add(1)
}

/// An existing callee, for edit class 2.
#[allow(missing_docs, dead_code)]
#[inline(never)]
pub fn spike_helper(x: u64) -> u64 {
    x.wrapping_mul(31).wrapping_add(7)
}

/// The hot root. `#[inline(never)]` because a replaceable boundary the
/// optimizer may erase is not one (V2 §10.3).
#[allow(missing_docs, dead_code)]
#[inline(never)]
pub fn spike_hot_root(reading: SpikeReading) -> u64 {
    reading.total().wrapping_mul(7).wrapping_add(1)
}

/// Reachable from the crate's roots, so whole-crate collection sees it.
#[allow(missing_docs, dead_code)]
#[unsafe(no_mangle)]
pub extern "C" fn spike_entry(value: u64) -> u64 {
    let reading = SpikeReading { value, scale: 3 };
    spike_hot_root(reading).wrapping_add(spike_helper(value)).wrapping_add(spike_convert(value))
}
"""

BASE_BODY = "reading.total().wrapping_mul(7).wrapping_add(1)"

EDITS = {
    "body_arith": "reading.total().wrapping_mul(11).wrapping_add(2)",
    "body_existing_call": "spike_helper(reading.total()).wrapping_add(3)",
    "body_new_generic": "spike_convert(reading.value as u32).wrapping_mul(5)",
}


def run(command, cwd=None, capture=True):
    return subprocess.run(
        command, cwd=cwd, capture_output=capture, text=True, check=False
    )


def copy_project(project: Path, into: Path) -> Path:
    """A git clone, so `target/` and other build output do not come along.

    Falls back to a filtered copy for a project that is not a repository.
    """
    if into.exists():
        shutil.rmtree(into)
    if (project / ".git").exists():
        result = run(["git", "clone", "--quiet", "--local", str(project), str(into)])
        if result.returncode == 0:
            # Uncommitted work is part of what the developer compiles, so it
            # has to come across; a clone alone would measure HEAD.
            for line in run(["git", "status", "--porcelain"], cwd=project).stdout.splitlines():
                relative = line[3:].strip()
                source = project / relative
                if source.is_file():
                    target = into / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(source, target)
            return into
    shutil.copytree(
        project, into, ignore=shutil.ignore_patterns("target", ".git", "*.o", "*.rlib")
    )
    return into


def capture_invocation(root: Path, package: str, target: str, source: Path,
                       toolchain: str) -> list:
    """The exact rustc command cargo runs for this crate.

    Built twice: once to bring the dependencies up to date, then once with the
    crate's source touched so cargo actually re-runs *it*. Without the touch
    the second build reports the crate fresh and prints no command at all,
    which is what the first attempt at this did.
    """
    # The very same compiler that will measure it. Dependencies built by a
    # different rustc are rejected with E0514 the moment the driver opens
    # them, which is how the first attempt at this failed: cargo used the
    # project's stable default and the driver is the pinned nightly.
    base = ["cargo", f"+{toolchain}", "build", "--release", "-p", package]
    first = run(base, cwd=root)
    if first.returncode != 0:
        sys.exit(f"the fixture does not build:\n{first.stderr[-3000:]}")
    source.touch()
    verbose = run(base + ["-v"], cwd=root)
    lines = verbose.stderr.splitlines()
    wanted = re.compile(rf"--crate-name\s+{re.escape(target)}\b")
    for line in reversed(lines):
        stripped = line.strip()
        if stripped.startswith("Running `") and wanted.search(stripped):
            command = stripped[len("Running `"):].rstrip("`")
            return shlex.split(command)
    sys.exit(
        f"cargo did not re-run rustc for {target}; nothing to capture.\n"
        "The crate was already fresh — touch its source and try again."
    )


def clean_invocation(argv: list, root: Path) -> list:
    """Strip what the driver supplies itself or must not inherit.

    `--json`/`--error-format` would make rustc render diagnostics cargo's way
    and time the rendering. `-C incremental` and `--sysroot` are the driver's
    to set. Everything else — the externs, the cfgs, the edition — is exactly
    what makes this a real compilation and is kept.
    """
    out, skip = [], False
    for index, arg in enumerate(argv):
        if skip:
            skip = False
            continue
        if index == 0:
            out.append("rustc")
            continue
        if arg.startswith(("--json", "--error-format", "--sysroot", "-Cincremental", "--emit")):
            continue
        if arg in ("--json", "--error-format", "--sysroot", "--emit"):
            skip = True
            continue
        if arg == "-C" and index + 1 < len(argv) and argv[index + 1].startswith("incremental"):
            skip = True
            continue
        # Relative paths in the captured command are relative to the project
        # root; the driver runs from its own directory.
        if not arg.startswith("-") and (root / arg).exists():
            out.append(str((root / arg).resolve()))
            continue
        out.append(arg)
    out.append("--emit=metadata")
    return out


def variants(pristine: str) -> dict:
    out = {"pristine": pristine}
    for name, body in EDITS.items():
        out[name] = pristine.replace(BASE_BODY, body)
    out["signature"] = (
        pristine.replace(
            "pub fn spike_hot_root(reading: SpikeReading) -> u64 {",
            "pub fn spike_hot_root(reading: SpikeReading, bias: u64) -> u64 {",
        )
        .replace(BASE_BODY, f"{BASE_BODY}.wrapping_add(bias)")
        .replace("spike_hot_root(reading).wrapping_add", "spike_hot_root(reading, 9).wrapping_add")
    )
    out["type_layout"] = pristine.replace(
        "pub value: u64,\n    pub scale: u32,",
        "pub value: u64,\n    pub tag: u64,\n    pub scale: u32,",
    ).replace(
        "SpikeReading { value, scale: 3 }", "SpikeReading { value, tag: 0, scale: 3 }"
    )
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--name", required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--package", required=True)
    parser.add_argument("--target", required=True, help="rustc --crate-name to capture")
    parser.add_argument("--file", required=True, help="source file, relative to the project")
    parser.add_argument("--work", default="/tmp/spike-fixtures")
    parser.add_argument("--toolchain", default="nightly-2026-07-27")
    parser.add_argument("--crate-type", default="bin", choices=("bin", "lib"))
    # For S0b: the binary at the top of the graph, so the cargo baseline can
    # be timed. An edit to a library is only finished when everything that
    # depends on it has been rebuilt, and that is the number a developer waits
    # for today.
    parser.add_argument("--downstream-package", default=None)
    options = parser.parse_args()

    work = Path(options.work) / options.name
    root = copy_project(Path(options.project), work)
    source = root / options.file
    if not source.exists():
        sys.exit(f"{source} does not exist in the copy")

    text = source.read_text()
    if "spike_hot_root" not in text:
        source.write_text(text + HOT_ROOT)

    argv = clean_invocation(
        capture_invocation(root, options.package, options.target, source, options.toolchain),
        root,
    )

    pristine = source.read_text()
    made = variants(pristine)
    if made["body_arith"] == pristine:
        sys.exit("the hot root's body was not found; the edits would measure nothing")

    fixture_dir = HERE / "fixtures" / options.name
    fixture_dir.mkdir(parents=True, exist_ok=True)
    store = fixture_dir / "variants"
    if store.exists():
        shutil.rmtree(store)
    store.mkdir()
    for name, variant in made.items():
        (store / f"{name}.rs").write_text(variant)

    manifest = {
        "name": options.name,
        "crate": str(fixture_dir),
        "file": str(source),
        "hot": "spike_hot_root",
        "crate_name": options.target,
        "crate_type": options.crate_type,
        "args": argv,
        "project": str(root),
        "variants": sorted(made),
        "toolchain": options.toolchain,
        "package": options.package,
        "downstream_package": options.downstream_package,
        "source_relative": options.file,
    }
    (fixture_dir / "fixture.json").write_text(json.dumps(manifest, indent=2) + "\n")
    externs = sum(1 for a in argv if a == "--extern")
    print(f"  {options.name}: {options.target}, {externs} externs, {len(argv)} args")
    print(f"    source {source}")
    print(f"    fixture {fixture_dir}")


if __name__ == "__main__":
    main()
