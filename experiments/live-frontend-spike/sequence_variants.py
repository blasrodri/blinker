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
import re
import sys

HERE = pathlib.Path(__file__).parent

# (name, multiplier, addend) — the value at (3, 5) is 15 * multiplier + addend.
REVISIONS = [
    ("body_arith", 11, 2),
    ("body_arith2", 3, 9),
    ("body_arith3", 23, 4),
]

FIXTURES = ["small", "rg-lib", "blinker-lib", "medium", "large"]

# A revision the classifier must refuse, so the soak exercises recovery as
# often as it exercises success.
#
# The first attempt introduced a `static` and read it, on the assumption that
# the base image would have nowhere to put the storage. At `-Copt-level=0` that
# is refused, and the differential fixture confirms it. At `-Copt-level=3` —
# which is what the captured cargo invocation for these fixtures uses — rustc
# folds the read of an immutable static, so the patch closure genuinely never
# references it and DIRECT is the *correct* verdict. The classifier was right
# and the fixture was wrong: it reasons about the closure that exists, not
# about the source text that produced it.
#
# `#[inline]` cannot be optimized away, because it is the thing being asked
# about: an inlinable body may already be embedded in a downstream crate, so
# replacing this copy would leave the old one running. The arithmetic changes
# too, so that a refusal which wrongly published would answer 198 rather than
# whatever the previous revision answered.
FALLBACK_MULTIPLIER = 13
FALLBACK_ADDEND = 3


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

        signature = re.search(r"pub fn (spike_)?hot_root\(", pristine)
        if signature:
            # The nearest `#[inline(never)]` above the hot root, not the first
            # one in the file: other functions carry it too.
            attribute = pristine.rfind("#[inline(never)]", 0, signature.start())
            if attribute == -1:
                print(f"{fixture}: the hot root has no #[inline(never)]", file=sys.stderr)
                return 1
            text = (
                pristine[:attribute]
                + "#[inline]"
                + pristine[attribute + len("#[inline(never)]") :]
            ).replace(
                anchor,
                f"reading.total().wrapping_mul({FALLBACK_MULTIPLIER})"
                f".wrapping_add({FALLBACK_ADDEND})",
                1,
            )
            (variants / "fallback_inline.rs").write_text(text)
        if False:
            # Appended, not inserted above the function. Inserting before
            # `pub fn …` put the static between the hot root's attributes and
            # the hot root, so `#[inline(never)]` landed on a static and the
            # crate stopped compiling — which the soak then reported as a
            # failed session rather than as a refused revision. A top-level
            # item can go anywhere in the module, so it goes somewhere with no
            # attributes above it.
            pass
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
