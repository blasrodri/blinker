#!/usr/bin/env python3
"""Generate the runtime-differential mutation suite (§30).

Every variant is produced from `pristine.rs` by a stated substitution, rather
than written out by hand. That is not tidiness: the differential's premise is
"the same starting revision, edited two ways", and a hand-written variant that
had drifted in some second, unnoticed respect would make a passing comparison
mean less than it looks like it means.

Everything is `wrapping_*` on purpose. `-Copt-level=0` leaves debug assertions
on, so an ordinary `+` emits an overflow check that calls
`core::panicking::panic_const_add_overflow` with a `&Location`, and a
`&Location` is a section-relative relocation, which the current DIRECT class
refuses. Arithmetic that can panic is outside the class today; §30 says so
rather than hiding it behind a fixture that avoids the question by accident.
"""

import pathlib
import sys

HERE = pathlib.Path(__file__).parent
VARIANTS = HERE / "fixtures" / "differential" / "variants"

PRISTINE_BODY = """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(total);"""

PRISTINE_BLEND = """#[inline(never)]
fn blend(x: u64) -> u64 {
    diff_second(x).wrapping_add(7)
}"""

# (name, what it exercises, [(anchor, replacement), ...])
MUTATIONS = [
    (
        "body_arith",
        "arithmetic in the root body",
        [(PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(total).wrapping_mul(11).wrapping_add(2);""")],
    ),
    (
        "call_existing",
        "a second call to a function the closure already contained",
        [(PRISTINE_BODY, """    let reading = Reading { value, scale };
    let other = Reading { value: scale as u64, scale: 3 };
    let total = reading.total().wrapping_add(other.total());
    let mixed = blend(total);""")],
    ),
    (
        "new_local_helper",
        "a function that did not exist in the previous revision",
        [
            (PRISTINE_BLEND, PRISTINE_BLEND + """

#[inline(never)]
fn skew(x: u64) -> u64 {
    x.rotate_left(3).wrapping_add(5)
}"""),
            (PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = skew(blend(total));"""),
        ],
    ),
    (
        "new_generic",
        "a generic instantiated at a type the crate had not instantiated it at",
        [
            (PRISTINE_BLEND, PRISTINE_BLEND + """

#[inline(never)]
fn fold<T: Copy + core::ops::BitXor<Output = T>>(a: T, b: T) -> T {
    a ^ b
}"""),
            (PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(fold(total, 0x5555u64));"""),
        ],
    ),
    (
        "read_static",
        "reading a static that already exists in the base image",
        [(PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(total).wrapping_add(DIFF_BASE);""")],
    ),
    (
        "multi_function",
        "two functions of one closure changed in the same revision",
        [
            (PRISTINE_BLEND, """#[inline(never)]
fn blend(x: u64) -> u64 {
    diff_second(x).wrapping_mul(31).wrapping_add(9)
}"""),
            (PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(total).wrapping_add(4);"""),
        ],
    ),
    (
        "branch_cold",
        "a branch only some probes take, so one path is newly generated "
        "code that most inputs never reach",
        [(PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = if scale > 8 {
        blend(total).wrapping_mul(13)
    } else {
        blend(total)
    };""")],
    ),
    (
        "loop_edit",
        "a loop whose trip count depends on the probe input",
        [(PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let limit = if scale < 4 { scale } else { 4 };
    let mut acc = total;
    let mut i = 0u32;
    while i < limit {
        acc = acc.wrapping_mul(3).wrapping_add(1);
        i = i.wrapping_add(1);
    }
    let mixed = blend(acc);""")],
    ),
    # Both patchable functions changed, for §40's generation scenarios. A
    # concurrent probe reads two gates from the generation it captured; if the
    # revision it is compared against changes only one of them, the second gate
    # returns the same number under either generation and proves nothing.
    (
        "two_gates",
        "both `extern \"C\"` members of the closure changed",
        [
            (
                """pub extern "C" fn diff_second(x: u64) -> u64 {
    x.wrapping_mul(3).wrapping_add(1)
}""",
                """pub extern "C" fn diff_second(x: u64) -> u64 {
    x.wrapping_mul(29).wrapping_add(6)
}""",
            ),
            (PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(total).wrapping_mul(17).wrapping_add(8);"""),
        ],
    ),
    # The edit the classifier cannot see. `diff_entry` is not the hot root and
    # is not in its closure, so nothing S0c examines changes — yet a clean
    # rebuild behaves differently, because the live path leaves the base
    # image's `diff_entry` in place. Only a behavioural comparison can catch
    # this, which is the entire argument for having one.
    (
        "edit_outside_closure",
        "a function outside the patch closure changed in the same revision",
        [("""    let through = unsafe { DIFF_GATE }.unwrap_or(diff_root);
    through(value, scale, out)""", """    let through = unsafe { DIFF_GATE }.unwrap_or(diff_root);
    through(value, scale, out).wrapping_add(1)""")],
    ),
    # Not a DIRECT edit. Included so the suite exercises a refusal as well as
    # an acceptance: a run in which nothing is ever refused cannot distinguish
    # a working classifier from one that says yes to everything.
    (
        "new_static",
        "introducing a static, which S0c refuses — the base image has no "
        "storage for it",
        [
            (PRISTINE_BLEND, PRISTINE_BLEND + """

static DIFF_FRESH: u64 = 77;"""),
            (PRISTINE_BODY, """    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(total).wrapping_add(DIFF_FRESH);"""),
        ],
    ),
]


def main() -> int:
    pristine = (VARIANTS / "pristine.rs").read_text()
    for name, _purpose, edits in MUTATIONS:
        text = pristine
        for anchor, replacement in edits:
            if anchor not in text:
                print(f"{name}: anchor not found in pristine.rs", file=sys.stderr)
                return 1
            text = text.replace(anchor, replacement, 1)
        if text == pristine:
            print(f"{name}: produced a file identical to pristine", file=sys.stderr)
            return 1
        (VARIANTS / f"{name}.rs").write_text(text)
        print(f"  {name:18} {_purpose}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
