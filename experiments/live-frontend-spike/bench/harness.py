#!/usr/bin/env python3
"""The two environments a task can be solved in, and what it costs in each.

The whole point of M2 is a comparison, so the two environments have to differ in
exactly one thing: how the agent interacts with the program. Same tasks, same
oracle, same definition of solved, same wall clock.

  cargo    edit the file, run `cargo test`. What an agent does today.
  blinker  the seven verbs of §44, over a resident session.

Warm, not cold
--------------

Both environments are warmed before the clock starts — cargo builds the task
once, Blinker opens the session once — and the warm-up is reported separately.
A cold `cargo build` on a fresh crate is dominated by compiling the test harness
and linking, which is real but is not what an agent pays per hypothesis. Timing
it would flatter Blinker by a factor that has nothing to do with Blinker.

What is recorded
----------------

Per attempt: wall time, whether the oracle passes, how many edits, how many
builds, how many probes, and how many bytes crossed the interface in each
direction. The last is a stand-in for tokens: this harness runs no model, and a
byte count is the honest version of the thing tokens would measure.
"""

import json
import pathlib
import shutil
import subprocess
import time

SPIKE = pathlib.Path(__file__).parent.parent

# How long one `cargo test` may take before it is called a failure.
#
# It was ten minutes, which is the wrong kind of generous. An agent — or a
# candidate repair — can write a program that does not terminate: reverse the
# wrong defect in the run-length domain and `decoded_sum` loops until
# `decoded_len` says stop, which it never does. The first full run wedged for
# thirty-nine minutes on exactly that, and the wall-clock budget could not help
# because it is only checked between candidates.
#
# A timeout is a failed attempt, which is the truthful outcome: the agent
# produced a program whose tests do not pass.
TEST_TIMEOUT_S = 20


def cargo_test(directory, deadline=None):
    """`cargo test`, bounded twice: by its own timeout and by the attempt's
    remaining budget.

    Both, because they answer different questions. The timeout catches a program
    that does not terminate; the deadline stops an attempt from running past the
    budget it was given. The first version had neither working — a ten-minute
    timeout inside a two-minute budget — and one non-terminating candidate
    repair wedged a full run for thirty-nine minutes.
    """
    limit = TEST_TIMEOUT_S
    if deadline is not None:
        limit = max(0.5, min(limit, deadline - time.perf_counter()))
    try:
        ran = subprocess.run(
            ["cargo", "test", "--quiet", "--offline"],
            cwd=directory, capture_output=True, text=True, timeout=limit,
        )
    except subprocess.TimeoutExpired:
        return False, 0
    return ran.returncode == 0, len(ran.stdout) + len(ran.stderr)


class Attempt:
    """One agent's run at one task, in one environment."""

    def __init__(self, task, environment):
        self.task = task
        self.environment = environment
        self.started = None
        self.solved = False
        self.edits = 0
        self.builds = 0
        self.probes = 0
        self.inspections = 0
        self.revisions = 0
        self.fallbacks = 0
        self.bytes_in = 0
        self.bytes_out = 0
        self.warmup_ms = 0.0
        self.elapsed_ms = 0.0
        self.note = ""

    def record(self):
        return {
            "task": self.task["id"],
            "family": self.task["family"],
            "environment": self.environment,
            "solved": self.solved,
            "elapsed_ms": round(self.elapsed_ms, 1),
            "warmup_ms": round(self.warmup_ms, 1),
            "edits": self.edits,
            "builds": self.builds,
            "probes": self.probes,
            "inspections": self.inspections,
            "revisions": self.revisions,
            "fallbacks": self.fallbacks,
            "bytes_in": self.bytes_in,
            "bytes_out": self.bytes_out,
            "note": self.note,
        }


class CargoEnvironment:
    """Edit the file, run the tests. The conventional loop.

    Deliberately not made clever. No test selection, no incremental trickery
    beyond what cargo does by default — because that *is* the baseline, and
    beating a strawman would prove nothing.
    """

    name = "cargo"

    def __init__(self, directory):
        self.directory = directory
        self.source = directory / "src" / "lib.rs"
        self.deadline = None

    def warm(self, attempt):
        at = time.perf_counter()
        cargo_test(self.directory)
        attempt.warmup_ms = (time.perf_counter() - at) * 1e3

    def read(self, attempt):
        text = self.source.read_text()
        attempt.inspections += 1
        attempt.bytes_in += len(text)
        return text

    def write(self, attempt, text):
        attempt.edits += 1
        attempt.bytes_out += len(text)
        self.source.write_text(text)

    def check(self, attempt):
        """`cargo test`, and what it cost."""
        attempt.builds += 1
        passed, size = cargo_test(self.directory, self.deadline)
        attempt.bytes_in += size
        return passed

    def close(self):
        pass


