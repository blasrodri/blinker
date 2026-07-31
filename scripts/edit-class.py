#!/usr/bin/env python3
"""Classify what an edit did to a Rust project's object files.

Why this exists
---------------

The incremental cache's hit rate depends entirely on *what kind of edits people
actually make*. Findings 42 and 43 established the mechanics — every section
after an edit moves, and body edits grow a contribution by 0.4-2.5%, so ~5%
slop absorbs them. What none of that establishes is how often a real edit
exceeds the budget, and eight hand-written examples cannot answer it.

This measures a single edit, so it can be run over many and the distribution
accumulated. It classifies into the buckets the cache design cares about:

  unchanged   the codegen unit's bytes are identical — free, cache hit
  body        same symbols, same sizes, different content — patch in place
  grew        same symbols, larger — needs slop; reported as a percentage
  additive    new symbols, existing ones unchanged — append, no relayout
  cascading   symbols removed or renamed — relayout, cold link

The classification is deliberately conservative: anything it cannot prove is
`body` or `additive` is reported as `cascading`, because a cache that guesses
wrong emits a subtly broken binary rather than a slow one.

Usage
-----

    scripts/edit-class.py <before-dir> <after-dir>

Each directory holds the `.rcgu.o` files from one build. Capture them with a
shim linker that copies its `*.o` arguments aside — see FINDINGS.md finding 15
for why the *filenames* cannot be compared directly (they carry a per-build
session id) and must be keyed on the codegen-unit component instead.
"""

import subprocess
import sys
from collections import Counter
from pathlib import Path


def cgu_key(path):
    """The stable codegen-unit identity inside an object's filename.

    `crate-<hash>.<cgu>.<session>.rcgu.o` — the middle component is stable
    across builds, the last one changes every time (finding 15).
    """
    parts = path.name.split(".")
    return parts[1] if len(parts) >= 3 else path.name


def symbols(path):
    """Defined symbols and the section sizes they live in."""
    result = subprocess.run(
        ["nm", "-jU", str(path)], capture_output=True, text=True
    )
    return set(s for s in result.stdout.split() if s)


def text_size(path):
    result = subprocess.run(["otool", "-l", str(path)], capture_output=True, text=True)
    seen = False
    for line in result.stdout.splitlines():
        stripped = line.strip()
        if stripped.startswith("sectname __text"):
            seen = True
        elif seen and stripped.startswith("size"):
            return int(stripped.split()[1], 16)
    return 0


def classify(before, after):
    if before.read_bytes() == after.read_bytes():
        return "unchanged", 0.0

    old_syms, new_syms = symbols(before), symbols(after)
    if old_syms - new_syms:
        return "cascading", 0.0

    old_size, new_size = text_size(before), text_size(after)
    growth = (new_size - old_size) / old_size * 100 if old_size else 0.0

    if new_syms - old_syms:
        return "additive", growth
    if new_size == old_size:
        return "body", 0.0
    if new_size < old_size:
        # Shrinking is safe for slop but still moves nothing only if the
        # contribution keeps its slot; treated as a body edit that wastes space.
        return "body", growth
    return "grew", growth


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.strip().split("Usage")[1].strip())

    before_dir, after_dir = Path(sys.argv[1]), Path(sys.argv[2])
    before = {cgu_key(p): p for p in before_dir.glob("*.o")}
    after = {cgu_key(p): p for p in after_dir.glob("*.o")}

    counts = Counter()
    growths = []
    print(f"{'codegen unit':<28} {'class':<11} {'growth':>8}")
    for key in sorted(set(before) | set(after)):
        if key not in before:
            counts["additive"] += 1
            print(f"  {key[:26]:<26} {'new-cgu':<11} {'-':>8}")
            continue
        if key not in after:
            counts["cascading"] += 1
            print(f"  {key[:26]:<26} {'removed':<11} {'-':>8}")
            continue
        kind, growth = classify(before[key], after[key])
        counts[kind] += 1
        if kind != "unchanged":
            growths.append(growth)
            print(f"  {key[:26]:<26} {kind:<11} {growth:+7.1f}%")

    total = sum(counts.values())
    print(f"\n{total} codegen units")
    for kind in ("unchanged", "body", "grew", "additive", "cascading"):
        if counts[kind]:
            print(f"  {kind:<11} {counts[kind]:4}  ({counts[kind] / total * 100:.0f}%)")

    over = [g for g in growths if g > 5.0]
    if growths:
        print(f"\n  max growth {max(growths):+.1f}%   over 5% slop budget: {len(over)}")
        if over:
            print("  NOTE: those would force a relayout and a cold link.")


if __name__ == "__main__":
    main()
