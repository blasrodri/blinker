#!/usr/bin/env python3
"""Run the adversarial suite and check every verdict against its expectation.

A case passes only when the verdict *and* the reason match. A FALLBACK for the
wrong reason is a pass by accident: it means the classifier refused on a
predicate that happens to fire today, and the predicate that should have
refused it is untested and free to break.

Cases whose expected verdict is `null` are open questions rather than
assertions — they are reported with whatever the classifier decided and are
not failures. Recording an undecided case as passing would be the same mistake
as a FALLBACK for the wrong reason, one level up.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPIKE = HERE / "target" / "release" / "spike"


def run_case(fixture: Path, edit: str, hot: str) -> dict:
    result = subprocess.run(
        [str(SPIKE), "--fixture", str(fixture), "--edit", edit,
         "--hot", hot, "--iterations", "1", "--warmup", "0"],
        capture_output=True, text=True,
    )
    start = result.stdout.find("[")
    if start < 0:
        return {"error": result.stderr[-500:] or "no records"}
    records = json.loads(result.stdout[start:])
    if not records:
        return {"error": "no records"}
    return records[-1]


def reason_of(verdict: dict) -> str:
    if not verdict:
        return "none"
    if verdict.get("verdict") == "direct":
        return "-"
    reason = verdict.get("reason")
    if isinstance(reason, str):
        return reason
    if isinstance(reason, dict):
        return next(iter(reason), "unknown")
    return "unknown"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", default=str(HERE / "fixtures" / "adversarial"))
    parser.add_argument("--out", default=str(HERE / "results" / "s0c-verdicts.json"))
    options = parser.parse_args()

    fixture = Path(options.fixture)
    described = json.loads((fixture / "fixture.json").read_text())
    expectations = described["expectations"]

    rows, failures, undecided = [], [], []
    for name, expected in expectations.items():
        record = run_case(fixture, name, expected["hot"])
        if record.get("error"):
            rows.append({"case": name, "got": "ERROR", "detail": record["error"][:200]})
            failures.append(f"{name}: the session failed — {record['error'][:160]}")
            continue
        verdict = record.get("verdict")
        got = "DIRECT" if verdict and verdict.get("verdict") == "direct" else "FALLBACK"
        got_reason = reason_of(verdict)
        row = {
            "case": name,
            "hot": expected["hot"],
            "expected": expected["verdict"],
            "expected_reason": expected["reason"],
            "got": got,
            "got_reason": got_reason,
            "classify_ms": record.get("classify_ms"),
            "closure": record.get("mono_items_examined"),
        }
        rows.append(row)

        if expected["verdict"] is None:
            undecided.append(f"{name}: undecided by design, classifier said {got}({got_reason})")
            continue
        if got != expected["verdict"]:
            failures.append(f"{name}: expected {expected['verdict']}, got {got}({got_reason})")
        elif expected["reason"] and got_reason != expected["reason"]:
            failures.append(
                f"{name}: {got} for the wrong reason — expected {expected['reason']},"
                f" got {got_reason}"
            )

    width = max(len(r["case"]) for r in rows)
    for row in rows:
        if row.get("got") == "ERROR":
            print(f"  {row['case']:<{width}}  ERROR  {row['detail'][:80]}")
            continue
        mark = " "
        if row["expected"] is None:
            mark = "?"
        elif row["got"] != row["expected"] or (
            row["expected_reason"] and row["got_reason"] != row["expected_reason"]
        ):
            mark = "X"
        detail = row["got"] if row["got"] == "DIRECT" else f"{row['got']}({row['got_reason']})"
        print(f"{mark} {row['case']:<{width}}  {detail:<44}"
              f" classify {row['classify_ms'] or 0:.3f} ms  closure {row['closure']}")

    Path(options.out).parent.mkdir(parents=True, exist_ok=True)
    Path(options.out).write_text(json.dumps(rows, indent=2) + "\n")

    if undecided:
        print("\n  undecided (reported, not asserted):")
        for line in undecided:
            print(f"    {line}")
    if failures:
        print(f"\n  {len(failures)} FAILED:")
        for line in failures:
            print(f"    {line}")
        sys.exit(1)
    decided = sum(1 for r in rows if r.get("expected") is not None)
    print(f"\n  {decided} cases passed, {len(undecided)} undecided")


if __name__ == "__main__":
    main()
