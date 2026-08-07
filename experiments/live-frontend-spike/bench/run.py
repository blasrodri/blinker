#!/usr/bin/env python3
"""Run Agent Bench and report it.

    python3 bench/run.py --agent null --agent oracle --agent searcher

Every combination of agent × environment × task, under a wall-clock budget,
with the controls run first: if `null` solves anything or `oracle` solves
nothing, the run stops rather than reporting numbers nobody should read.
"""

import argparse
import json
import os
import pathlib
import statistics
import sys
import tempfile
import time

HERE = pathlib.Path(__file__).parent
sys.path.insert(0, str(HERE))
import agents  # noqa: E402
import harness  # noqa: E402


def summarise(records):
    by = {}
    for record in records:
        key = (record["agent"], record["environment"])
        by.setdefault(key, []).append(record)
    rows = []
    for (agent, environment), group in sorted(by.items()):
        solved = [r for r in group if r["solved"]]
        times = sorted(r["elapsed_ms"] for r in solved)
        rows.append({
            "agent": agent,
            "environment": environment,
            "tasks": len(group),
            "solved": len(solved),
            "rate": len(solved) / max(len(group), 1),
            "at_1s": sum(1 for r in solved if r["elapsed_ms"] <= 1_000) / max(len(group), 1),
            "at_5s": sum(1 for r in solved if r["elapsed_ms"] <= 5_000) / max(len(group), 1),
            "at_30s": sum(1 for r in solved if r["elapsed_ms"] <= 30_000) / max(len(group), 1),
            "median_ms": statistics.median(times) if times else None,
            "total_s": sum(r["elapsed_ms"] for r in group) / 1e3,
            "builds": sum(r["builds"] for r in group),
            "revisions": sum(r["revisions"] for r in group),
            "fallbacks": sum(r["fallbacks"] for r in group),
            "probes": sum(r["probes"] for r in group),
            "bytes": sum(r["bytes_in"] + r["bytes_out"] for r in group),
            "warmup_s": sum(r["warmup_ms"] for r in group) / 1e3,
        })
    return rows


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tasks", type=pathlib.Path,
                        default=pathlib.Path(os.environ.get("TMPDIR", "/tmp")) / "agentbench-tasks")
    parser.add_argument("--agent", action="append", default=[])
    parser.add_argument("--environment", action="append", default=[])
    parser.add_argument("--backend", default=None)
    parser.add_argument("--budget", type=float, default=60.0)
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--out", type=pathlib.Path)
    # A ceiling on the whole run, not only on one attempt. The first full run
    # had no such thing and cost thirty-nine minutes to a single hang.
    parser.add_argument("--max-total", type=float, default=1800.0)
    parser.add_argument("--work", type=pathlib.Path)
    parser.add_argument("--keep", action="store_true")
    options = parser.parse_args()

    chosen = options.agent or ["null", "oracle", "searcher"]
    environments = options.environment or ["cargo", "blinker"]
    index = json.loads((options.tasks / "index.json").read_text())
    if options.limit:
        index = index[: options.limit]

    records = []
    began = time.perf_counter()
    stopped_early = None
    # Persistent across the whole run, so each task's `target/` is built once
    # rather than 294 times. Removed at the end unless `--keep` says otherwise.
    work = options.work or pathlib.Path(tempfile.mkdtemp(prefix="agentbench-"))
    work.mkdir(parents=True, exist_ok=True)
    print("  work: %s" % work, flush=True)
    if True:
        for name in chosen:
            agent = agents.AGENTS[name]
            for environment in environments:
                print("\n  %s / %s" % (name, environment), flush=True)
                for task in index:
                    if time.perf_counter() - began > options.max_total:
                        stopped_early = "the run hit its --max-total ceiling"
                        break
                    task_dir = options.tasks / task["id"]
                    task = dict(task)
                    attempt = harness.run(
                        task, task_dir, work, _bind(agent, task),
                        environment, options.backend, options.budget,
                    )
                    record = attempt.record()
                    record["agent"] = name
                    records.append(record)
                    print("    %-44s %-7s %7.0f ms%s" % (
                        record["task"],
                        "solved" if record["solved"] else "unsolved",
                        record["elapsed_ms"],
                        "  " + record["note"] if record["note"] else "",
                    ), flush=True)

    if not options.keep and options.work is None:
        import shutil
        shutil.rmtree(work, ignore_errors=True)
    if stopped_early:
        # Named, never silent. A partial run reported as a whole one is the
        # worst kind of benchmark result.
        print("\n  STOPPED EARLY: %s (%d of %d attempts)" % (
            stopped_early, len(records), len(index) * len(chosen) * len(environments)))
    rows = summarise(records)
    print("\n  %-9s %-8s %5s %6s %7s %7s %8s %7s %6s %6s %9s" % (
        "agent", "env", "n", "solved", "@1s", "@5s", "median", "builds", "revs", "fb", "bytes"))
    for row in rows:
        print("  %-9s %-8s %5d %5d%% %6.0f%% %6.0f%% %7s %7d %6d %6d %9d" % (
            row["agent"], row["environment"], row["tasks"],
            round(100 * row["rate"]), 100 * row["at_1s"], 100 * row["at_5s"],
            "%.0f" % row["median_ms"] if row["median_ms"] else "-",
            row["builds"], row["revisions"], row["fallbacks"], row["bytes"],
        ))

    # The controls, checked before the comparison is allowed to mean anything.
    ok = True
    for row in rows:
        if row["agent"] == "null" and row["solved"]:
            ok = False
            print("\n  the null agent solved %d task(s) in %s: the oracle is not "
                  "discriminating" % (row["solved"], row["environment"]))
        if row["agent"] == "oracle" and row["solved"] < row["tasks"]:
            print("\n  the oracle agent left %d task(s) unsolved in %s — each is a fix "
                  "that environment cannot express" % (
                      row["tasks"] - row["solved"], row["environment"]))
    if options.out:
        options.out.write_text(json.dumps(
            {"records": records, "summary": rows}, indent=2) + "\n")
    return 0 if ok else 1


def _bind(agent, task):
    def bound(environment, attempt, _task, deadline):
        return agent(environment, attempt, task, deadline)
    return bound


if __name__ == "__main__":
    raise SystemExit(main())
