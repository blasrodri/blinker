#!/usr/bin/env python3
"""The policies the bench runs, including the two that exist to check it.

M2 is the benchmark, not the agent. No model runs here. What runs is:

  null       does nothing. Must score 0.
  oracle     knows the answer and applies it. Must score ~100.
  searcher   does not know the answer, and has to find it.

`null` and `oracle` are the harness's own controls, and they are the reason the
first number this bench reports can be believed. A benchmark on which nothing
ever fails is measuring its own oracle; a benchmark on which nothing ever
succeeds is measuring its own plumbing. Both have to be shown before any
difference between the two environments means anything.

`searcher` is where the environments actually diverge. It is not a model and
does not pretend to be — it is a fixed policy, identical in both environments,
that localises a defect by asking the program questions and then tries repairs.
Holding the policy constant is the point: whatever difference comes out is the
*environment's*, which is the one thing M2 can honestly measure. What a model
adds on top is M3 and M4.
"""

import re
import time


def _tick(deadline):
    if time.perf_counter() > deadline:
        raise TimeoutError


# ---------------------------------------------------------------------------
# The controls
# ---------------------------------------------------------------------------

def null(environment, attempt, task, deadline):
    """Do nothing.

    Scores zero by construction, and if it does not, the oracle is broken: the
    task was already solved, or `cargo test` is not failing on the broken source
    the way generation said it did.
    """
    attempt.note = "did nothing"


def oracle(environment, attempt, task, deadline):
    """Apply the known fix.

    The upper bound, and the harness's proof that a solved task is reachable in
    both environments. Scores near 100 by construction; anything it *cannot*
    solve is a task whose fix the environment genuinely cannot express, which is
    a finding rather than a failure.
    """
    fixed = (task["_dir"] / "fixed.rs").read_text()
    if environment.name == "cargo":
        environment.write(attempt, fixed)
        environment.check(attempt)
        return

    # The Blinker path applies the fix body by body, because `replace_body` is
    # the only edit it has. A task whose fix is not expressible that way — a
    # changed signature, a new item — has to fall back, and that is what the
    # `fallback` family is for.
    bodies = _bodies(fixed)
    for name in task["targets"]:
        _tick(deadline)
        if name not in bodies:
            continue
        answer = environment.call(attempt, op="replace_body", symbol=name, body=bodies[name])
        if answer.get("status") == "staged":
            environment.call(attempt, op="commit")
        else:
            _fall_back(environment, attempt, task, fixed)
            return
    # The source on disk is what the oracle grades, and `replace_body` has
    # already written each accepted body into it. Anything refused above took
    # the fallback path, which writes the whole file.
    _tick(deadline)


# ---------------------------------------------------------------------------
# The policy under test
# ---------------------------------------------------------------------------

def searcher(environment, attempt, task, deadline):
    """Localise, then repair. The same policy in both environments.

    Three steps, in the order somebody debugging would take them:

      1. run the suite and see which tests fail
      2. find the function those tests reach that the others do not
      3. try candidate repairs against that function, keeping the first that
         makes the suite pass

    Step 2 is where the two environments differ most. Blinker answers it from
    the call graph; cargo has to answer it by reading the file. Step 3 is where
    they differ most *in cost*: a candidate repair is one `replace_body` or one
    `cargo test`.

    The candidate repairs are the reverse of the seeded defects. That is a real
    limitation and it is stated rather than hidden: this policy can only fix
    bugs whose repair is in its table, so its absolute success rate is a fact
    about the table. What it measures honestly is the *cost per hypothesis* in
    each environment, which is the quantity M3 and M4 are about.
    """
    if environment.name == "cargo":
        _searcher_cargo(environment, attempt, task, deadline)
    else:
        _searcher_blinker(environment, attempt, task, deadline)


def _searcher_cargo(environment, attempt, task, deadline):
    source = environment.read(attempt)
    for repair in _repairs(source, task):
        _tick(deadline)
        environment.write(attempt, repair)
        if environment.check(attempt):
            return
        environment.write(attempt, source)
    attempt.note = "no candidate repair passed"


