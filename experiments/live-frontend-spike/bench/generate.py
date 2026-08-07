#!/usr/bin/env python3
"""Build the Agent Bench task set, and refuse to emit a task that proves nothing.

Every task is generated three times over:

  broken.rs   what the agent is given
  fixed.rs    ground truth, which is the pristine text
  oracle      the `test_*` functions, shared by both environments

and then *verified*. The suite must fail on `broken` and pass on `fixed`. A
seeded defect the tests do not catch is not a task — it is a hole in the suite —
and it is dropped, counted, and named in the summary rather than quietly
skipped. The same goes for a task whose "fix" does not actually pass.

This is the same discipline as §31's negative controls, applied to the benchmark
itself: a task set nobody has shown *can* fail is a task set that measures
nothing. Roughly a third of the first draft's defects were dropped here, mostly
for being invisible to the inputs the tests happened to use.

    python3 bench/generate.py [--out bench/tasks]
"""

import argparse
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import domains  # noqa: E402

HERE = pathlib.Path(__file__).parent
SPIKE = HERE.parent

# Outside the repository on purpose.
#
# These are 49 generated cargo crates, and the first full run put them in the
# working tree. An editor's Rust indexer found them, decided it had 49 new
# crates to analyse, and took 327% CPU for the rest of the afternoon — the
# benchmark was competing for cores with something indexing the benchmark. Run
# rates fell from 24 attempts a minute to two.
#
# They are also pure build output: `domains.py` and this file are the source of
# truth and reproduce them exactly, so there is nothing here to keep.
DEFAULT_OUT = pathlib.Path(
    os.environ.get("TMPDIR", "/tmp")) / "agentbench-tasks"


def runner_for(domain):
    """A `main` that runs the suite and prints which tests failed."""
    names = domains.test_names(domain)
    checks = "\n".join(
        """    let mut want = 0u64;
    let got = {name}(&mut want);
    if got != want {{
        failures.push(("{name}", want, got));
    }}""".format(name=name)
        for name in names
    )
    return """
fn main() {{
    let mut failures: Vec<(&str, u64, u64)> = Vec::new();
{checks}
    let rendered: Vec<String> = failures
        .iter()
        .map(|(n, w, g)| format!("{{{{\\"test\\":\\"{{n}}\\",\\"expected\\":{{w}},\\"actual\\":{{g}}}}}}"))
        .collect();
    println!("[{{}}]", rendered.join(","));
}}
""".format(checks=checks)


def run_suite(source, domain, workdir):
    """Compile `source` with its runner and report the failing tests.

    Direct `rustc`, not cargo: this runs twice per candidate task and cargo's
    per-invocation overhead would dominate the generation time without changing
    a single answer. The *bench* uses a real cargo project, because there the
    overhead is the thing being measured.
    """
    path = workdir / "probe.rs"
    path.write_text(source + runner_for(domain))
    binary = workdir / "probe"
    build = subprocess.run(
        ["rustc", "--edition=2021", "-Copt-level=0", "-Cdebuginfo=0",
         "--cap-lints=allow", str(path), "-o", str(binary)],
        capture_output=True, text=True,
    )
    if build.returncode != 0:
        return None, build.stderr.strip().splitlines()[:3]
    ran = subprocess.run([str(binary)], capture_output=True, text=True, timeout=30)
    if ran.returncode != 0:
        return None, ["the suite did not run: %s" % ran.stderr.strip()[:200]]
    return json.loads(ran.stdout), None


def measure_expectations(domain, workdir):
    """What the pristine crate answers for each test.

    The expectations are read off the reference implementation rather than
    written down. That removes a whole class of generator bug — the first draft
    asserted them by hand and nine tasks were dropped because the *fixed* source
    failed its own suite — and it is the right definition anyway: a mutation
    benchmark asks for the intended behaviour restored, and pristine is what
    intended means.
    """
    pristine, _ = domains.DOMAINS[domain]
    names = domains.test_names(domain)
    # A runner that prints what each test *returned*, with the expectations
    # still zero, so this is a measurement rather than a comparison.
    probe = pristine + domains.render_tests(domain, {}) + """
fn main() {{
    let mut out: Vec<String> = Vec::new();
{calls}
    println!("{{{{{{}}}}}}", out.join(","));
}}
""".format(calls="\n".join(
        '    {{ let mut w = 0u64; let g = {name}(&mut w); out.push(format!("\\"{name}\\":{{}}", g)); }}'.format(name=name)
        for name in names
    ))
    path = workdir / "measure.rs"
    path.write_text(probe)
    binary = workdir / "measure"
    build = subprocess.run(
        ["rustc", "--edition=2021", "-Copt-level=0", "-Cdebuginfo=0",
         "--cap-lints=allow", str(path), "-o", str(binary)],
        capture_output=True, text=True,
    )
    if build.returncode != 0:
        raise SystemExit("the %s domain does not compile:\n%s" % (domain, build.stderr))
    ran = subprocess.run([str(binary)], capture_output=True, text=True, timeout=30)
    if ran.returncode != 0:
        raise SystemExit("the %s domain does not run: %s" % (domain, ran.stderr))
    return json.loads(ran.stdout)


