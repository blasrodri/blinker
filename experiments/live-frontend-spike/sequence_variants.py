#!/usr/bin/env python3
"""Generate the successive revisions §35 drives back to back.

`pristine` computes `total() * 7 + 1` and `body_arith` computes
`total() * 11 + 2`. Two more of the same shape make a four-revision sequence
G0 → G1 → G2 → G3, each with an exactly predictable answer for the probe
`value = 3, scale = 5`, where `total()` is 15.

Generated from `pristine.rs` by substitution, for the same reason §30's
mutations are: the point of the sequence is that each revision differs from the
last in exactly one stated way, and a hand-edited file that had drifted would
make a passing run mean less than it looks like.
"""

import pathlib
import sys

HERE = pathlib.Path(__file__).parent

# (name, multiplier, addend) — the value at (3, 5) is 15 * multiplier + addend.
REVISIONS = [
    ("body_arith", 11, 2),
    ("body_arith2", 3, 9),
    ("body_arith3", 23, 4),
]

FIXTURES = ["small", "rg-lib", "blinker-lib", "medium", "large"]


def main() -> int:
    for fixture in FIXTURES:
        variants = HERE / "fixtures" / fixture / "variants"
        pristine_path = variants / "pristine.rs"
        if not pristine_path.exists():
            continue
        pristine = pristine_path.read_text()
        anchor = "reading.total().wrapping_mul(7).wrapping_add(1)"
        if anchor not in pristine:
            print(f"{fixture}: no hot-root body to vary", file=sys.stderr)
            continue
        for name, multiplier, addend in REVISIONS:
            text = pristine.replace(
                anchor,
                f"reading.total().wrapping_mul({multiplier}).wrapping_add({addend})",
                1,
            )
            (variants / f"{name}.rs").write_text(text)
        print(
            f"  {fixture:12} "
            + " ".join(
                f"{name}={15 * multiplier + addend}"
                for name, multiplier, addend in REVISIONS
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