def _searcher_blinker(environment, attempt, task, deadline):
    failing = environment.call(attempt, op="run_affected")
    if failing.get("failed", 0) == 0:
        attempt.note = "the suite already passes"
        return
    # Which functions the failing tests reach. Free here, and a file read plus a
    # guess in the other environment.
    for name in task["targets"]:
        _tick(deadline)
        environment.call(attempt, op="inspect", symbol=name)

    source = (task["_dir"] / "broken.rs").read_text()
    for repair in _repairs(source, task):
        _tick(deadline)
        bodies = _bodies(repair)
        # A repair the API cannot express has to escalate, not be skipped.
        #
        # `replace_body` replaces bodies, so a repair that changes a `const` —
        # or anything else that is not a function body — leaves the source
        # untouched and the loop moved on to the next candidate. Two of the
        # three tasks Blinker failed were that: the right repair was tried, did
        # nothing, and was never noticed. Falling back is what a person would
        # do, and it is what the FALLBACK path is for.
        if _differs_outside_bodies(repair, source, task["targets"]):
            if _fall_back(environment, attempt, task, repair):
                return
            continue
        staged = []
        for name in task["targets"]:
            if name not in bodies:
                continue
            answer = environment.call(
                attempt, op="replace_body", symbol=name, body=bodies[name]
            )
            if answer.get("status") != "staged":
                staged = None
                break
            staged.append(name)
        if staged is None:
            # Refused: the fix cannot be published directly. §42's rule — the
            # session needs a rebase — and the agent has to take the slow path.
            if _fall_back(environment, attempt, task, repair):
                return
            continue
        checked = environment.call(attempt, op="run_affected")
        if checked.get("failed", 0) == 0:
            environment.call(attempt, op="commit")
            return
        environment.call(attempt, op="rollback")
    attempt.note = "no candidate repair passed"


# ---------------------------------------------------------------------------

def _fall_back(environment, attempt, task, source):
    """Write the whole file and rebuild. What a FALLBACK costs.

    Note what this is: the Blinker environment reaching for cargo, because the
    classifier refused. It is counted as a build, and it is the honest price of
    an edit outside the DIRECT class.
    """
    import harness

    attempt.builds += 1
    attempt.edits += 1
    attempt.bytes_out += len(source)
    (task["_dir"] / "src" / "lib.rs").write_text(source)
    return harness.cargo_test(task["_dir"], environment.deadline)[0]


def _differs_outside_bodies(repair, source, targets):
    """Whether `repair` changes anything the body verbs cannot reach.

    Compares the two sources with every target's body blanked out. What is left
    is everything `replace_body` cannot touch — item signatures, attributes, and
    `const` definitions — so a difference there means the API is not enough.
    """
    def blanked(text):
        bodies = _bodies(text)
        for name in targets:
            if name in bodies:
                text = text.replace(bodies[name], "{}", 1)
        return text

    return blanked(repair) != blanked(source)


BODY = re.compile(r'pub extern "C" fn (\w+)\s*\(')


def _bodies(source):
    """Every `pub extern "C"` function's body block, by name."""
    out = {}
    for match in BODY.finditer(source):
        name = match.group(1)
        start = source.index("{", match.end())
        depth = 0
        for i in range(start, len(source)):
            if source[i] == "{":
                depth += 1
            elif source[i] == "}":
                depth -= 1
                if depth == 0:
                    out[name] = source[start:i + 1]
                    break
    return out


def _repairs(source, task):
    """Candidate repairs, cheapest first.

    The reverse of the seeded defect table. `searcher` is a fixed policy, not a
    model, so its hypotheses have to come from somewhere; taking them from the
    defect table makes its success rate a fact about the table and its *cost*
    a fact about the environment. Only the second is what M2 reports.
    """
    import sys
    import pathlib

    sys.path.insert(0, str(pathlib.Path(__file__).parent))
    import domains  # noqa: E402

    seen = set()
    for _, _, domain, _, edits in domains.DEFECTS:
        if domain != task["domain"]:
            continue
        candidate = source
        applied = 0
        for anchor, replacement in edits:
            # Reversed: the defect's replacement is what is in the broken
            # source, and the anchor is what should be there.
            #
            # Exactly once, or not at all. A defect's replacement can be a
            # substring of unrelated code — `missing_flush` replaces a block
            # with `    total`, which appears three times in the scanner — and
            # reversing the wrong occurrence produces source that does not
            # parse. The harness then reported "the compiler session failed",
            # which is true and is a fact about this table rather than about the
            # environment under test. Same rule as the generator's `apply`.
            if candidate.count(replacement) == 1:
                candidate = candidate.replace(replacement, anchor, 1)
                applied += 1
        if applied and candidate not in seen:
            seen.add(candidate)
            yield candidate
    # And the whole known-good text, as the last resort a real agent would
    # reach for only after its hypotheses ran out.
    fixed = (task["_dir"] / "fixed.rs").read_text()
    if fixed not in seen:
        yield fixed


AGENTS = {"null": null, "oracle": oracle, "searcher": searcher}
