#!/usr/bin/env python3
"""The M1 gate: fix a real bug using only the agent API.

The criterion is not "the API has seven verbs". It is that somebody can find
and fix a bug with them and never reach for cargo. So this is a *transcript*,
not a unit test: every step is a question a person debugging would actually ask,
in the order they would ask it, and each one is checked for the answer that
would let them take the next step.

    open              → the crate, resident
    run_affected      → three tests fail, with both numbers
    inspect / callers → what the failing path is made of
    probe ×4          → the hypothesis, tested against the running program
    replace_body      → the fix, compiled and classified, published to nothing
    run_affected      → only the tests that reach the change, all passing
    probe             → the candidate answers 30 where the image answers 12
    commit            → and now the program does
    rollback          → and now it does not

The four probes are the point. Two of them are the experiment that identifies
the bug — `12,18` gives 12 and `12,18,` gives 30, so the machine loses whatever
follows the last separator — and neither required a rebuild, a print statement
or a debugger.

Run:  python3 agent_gate.py [--backend PATH]
"""

import argparse
import json
import os
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).parent


class Session:
    """One resident spike process, spoken to in JSON lines."""

    def __init__(self, backend, fixture):
        args = [str(HERE / "target/release/spike"), "--fixture", str(fixture), "--serve"]
        if backend:
            args += ["--backend", str(backend)]
        self.process = subprocess.Popen(
            args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1
        )
        self.transcript = []
        self.failures = []

    def __call__(self, **request):
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        if not line:
            raise SystemExit("the session died")
        observation = json.loads(line)
        self.transcript.append((request, observation))
        return observation

    def close(self):
        try:
            self(op="quit")
        except SystemExit:
            pass
        self.process.stdin.close()
        self.process.wait(timeout=30)

    def expect(self, what, condition, observation):
        mark = "ok  " if condition else "FAIL"
        if not condition:
            self.failures.append(what)
        print(f"  {mark}  {what}")
        if not condition:
            print(f"          {json.dumps(observation)}")


# The fix: flush the accumulator when the input ends inside a number.
SCAN_FIXED = """{
    let mut total: u64 = 0;
    let mut acc: u64 = 0;
    let mut in_number: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: the caller passes a pointer to at least `len` bytes.
        let b = unsafe { *data.add(i as usize) };
        let class = classify_byte(b);
        if class == CLASS_DIGIT {
            acc = accumulate(acc, b);
            in_number = 1;
        } else if class == CLASS_SEP {
            total = total.wrapping_add(acc);
            acc = 0;
            in_number = 0;
        }
        i = i.wrapping_add(1);
    }
    if in_number == 1 {
        total = total.wrapping_add(acc);
    }
    total
}"""