def apply(source, edits, where):
    """Apply substitutions, insisting each one matched exactly once."""
    for anchor, replacement in edits:
        if source.count(anchor) != 1:
            raise ValueError(
                "%s: anchor matched %d times, not once" % (where, source.count(anchor))
            )
        source = source.replace(anchor, replacement, 1)
    return source


def make_inline(source, function):
    """Mark `function` `#[inline]`, so its fix cannot go DIRECT."""
    anchor = 'pub extern "C" fn %s(' % function
    at = source.index(anchor)
    attribute = source.rindex("#[inline(never)]", 0, at)
    return source[:attribute] + "#[inline]" + source[attribute + len("#[inline(never)]"):]


def stub(source, function):
    """Replace `function`'s body with `0`, so it has to be written."""
    at = source.index('pub extern "C" fn %s(' % function)
    open_brace = source.index("{", source.index(")", at))
    depth = 0
    for i in range(open_brace, len(source)):
        if source[i] == "{":
            depth += 1
        elif source[i] == "}":
            depth -= 1
            if depth == 0:
                return source[:open_brace] + "{\n    0\n}" + source[i + 1:]
    raise ValueError("no body for %s" % function)


# Two candidates that must never survive verification.
#
# The first draft dropped sixteen of forty-nine; measured expectations and two
# extra test inputs took that to zero. A rejection rate that falls to nothing is
# exactly when a filter stops being watched, so these keep it demonstrable: a
# defect that changes nothing observable, and a "fix" that does not pass. If
# either is emitted as a task, generation fails.
SELF_TEST = [
    dict(id="self-test-invisible", domain="scanner", family="self-test",
         targets=["scan"], verdict="DIRECT", inline=None, stubs=[],
         description="a comment change, which no test can see",
         edits=[("/// Sum every decimal number in `data`.",
                 "/// Sum every decimal number in `data`. (reworded)")]),
    dict(id="self-test-wrong-oracle", domain="scanner", family="self-test",
         targets=["scan"], verdict="DIRECT", inline=None, stubs=[],
         description="a real defect, checked against an expectation that is wrong",
         edits=[("acc.wrapping_mul(10)", "acc.wrapping_mul(8)")]),
]


def candidates():
    """Every task this generator would like to emit, before verification."""
    out = []
    by_name = {d[0]: d for d in domains.DEFECTS}

    for name, family, domain, target, edits in domains.DEFECTS:
        out.append(dict(
            id="%s-%s" % (domain, name), domain=domain, family=family,
            targets=[target], verdict="DIRECT",
            edits=list(edits), inline=None, stubs=[],
            description="one seeded defect in `%s`" % target,
        ))

    for domain, first, second in domains.PAIRS:
        a, b = by_name[first], by_name[second]
        out.append(dict(
            id="%s-%s+%s" % (domain, first, second), domain=domain, family="multi-function",
            targets=sorted({a[3], b[3]}), verdict="DIRECT",
            edits=list(a[4]) + list(b[4]), inline=None, stubs=[],
            description="two seeded defects, in `%s` and `%s`" % (a[3], b[3]),
        ))

    for domain, name in domains.FALLBACK_TARGETS:
        d = by_name[name]
        out.append(dict(
            id="%s-%s-inline" % (domain, name), domain=domain, family="fallback",
            targets=[d[3]], verdict="FALLBACK",
            edits=list(d[4]), inline=d[3], stubs=[],
            description="a seeded defect in `%s`, which is `#[inline]` — the fix "
                        "cannot be published directly" % d[3],
        ))

    for domain, function, note in domains.FEATURES:
        out.append(dict(
            id="%s-%s-stub" % (domain, function), domain=domain, family="feature",
            targets=[function], verdict="DIRECT",
            edits=[], inline=None, stubs=[function],
            description=note,
        ))
    return out


def build(task, workdir, expectations):
    """Render and verify one task. Returns `(record, source, fixed)` or a reason."""
    pristine, _ = domains.DOMAINS[task["domain"]]
    tests = domains.render_tests(task["domain"], expectations[task["domain"]])

    fixed = pristine
    if task["inline"]:
        fixed = make_inline(fixed, task["inline"])
    try:
        broken = apply(fixed, task["edits"], task["id"])
    except ValueError as error:
        return None, str(error)
    for function in task["stubs"]:
        broken = stub(broken, function)

    fixed_source = fixed + tests
    broken_source = broken + tests

    before, error = run_suite(broken_source, task["domain"], workdir)
    if error:
        return None, "broken does not compile: %s" % "; ".join(error)
    after, error = run_suite(fixed_source, task["domain"], workdir)
    if error:
        return None, "fixed does not compile: %s" % "; ".join(error)

    # The discriminating check, and the only reason this file is worth its
    # length. A defect the suite cannot see is not a task.
    if after:
        return None, "the fixed source fails %d test(s): %s" % (
            len(after), ", ".join(t["test"] for t in after))
    if not before:
        return None, "the suite passes on the broken source — the defect is invisible"

    record = dict(task)
    record.pop("edits")
    record.pop("stubs")
    record["failing"] = before
    record["failing_count"] = len(before)
    return (record, broken_source, fixed_source), None