class BlinkerEnvironment:
    """The seven verbs, over a resident session."""

    name = "blinker"

    def __init__(self, directory, backend):
        self.directory = directory
        self.backend = backend
        self.process = None
        self.deadline = None

    def warm(self, attempt):
        at = time.perf_counter()
        args = [str(SPIKE / "target/release/spike"),
                "--fixture", str(self.directory), "--serve"]
        if self.backend:
            args += ["--backend", str(self.backend)]
        self.process = subprocess.Popen(
            args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        self.call(attempt, op="open")
        attempt.warmup_ms = (time.perf_counter() - at) * 1e3

    def call(self, attempt, **request):
        line = json.dumps(request)
        attempt.bytes_out += len(line)
        self.process.stdin.write(line + "\n")
        self.process.stdin.flush()
        answer = self.process.stdout.readline()
        if not answer:
            return {"status": "error", "error": "the session died"}
        attempt.bytes_in += len(answer)
        observation = json.loads(answer)
        if request["op"] == "probe":
            attempt.probes += 1
        elif request["op"] in ("inspect", "callers"):
            attempt.inspections += 1
        elif request["op"] == "replace_body":
            attempt.edits += 1
            attempt.revisions += 1
            if observation.get("status") == "refused":
                attempt.fallbacks += 1
        return observation

    def close(self):
        if self.process:
            try:
                self.process.stdin.write('{"op":"quit"}\n')
                self.process.stdin.flush()
                self.process.stdin.close()
                self.process.wait(timeout=30)
            except (BrokenPipeError, ValueError, subprocess.TimeoutExpired):
                self.process.kill()


def oracle(directory):
    """The single definition of solved, run outside whichever environment
    produced the fix.

    Deliberately its own `cargo test`, even for the Blinker run. An environment
    that graded its own work would be measuring its own opinion — and the
    Blinker path in particular publishes into an arena, so "the tests pass" has
    to mean the *source on disk* is correct, not that some generation somewhere
    answered well.
    """
    return cargo_test(directory)[0]


def prepare(task_dir, work_root):
    """A private copy of a task, reset to `broken.rs` but keeping its `target/`.

    The reset has to happen — an attempt must never see the last one's edits —
    but throwing the build directory away with it is what made the first full
    run unusable. Every attempt then began with a *cold* `cargo build`: compile
    the library, compile the test harness, link, about eight seconds, and it was
    paid 294 times whether the agent did anything or not.

    So the copy is made once per task and reused. `src/lib.rs` is restored from
    `broken.rs` at the start of every attempt, which is the only state an agent
    can change, and `target/` stays warm. That is also the more honest baseline:
    a developer does not delete their build directory between hypotheses.
    """
    destination = work_root / task_dir.name
    if not destination.exists():
        shutil.copytree(task_dir, destination)
        # The fixture's crate path has to point at the copy, not the original.
        fixture = json.loads((destination / "fixture.json").read_text())
        fixture["crate"] = str(destination.resolve())
        (destination / "fixture.json").write_text(json.dumps(fixture, indent=2) + "\n")
    else:
        broken = (destination / "broken.rs").read_text()
        source = destination / "src" / "lib.rs"
        # Only if it actually differs. Writing identical bytes still moves the
        # mtime, cargo rebuilds on mtime, and the reset between attempts was
        # therefore paying for a full rebuild — of the same source — 294 times.
        if source.read_text() != broken:
            source.write_text(broken)
    return destination


def run(task, task_dir, work_root, agent, environment_name, backend, budget_s):
    """One attempt: warm, hand it to the agent under a budget, then grade."""
    directory = prepare(task_dir, work_root)
    # The agent works on the copy, never on the task set. Passed through the
    # task dict because an agent that had to be told where it was would be an
    # agent the harness could hand the wrong directory.
    task["_dir"] = directory
    attempt = Attempt(task, environment_name)
    if environment_name == "cargo":
        environment = CargoEnvironment(directory)
    else:
        environment = BlinkerEnvironment(directory, backend)

    try:
        environment.warm(attempt)
        started = time.perf_counter()
        deadline = started + budget_s
        environment.deadline = deadline
        try:
            agent(environment, attempt, task, deadline)
        except TimeoutError:
            attempt.note = "budget exhausted"
        except Exception as error:  # noqa: BLE001 — an agent's crash is a datum
            attempt.note = "agent raised %s: %s" % (type(error).__name__, error)
        attempt.elapsed_ms = (time.perf_counter() - started) * 1e3
    finally:
        environment.close()

    attempt.solved = oracle(directory)
    return attempt
