#!/usr/bin/env python3
"""S0b: what does an edit to a *library* crate cost?

S0 measured a single-crate edit — the edited file always belonged to the crate
being compiled — and said so as its first limitation. That is the easy
topology. The common one is an edit to a library that other crates depend on,
and there the question splits in two.

**What a developer pays today.** cargo rebuilds the edited crate and then every
crate downstream of it, because a changed rlib changes the inputs of everything
that reads it. This is timed as `cargo build` of the binary at the top of the
graph, which is the wall clock a person actually waits through.

**What Blinker Live would have to pay.** If an edit is confined to a function
body and changes no exported signature and no type layout, then no dependent
crate's *code* changes — only that one function's MIR. A live runtime could
validate the edited crate alone and replace the function, provided it can prove
the crate's interface is unchanged. So the second measurement compiles only the
edited library and stops at validated MIR.

The gap between those two numbers is the entire value of the product on this
edit class. Neither number means anything without the other, which is why they
are measured together and reported side by side.

    ./blast.py --fixtures blinker-lib rg-lib
"""

import argparse
import json
import shutil
import statistics
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPIKE = HERE / "target" / "release" / "spike"


def run(command, cwd=None):
    return subprocess.run(command, cwd=cwd, capture_output=True, text=True)


def cargo_baseline(fixture: dict, variants: Path, source: Path, rounds: int) -> dict:
    """Time `cargo build` of the whole binary after editing the leaf library.

    Alternating, exactly as the linker's own relink harness does: an edit
    applied twice in a row is a rebuild of text cargo has already seen, and
    the second one measures nothing. Both directions are real edits.
    """
    project = Path(fixture["project"])
    downstream = fixture.get("downstream_package")
    if not downstream:
        return {}
    build = ["cargo", f"+{fixture['toolchain']}", "build", "--release", "-p", downstream]

    pristine = (variants / "pristine.rs").read_text()
    edited = (variants / "body_arith.rs").read_text()

    # Warm: everything downstream must already be built, or the first timed
    # round measures a cold workspace.
    source.write_text(pristine)
    first = run(build, cwd=project)
    if first.returncode != 0:
        return {"error": first.stderr[-600:]}

    walls = []
    for index in range(rounds):
        source.write_text(edited if index % 2 == 0 else pristine)
        started = time.perf_counter()
        result = run(build, cwd=project)
        elapsed = (time.perf_counter() - started) * 1e3
        if result.returncode != 0:
            return {"error": result.stderr[-600:]}
        walls.append(elapsed)
    source.write_text(pristine)
    run(build, cwd=project)
    return {
        "rounds": rounds,
        "p50": round(statistics.median(walls), 1),
        "min": round(min(walls), 1),
        "max": round(max(walls), 1),
    }


def live_lower_bound(fixture_dir: Path, edits: list, iterations: int) -> dict:
    """Compile only the edited library, to validated MIR."""
    out = {}
    for edit in edits:
        if not (fixture_dir / "variants" / f"{edit}.rs").exists():
            continue
        result = run([
            str(SPIKE), "--fixture", str(fixture_dir), "--edit", edit,
            "--iterations", str(iterations), "--warmup", "3",
        ])
        start = result.stdout.find("[")
        if start < 0:
            out[edit] = {"error": result.stderr[-400:]}
            continue
        records = [r for r in json.loads(result.stdout[start:]) if not r["error"]]
        if not records:
            out[edit] = {"error": "every session failed"}
            continue
        values = sorted(r["total_required_frontend_ms"] for r in records)
        out[edit] = {
            "n": len(values),
            "p50": round(values[len(values) // 2], 2),
            "p95": round(values[min(len(values) - 1, int(0.95 * (len(values) - 1)))], 2),
            "items": records[0]["mono_items_examined"],
        }
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", nargs="+", default=["blinker-lib", "rg-lib"])
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--cargo-rounds", type=int, default=6)
    parser.add_argument("--edits", nargs="+",
                        default=["body_arith", "body_existing_call", "body_new_generic",
                                 "signature", "type_layout"])
    parser.add_argument("--out", default=str(HERE / "results"))
    options = parser.parse_args()

    out = Path(options.out)
    out.mkdir(parents=True, exist_ok=True)
    summary = []
    for name in options.fixtures:
        fixture_dir = HERE / "fixtures" / name
        described = json.loads((fixture_dir / "fixture.json").read_text())
        source = Path(described["file"])
        print(f"\n  {name}  ({described['crate_name']}, {described['crate_type']})")

        live = live_lower_bound(fixture_dir, options.edits, options.iterations)
        for edit, stats in live.items():
            if "error" in stats:
                print(f"    live  {edit:20s} FAILED {stats['error'][:80]}")
            else:
                print(f"    live  {edit:20s} p50 {stats['p50']:8.2f}  p95 {stats['p95']:8.2f}"
                      f"  ({stats['items']} instances)")

        cargo = cargo_baseline(described, fixture_dir / "variants", source,
                               options.cargo_rounds)
        if cargo.get("error"):
            print(f"    cargo baseline FAILED: {cargo['error'][:200]}")
        elif cargo:
            print(f"    cargo full downstream rebuild  p50 {cargo['p50']:.0f} ms"
                  f"  (min {cargo['min']:.0f}, max {cargo['max']:.0f})")
            body = live.get("body_arith", {})
            if "p50" in body and body["p50"]:
                print(f"    ratio: cargo is {cargo['p50'] / body['p50']:.0f}x the live"
                      f" lower bound for a body edit")
        summary.append({"fixture": name, "crate": described["crate_name"],
                        "live": live, "cargo_downstream": cargo})

    (out / "s0b-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(f"\n  {out / 's0b-summary.json'}")


if __name__ == "__main__":
    main()