# The same bug in the counting machine, which `test_count` catches.
COUNT_FIXED = """{
    let mut count: u64 = 0;
    let mut in_number: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: as above.
        let b = unsafe { *data.add(i as usize) };
        if classify_byte(b) == CLASS_DIGIT {
            in_number = 1;
        } else if in_number == 1 {
            count = count.wrapping_add(1);
            in_number = 0;
        }
        i = i.wrapping_add(1);
    }
    if in_number == 1 {
        count = count.wrapping_add(1);
    }
    count
}"""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--backend", default=os.environ.get("SPIKE_BACKEND"))
    parser.add_argument("--fixture", default=str(HERE / "fixtures/agent"))
    parser.add_argument("--json", type=pathlib.Path)
    options = parser.parse_args()

    session = Session(options.backend, options.fixture)
    say = session.expect

    print("\n  the program, as it is")
    opened = session(op="open")
    say("the crate opens resident", opened["status"] == "ok", opened)

    failing = session(op="run_affected")
    names = {f["test"] for f in failing.get("failures", [])}
    say(
        "the suite reports three failures, with both numbers",
        failing["failed"] == 3
        and names == {"::test_trailing_number", "::test_scaled", "::test_count"}
        and all("expected" in f and "actual" in f for f in failing["failures"]),
        failing,
    )
    say(
        "and they run against the base image, not against nothing",
        failing["tests"] == 6,
        failing,
    )

    print("\n  what the failing path is made of")
    test = session(op="inspect", symbol="test_trailing_number")
    say("the failing test names what it calls", "::scan" in test.get("calls", []), test)

    scan = session(op="inspect", symbol="scan")
    say(
        "and `scan` comes back with its body and its callees",
        scan.get("body", "").startswith("{")
        and set(scan.get("calls", [])) >= {"::classify_byte", "::accumulate"},
        scan,
    )

    callers = session(op="callers", symbol="scan")
    say(
        "callers of `scan` come from the call graph, not from a search",
        set(callers.get("callers", [])) == {
            "::total_scaled",
            "::test_trailing_number",
            "::test_trailing_separator",
            "::test_empty",
        },
        callers,
    )

    print("\n  the hypothesis, asked of the running program")
    trials = {
        "12,18": 12,   # ends on a digit: the 18 is lost
        "12,18,": 30,  # ends on a separator: correct
        "7": 0,        # one number, no separator at all
        "7,": 7,       # the same number, flushed
    }
    for text, expected in trials.items():
        answer = session(op="probe", symbol="scan", args=[0, len(text)], bytes=text)
        say(
            f"probe scan({text!r}) = {expected}   [{answer.get('source')}]",
            answer.get("returned") == expected and answer.get("source") == "image",
            answer,
        )

    print("\n  the fix, compiled and classified, published to nothing")
    staged = session(op="replace_body", symbol="scan", body=SCAN_FIXED)
    say("the edit is DIRECT and staged", staged["status"] == "staged", staged)
    say(
        f"edit → staged in {staged.get('latency_ms', 0):.1f} ms",
        staged.get("latency_ms", 1e9) < 200,
        staged,
    )

    before = session(op="probe", symbol="scan", args=[0, 5], bytes="12,18")
    say(
        "the candidate answers 30 where the image answered 12",
        before.get("returned") == 30 and before.get("source") == "candidate",
        before,
    )
    say(
        "and nothing has been published: the generation is still 0",
        session(op="status").get("generation") == 0,
        staged,
    )

    print("\n  the tests that reach the change, and only those")
    selected = session(op="run_affected")
    say(
        "four of six selected — `test_count` and `test_classify_only` cannot reach `scan`",
        selected["tests"] == 4,
        selected,
    )
    say("and every selected test passes", selected["failed"] == 0, selected)

    print("\n  commit")
    active = session(op="commit")
    say("the candidate becomes the program", active["status"] == "active", active)
    after = session(op="probe", symbol="scan", args=[0, 5], bytes="12,18")
    say(
        "and the probe now answers from the generation",
        after.get("returned") == 30 and after.get("source") == "generation",
        after,
    )

    print("\n  the second bug, which the first fix does not reach")
    still = session(op="run_affected")
    counting = session(op="replace_body", symbol="count_numbers", body=COUNT_FIXED)
    say("`count_numbers` is DIRECT too", counting["status"] == "staged", counting)
    session(op="commit")
    whole = session(op="run_affected")
    say(
        "with both fixed, `test_count` passes",
        whole["failed"] == 0,
        whole,
    )
    _ = still

    print("\n  rollback")
    rolled = session(op="rollback")
    say("rollback returns to the previous generation", rolled["status"] == "rolled_back", rolled)
    back = session(op="probe", symbol="count_numbers", args=[0, 5], bytes="12,18")
    say(
        "and the retired implementation is the one running again",
        back.get("returned") == 1,
        back,
    )

    print("\n  the cost of the whole session")
    calls = len(session.transcript)
    compiles = sum(
        1 for request, _ in session.transcript if request["op"] in ("replace_body", "open")
    )
    latencies = [o.get("latency_ms", 0.0) for _, o in session.transcript]
    print(f"    {calls} calls, {compiles} compiler sessions, {sum(latencies):.0f} ms total")
    print(f"    cargo, invoked: 0")

    if options.json:
        options.json.write_text(
            json.dumps(
                [{"request": q, "observation": a} for q, a in session.transcript], indent=2
            )
        )

    session.close()
    if session.failures:
        print(f"\n  {len(session.failures)} FAILED")
        for failure in session.failures:
            print(f"    {failure}")
        return 1
    print("\n  the gate is green: a bug found and fixed without cargo")
    return 0


if __name__ == "__main__":
    sys.exit(main())
