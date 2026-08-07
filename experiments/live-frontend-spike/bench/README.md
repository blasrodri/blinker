# Agent Bench

Does the agent API (§44) make an agent better, or only faster to watch?

```
python3 bench/generate.py                    # 49 tasks, verified, into $TMPDIR
python3 bench/run.py --backend <cg_clif.dylib> \
    --work /tmp/agentbench-work --keep
```

## What is here

| file | |
|---|---|
| `domains.py` | four crates and the defects seeded into them |
| `generate.py` | renders and **verifies** every task, and drops the ones that prove nothing |
| `harness.py` | the two environments, and what an attempt costs in each |
| `agents.py` | `null`, `oracle`, `searcher` |
| `run.py` | the sweep, the summary, and the control checks |

Tasks are generated into `$TMPDIR/agentbench-tasks`, not into the repository.
They are build output — `domains.py` reproduces them exactly — and 49 generated
cargo crates inside a working tree make an editor's Rust indexer take 327% CPU
analysing the benchmark while the benchmark runs.

## The rules it follows

**A task must fail before and pass after.** A seeded defect the suite cannot see
is a hole in the suite, and it is dropped and named rather than shipped.

**Expectations are measured, not written.** They come from running the pristine
crate. Writing them by hand cost nine tasks in the first draft, because the
arithmetic was wrong.

**The filter has to be demonstrable.** Two candidates are injected on every run
that must never survive verification — an invisible edit, and a real defect
checked against a bent expectation. If either is emitted, generation fails.

**Grading is external.** Always a separate `cargo test`, including for the
Blinker runs, because "solved" has to mean the source on disk is right rather
than that some generation answered well.

**Nothing is unbounded.** Each `cargo test` is bounded by its own timeout *and*
by the attempt's remaining budget; the run has a `--max-total` ceiling that
prints `STOPPED EARLY` rather than presenting a partial sweep as a whole one. A
candidate repair can write a program that does not terminate, and one did.

## What it does not measure

Models. `searcher` is a fixed policy with a table of repairs, so its success
rate is a fact about the table. What it measures honestly is the cost per
hypothesis in each environment, which is what M3 and M4 turn into a claim.