CARGO_TOML = """[package]
name = "task"
version = "0.0.0"
edition = "2021"

[lib]
name = "task"
path = "src/lib.rs"

[profile.dev]
debug = 0
"""

ORACLE = '''//! The cargo-side view of the same suite.
//!
//! It calls the very same `test_*` functions the Blinker API's `run_affected`
//! calls, so "solved" cannot mean two different things in the two environments.
{tests}
'''


def write_task(root, record, broken, fixed, tests):
    directory = root / record["id"]
    if directory.exists():
        shutil.rmtree(directory)
    (directory / "src").mkdir(parents=True)
    (directory / "tests").mkdir()
    (directory / "variants").mkdir()

    (directory / "broken.rs").write_text(broken)
    (directory / "fixed.rs").write_text(fixed)
    # The spike's `open` copies `variants/pristine.rs` over the target file, so
    # for a task "pristine" means the state the agent is handed, not the state
    # anybody would call correct.
    (directory / "variants" / "pristine.rs").write_text(broken)
    (directory / "src" / "lib.rs").write_text(broken)
    (directory / "Cargo.toml").write_text(CARGO_TOML)

    names = domains.test_names(record["domain"])
    checks = "\n".join(
        """
#[test]
fn {name}() {{
    let mut want = 0u64;
    let got = unsafe {{ task::{name}(&mut want) }};
    assert_eq!(got, want, "{name}");
}}""".format(name=name)
        for name in names
    )
    (directory / "tests" / "oracle.rs").write_text(ORACLE.format(tests=checks))
    _ = tests

    (directory / "fixture.json").write_text(json.dumps({
        "name": record["id"],
        "crate": str(directory.resolve()),
        "file": "src/lib.rs",
        "crate_name": "task",
        "crate_type": "lib",
        "hot": record["targets"][0],
        "args": [],
        "extra_args": ["-Cmetadata=agent", "-Cdebug-assertions=off"],
    }, indent=2) + "\n")
    (directory / "task.json").write_text(json.dumps(record, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=pathlib.Path, default=DEFAULT_OUT)
    options = parser.parse_args()
    options.out.mkdir(parents=True, exist_ok=True)

    kept, dropped = [], []
    with tempfile.TemporaryDirectory() as tmp:
        workdir = pathlib.Path(tmp)
        expectations = {d: measure_expectations(d, workdir) for d in domains.DOMAINS}
        # The filter, before anything trusts it.
        for probe in SELF_TEST:
            # The second probe is checked against *bent* expectations, so the
            # pristine source fails its own suite. That is the failure mode the
            # first draft actually had — nine tasks whose "fix" did not pass —
            # and it is the one worth keeping a tripwire for.
            bent = expectations
            if probe["id"].endswith("wrong-oracle"):
                bent = {d: dict(v) for d, v in expectations.items()}
                first = domains.test_names("scanner")[0]
                bent["scanner"][first] += 1
            built, why = build(probe, workdir, bent)
            if built is not None:
                print("  the verifier accepted %s, which it must not" % probe["id"])
                return 1
            print("  self-test  %-24s dropped: %s" % (probe["id"], why))
        for task in candidates():
            built, why = build(task, workdir, expectations)
            if built is None:
                dropped.append((task["id"], why))
                continue
            record, broken, fixed = built
            write_task(options.out, record, broken, fixed,
                       domains.render_tests(record["domain"], expectations[record["domain"]]))
            kept.append(record)

    (options.out / "index.json").write_text(json.dumps(kept, indent=2) + "\n")

    families = {}
    for record in kept:
        families[record["family"]] = families.get(record["family"], 0) + 1
    print("\n  %d tasks, verified fail-before and pass-after" % len(kept))
    for family in sorted(families):
        print("    %-16s %3d" % (family, families[family]))
    print("    %-16s %3.1f" % ("mean failing", sum(r["failing_count"] for r in kept) / max(len(kept), 1)))
    if dropped:
        # Named, not counted. A generator that silently discards half its
        # candidates is one whose task set nobody can reason about.
        print("\n  %d dropped:" % len(dropped))
        for name, why in dropped:
            print("    %-34s %s" % (name, why))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
