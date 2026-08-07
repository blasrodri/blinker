#!/usr/bin/env python3
"""The crates Agent Bench tasks are cut from, and the defects seeded into them.

Why synthesised rather than scraped
-----------------------------------

A benchmark has to know the right answer, and it has to be re-runnable. A task
mined from a real repository gives you neither for free: the "fix" is whatever
the maintainer happened to commit, the test that proves it may not exist, and
the crate's dependencies rot. Every one of those turns a measurement into an
argument.

So each task here is a known-good crate plus one seeded defect, which makes the
ground truth exact — the fix *is* the pristine text — and lets the DIRECT and
FALLBACK mix be chosen rather than discovered. The cost is external validity,
and it is a real cost: these are bugs of a realistic *shape* in crates smaller
than real ones. Tasks drawn from real repositories are the obvious next thing
and are not here.

What a domain is
----------------

A small, self-contained computation with four to eight functions and a handful
of tests, written inside the artifact class §46 leaves: no trait objects, no
generics as roots, no `async`. Constant data is fine now, so the code can index,
panic and use array literals like ordinary Rust.

Tests are `extern "C" fn(*mut u64) -> u64` — write what you expected, return
what you got — so the same functions serve the Blinker API's `run_affected` and
a `cargo test` target. One source of truth for what "solved" means, rather than
two that can disagree.

What a defect is
----------------

A textual substitution against the pristine source, with the family it belongs
to and the verdict it should get. Generation *verifies* every one: the tests
must fail before and pass after. A seeded defect that the tests do not catch is
not a task, it is a hole in the suite, and it is dropped and counted.
"""

import textwrap

# --------------------------------------------------------------------------
# Domain 1: a decimal scanner. A state machine over bytes, where the classic
# defect is forgetting to flush at end of input.
# --------------------------------------------------------------------------

SCANNER = r'''
//! A decimal scanner: sum the numbers in a byte buffer.

#![allow(dead_code)]

pub const CLASS_OTHER: u32 = 0;
pub const CLASS_DIGIT: u32 = 1;
pub const CLASS_SEP: u32 = 2;

#[no_mangle]
#[inline(never)]
pub extern "C" fn classify_byte(b: u8) -> u32 {
    if b >= b'0' && b <= b'9' {
        CLASS_DIGIT
    } else if b == b' ' || b == b',' {
        CLASS_SEP
    } else {
        CLASS_OTHER
    }
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn accumulate(acc: u64, b: u8) -> u64 {
    acc.wrapping_mul(10).wrapping_add((b - b'0') as u64)
}

/// Sum every decimal number in `data`.
#[no_mangle]
#[inline(never)]
pub extern "C" fn scan(data: *const u8, len: u64) -> u64 {
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
}

/// How many numbers `scan` would have added.
#[no_mangle]
#[inline(never)]
pub extern "C" fn count_numbers(data: *const u8, len: u64) -> u64 {
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
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn total_scaled(data: *const u8, len: u64, scale: u64) -> u64 {
    scan(data, len).wrapping_mul(scale)
}
'''

SCANNER_TESTS = [
    ("test_two_numbers", "12,18", "scan(p, LEN)"),
    ("test_trailing_separator", "12,18,", "scan(p, LEN)"),
    ("test_space_separated", "12 18", "scan(p, LEN)"),
    ("test_single", "7", "scan(p, LEN)"),
    ("test_empty_prefix", ",5", "scan(p, LEN)"),
    ("test_wide", "123,4", "scan(p, LEN)"),
    ("test_count", "12,18", "count_numbers(p, LEN)"),
    ("test_count_one", "9", "count_numbers(p, LEN)"),
    ("test_scaled", "7,3", "total_scaled(p, LEN, 3)"),
    ("test_classify_digit", "", "classify_byte(b'4') as u64"),
    ("test_classify_space", "", "classify_byte(b' ') as u64"),
    ("test_classify_other", "", "classify_byte(b'x') as u64"),
]

# --------------------------------------------------------------------------
# Domain 2: a run-length decoder. Length/value pairs, where the defects are
# about bounds and about which of the pair is which.
# --------------------------------------------------------------------------

RUNLENGTH = r'''
//! A run-length decoder: `[count, value, count, value, ...]`.

#![allow(dead_code)]

/// The decoded length of a run-length encoded buffer.
#[no_mangle]
#[inline(never)]
pub extern "C" fn decoded_len(data: *const u8, len: u64) -> u64 {
    let mut total: u64 = 0;
    let mut i: u64 = 0;
    while i + 1 < len {
        // SAFETY: `i + 1 < len`, so both reads are inside the buffer.
        let count = unsafe { *data.add(i as usize) } as u64;
        total = total.wrapping_add(count);
        i = i.wrapping_add(2);
    }
    total
}

/// The byte at `index` of the decoded stream, or `SENTINEL` past the end.
#[no_mangle]
#[inline(never)]
pub extern "C" fn decoded_at(data: *const u8, len: u64, index: u64) -> u64 {
    let mut seen: u64 = 0;
    let mut i: u64 = 0;
    while i + 1 < len {
        // SAFETY: as above.
        let count = unsafe { *data.add(i as usize) } as u64;
        let value = unsafe { *data.add((i + 1) as usize) } as u64;
        if index < seen.wrapping_add(count) {
            return value;
        }
        seen = seen.wrapping_add(count);
        i = i.wrapping_add(2);
    }
    SENTINEL
}

pub const SENTINEL: u64 = 256;

/// The sum of every decoded byte.
#[no_mangle]
#[inline(never)]
pub extern "C" fn decoded_sum(data: *const u8, len: u64) -> u64 {
    let mut sum: u64 = 0;
    let mut i: u64 = 0;
    // Bounded twice over. `decoded_len` is the answer this is supposed to
    // trust, and a defect in it — seeded or introduced by an agent — turns this
    // into a loop that does not stop. The cap is larger than any input here, so
    // it changes no correct answer, and it means a wrong one costs a wrong
    // number rather than a hung test binary.
    while i < decoded_len(data, len) && i < 4096 {
        sum = sum.wrapping_add(decoded_at(data, len, i));
        i = i.wrapping_add(1);
    }
    sum
}
'''

RUNLENGTH_TESTS = [
    ("test_len_simple", "\x03A\x02B", "decoded_len(p, LEN)"),
    ("test_len_empty", "", "decoded_len(p, LEN)"),
    ("test_len_odd_tail", "\x03A\x02", "decoded_len(p, LEN)"),
    ("test_at_first", "\x03A\x02B", "decoded_at(p, LEN, 0)"),
    ("test_at_last_of_run", "\x03A\x02B", "decoded_at(p, LEN, 2)"),
    ("test_at_boundary", "\x03A\x02B", "decoded_at(p, LEN, 3)"),
    ("test_at_past_end", "\x03A\x02B", "decoded_at(p, LEN, 9)"),
    ("test_sum", "\x03A\x02B", "decoded_sum(p, LEN)"),
]

# --------------------------------------------------------------------------
# Domain 3: bracket matching. Depth counting, where the defects are about
# order of operations and about the underflow case.
# --------------------------------------------------------------------------

BRACKETS = r'''
//! Bracket matching: depth, balance, and the first offending position.

#![allow(dead_code)]

pub const BALANCED: u64 = 0;
pub const UNOPENED: u64 = 1;
pub const UNCLOSED: u64 = 2;

#[no_mangle]
#[inline(never)]
pub extern "C" fn is_open(b: u8) -> u64 {
    if b == b'(' || b == b'[' || b == b'{' { 1 } else { 0 }
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn is_close(b: u8) -> u64 {
    if b == b')' || b == b']' || b == b'}' { 1 } else { 0 }
}

/// The greatest nesting depth reached.
#[no_mangle]
#[inline(never)]
pub extern "C" fn max_depth(data: *const u8, len: u64) -> u64 {
    let mut depth: u64 = 0;
    let mut best: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: `i < len`.
        let b = unsafe { *data.add(i as usize) };
        if is_open(b) == 1 {
            depth = depth.wrapping_add(1);
            if depth > best {
                best = depth;
            }
        } else if is_close(b) == 1 && depth > 0 {
            depth = depth.wrapping_sub(1);
        }
        i = i.wrapping_add(1);
    }
    best
}

/// `BALANCED`, `UNOPENED` or `UNCLOSED`.
#[no_mangle]
#[inline(never)]
pub extern "C" fn balance(data: *const u8, len: u64) -> u64 {
    let mut depth: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: `i < len`.
        let b = unsafe { *data.add(i as usize) };
        if is_open(b) == 1 {
            depth = depth.wrapping_add(1);
        } else if is_close(b) == 1 {
            if depth == 0 {
                return UNOPENED;
            }
            depth = depth.wrapping_sub(1);
        }
        i = i.wrapping_add(1);
    }
    if depth == 0 { BALANCED } else { UNCLOSED }
}
'''

BRACKETS_TESTS = [
    ("test_depth_nested", "((()))", "max_depth(p, LEN)"),
    ("test_depth_flat", "()()()", "max_depth(p, LEN)"),
    ("test_depth_mixed", "([{}])", "max_depth(p, LEN)"),
    # A leading close, so a machine that lets `depth` underflow is visible.
    ("test_depth_leading_close", ")()", "max_depth(p, LEN)"),
    ("test_balanced", "([{}])", "balance(p, LEN)"),
    ("test_unopened", ")(", "balance(p, LEN)"),
    ("test_unclosed", "((", "balance(p, LEN)"),
    ("test_depth_none", "abc", "max_depth(p, LEN)"),
    ("test_open_is_not_close", "", "is_close(b'(')"),
    ("test_close_is_close", "", "is_close(b')')"),
]

# --------------------------------------------------------------------------
# Domain 4: a rolling checksum. Arithmetic, where the defects are about the
# modulus, the order of the two halves, and the seed.
# --------------------------------------------------------------------------

CHECKSUM = r'''
//! A Fletcher-style rolling checksum.

#![allow(dead_code)]

pub const MODULUS: u64 = 255;

#[no_mangle]
#[inline(never)]
pub extern "C" fn fold(low: u64, b: u8) -> u64 {
    (low.wrapping_add(b as u64)) % MODULUS
}

/// The low half: every byte, folded.
#[no_mangle]
#[inline(never)]
pub extern "C" fn low_half(data: *const u8, len: u64) -> u64 {
    let mut low: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: `i < len`.
        low = fold(low, unsafe { *data.add(i as usize) });
        i = i.wrapping_add(1);
    }
    low
}

/// The high half: the running low half, folded again.
#[no_mangle]
#[inline(never)]
pub extern "C" fn high_half(data: *const u8, len: u64) -> u64 {
    let mut low: u64 = 0;
    let mut high: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: `i < len`.
        low = fold(low, unsafe { *data.add(i as usize) });
        high = (high.wrapping_add(low)) % MODULUS;
        i = i.wrapping_add(1);
    }
    high
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn checksum(data: *const u8, len: u64) -> u64 {
    high_half(data, len).wrapping_mul(256).wrapping_add(low_half(data, len))
}
'''

CHECKSUM_TESTS = [
    ("test_low_abc", "abc", "low_half(p, LEN)"),
    ("test_low_empty", "", "low_half(p, LEN)"),
    ("test_high_abc", "high", "high_half(p, LEN)"),
    ("test_high_empty", "", "high_half(p, LEN)"),
    ("test_checksum_abc", "abc", "checksum(p, LEN)"),
    ("test_low_wraps", "\xff\xff\xff", "low_half(p, LEN)"),
    ("test_fold_wraps", "", "fold(250, 10)"),
]

DOMAINS = {
    "scanner": (SCANNER, SCANNER_TESTS),
    "runlength": (RUNLENGTH, RUNLENGTH_TESTS),
    "brackets": (BRACKETS, BRACKETS_TESTS),
    "checksum": (CHECKSUM, CHECKSUM_TESTS),
}


def render_tests(domain, expected):
    """The `test_*` functions, as source.

    One shape for every domain: build the input on the stack, write what you
    expected through the out-parameter, return what you got. Both numbers reach
    the observation, because `{"expected": 30, "actual": 12}` is something an
    agent can act on and `false` is not.

    `expected` is *measured*, not written here. The first draft asserted the
    values by hand and the verifier threw out nine tasks whose "fixed" source
    failed its own suite — the arithmetic was mine and it was wrong. The
    expectations now come from running the pristine crate, which is also the
    right definition for a mutation benchmark: the task is to restore the
    intended behaviour, and pristine is what "intended" means.
    """
    _, tests = DOMAINS[domain]
    out = ["\n// The suite. Ground truth for this task; the agent may read it."]
    for name, text, call in tests:
        raw = text.encode("latin-1")
        writes = "\n".join(
            "        *p.add(%d) = %d;" % (i, b) for i, b in enumerate(raw)
        )
        out.append(
            textwrap.dedent(
                '''
                #[no_mangle]
                pub extern "C" fn {name}(expected: *mut u64) -> u64 {{
                    let mut buf: [u64; 8] = [0; 8];
                    let p = buf.as_mut_ptr() as *mut u8;
                    const LEN: u64 = {length};
                    // SAFETY: sixty-four bytes of this frame and the writes stay
                    // inside them; `expected` is a writable `u64` the caller owns.
                    unsafe {{
                {writes}
                        *expected = {value};
                    }}
                    let p = p as *const u8;
                    let answer = {call};
                    let _ = &mut buf;
                    answer
                }}'''
            ).format(
                name=name,
                writes=writes,
                length=len(raw),
                value=expected.get(name, 0),
                call=call,
            )
        )
    return "\n".join(out) + "\n"


def test_names(domain):
    return [name for name, _, _ in DOMAINS[domain][1]]


# --------------------------------------------------------------------------
# The defects.
#
# `(name, family, domain, target, [(anchor, replacement), ...])`
#
# Each is a bug of a shape that happens for real: a missing end-of-input flush,
# a bound off by one, two halves of a pair swapped, a guard deleted. `target` is
# the function an agent would edit, recorded so the harness can report whether
# the agent found the right one and not only the right answer.
#
# Nothing here is trusted. `generate.py` compiles and runs every task twice and
# drops any defect the suite does not catch — a seeded bug the tests miss is not
# a task, it is a hole in the suite.
# --------------------------------------------------------------------------

FLUSH = """    if in_number == 1 {
        total = total.wrapping_add(acc);
    }
    total"""

COUNT_FLUSH = """    if in_number == 1 {
        count = count.wrapping_add(1);
    }
    count"""

DEFECTS = [
    ("missing_flush", "state-machine", "scanner", "scan",
     [(FLUSH, "    total")]),
    ("count_missing_flush", "state-machine", "scanner", "count_numbers",
     [(COUNT_FLUSH, "    count")]),
    ("loop_stops_early", "off-by-one", "scanner", "scan",
     [("    while i < len {\n        // SAFETY: the caller passes",
       "    while i + 1 < len {\n        // SAFETY: the caller passes")]),
    ("wrong_radix", "local-bug", "scanner", "accumulate",
     [("acc.wrapping_mul(10)", "acc.wrapping_mul(8)")]),
    ("space_is_a_digit", "local-bug", "scanner", "classify_byte",
     [("    } else if b == b' ' || b == b',' {\n        CLASS_SEP",
       "    } else if b == b',' {\n        CLASS_SEP")]),
    ("accumulator_not_reset", "state-machine", "scanner", "scan",
     [("            total = total.wrapping_add(acc);\n            acc = 0;\n            in_number = 0;",
       "            total = total.wrapping_add(acc);\n            in_number = 0;")]),
    ("scale_off_by_one", "local-bug", "scanner", "total_scaled",
     [("scan(data, len).wrapping_mul(scale)",
       "scan(data, len).wrapping_mul(scale.wrapping_add(1))")]),
    ("digit_value_offset", "local-bug", "scanner", "accumulate",
     [("(b - b'0') as u64", "(b - b'0' + 1) as u64")]),

    ("pair_swapped", "local-bug", "runlength", "decoded_at",
     [("        let count = unsafe { *data.add(i as usize) } as u64;\n        let value = unsafe { *data.add((i + 1) as usize) } as u64;",
       "        let value = unsafe { *data.add(i as usize) } as u64;\n        let count = unsafe { *data.add((i + 1) as usize) } as u64;")]),
    ("index_bound_inclusive", "off-by-one", "runlength", "decoded_at",
     [("if index < seen.wrapping_add(count) {", "if index <= seen.wrapping_add(count) {")]),
    ("len_counts_pairs", "local-bug", "runlength", "decoded_len",
     [("        total = total.wrapping_add(count);", "        total = total.wrapping_add(1);")]),
    ("stride_of_one", "off-by-one", "runlength", "decoded_len",
     [("        total = total.wrapping_add(count);\n        i = i.wrapping_add(2);",
       "        total = total.wrapping_add(count);\n        i = i.wrapping_add(1);")]),
    ("sentinel_is_zero", "local-bug", "runlength", "decoded_at",
     [("    SENTINEL\n}", "    0\n}")]),
    ("sum_stops_one_early", "off-by-one", "runlength", "decoded_sum",
     [("while i < decoded_len(data, len) && i < 4096 {",
       "while i + 1 < decoded_len(data, len) && i < 4096 {")]),

    ("max_before_increment", "off-by-one", "brackets", "max_depth",
     [("            depth = depth.wrapping_add(1);\n            if depth > best {\n                best = depth;\n            }",
       "            if depth > best {\n                best = depth;\n            }\n            depth = depth.wrapping_add(1);")]),
    ("no_underflow_guard", "state-machine", "brackets", "max_depth",
     [("} else if is_close(b) == 1 && depth > 0 {", "} else if is_close(b) == 1 {")]),
    ("unopened_undetected", "state-machine", "brackets", "balance",
     [("            if depth == 0 {\n                return UNOPENED;\n            }\n            depth = depth.wrapping_sub(1);",
       "            depth = depth.wrapping_sub(1);")]),
    ("close_also_opens", "local-bug", "brackets", "is_close",
     [("    if b == b')' || b == b']' || b == b'}' { 1 } else { 0 }",
       "    if b == b')' || b == b']' || b == b'}' || b == b'(' { 1 } else { 0 }")]),
    ("balanced_and_unclosed_swapped", "local-bug", "brackets", "balance",
     [("    if depth == 0 { BALANCED } else { UNCLOSED }",
       "    if depth == 0 { UNCLOSED } else { BALANCED }")]),

    ("modulus_off_by_one", "local-bug", "checksum", "fold",
     [("pub const MODULUS: u64 = 255;", "pub const MODULUS: u64 = 256;")]),
    ("halves_swapped", "local-bug", "checksum", "checksum",
     [("high_half(data, len).wrapping_mul(256).wrapping_add(low_half(data, len))",
       "low_half(data, len).wrapping_mul(256).wrapping_add(high_half(data, len))")]),
    ("high_before_low", "state-machine", "checksum", "high_half",
     [("        low = fold(low, unsafe { *data.add(i as usize) });\n        high = (high.wrapping_add(low)) % MODULUS;",
       "        high = (high.wrapping_add(low)) % MODULUS;\n        low = fold(low, unsafe { *data.add(i as usize) });")]),
    ("shift_is_modulus", "local-bug", "checksum", "checksum",
     [("high_half(data, len).wrapping_mul(256)", "high_half(data, len).wrapping_mul(255)")]),
    ("fold_forgets_modulus", "local-bug", "checksum", "fold",
     [("    (low.wrapping_add(b as u64)) % MODULUS", "    low.wrapping_add(b as u64)")]),
]

# Two defects in one domain, so that fixing one function is not enough. The M1
# gate's `scan` + `count_numbers` was exactly this shape, and it was the part of
# that transcript with the most room to go wrong.
PAIRS = [
    ("scanner", "missing_flush", "count_missing_flush"),
    ("scanner", "wrong_radix", "scale_off_by_one"),
    ("scanner", "space_is_a_digit", "missing_flush"),
    ("scanner", "digit_value_offset", "count_missing_flush"),
    ("runlength", "pair_swapped", "len_counts_pairs"),
    ("runlength", "sentinel_is_zero", "index_bound_inclusive"),
    ("runlength", "stride_of_one", "pair_swapped"),
    ("brackets", "no_underflow_guard", "unopened_undetected"),
    ("brackets", "close_also_opens", "max_before_increment"),
    ("brackets", "balanced_and_unclosed_swapped", "no_underflow_guard"),
    ("checksum", "modulus_off_by_one", "halves_swapped"),
    ("checksum", "high_before_low", "shift_is_modulus"),
    ("checksum", "fold_forgets_modulus", "halves_swapped"),
]

# Defects whose fix cannot go DIRECT, because the function carrying them is
# `#[inline]`: its body may already be embedded in a downstream crate, so
# replacing this copy would leave the old one running. §42's rule then applies
# and the session needs a rebase.
#
# Not decoration. An agent that only ever meets DIRECT edits never learns to
# escalate, and the whole M6 routing question is about when to.
FALLBACK_TARGETS = [
    ("scanner", "wrong_radix"),
    ("scanner", "digit_value_offset"),
    ("scanner", "space_is_a_digit"),
    ("runlength", "len_counts_pairs"),
    ("runlength", "sentinel_is_zero"),
    ("brackets", "close_also_opens"),
    ("checksum", "modulus_off_by_one"),
    ("checksum", "fold_forgets_modulus"),
]

# Behaviour a test demands and no function provides: the body is a stub, and the
# agent has to write an implementation rather than repair one.
FEATURES = [
    ("scanner", "scan", "the scanner is a stub"),
    ("runlength", "decoded_len", "the decoded length is a stub"),
    ("brackets", "max_depth", "depth tracking is a stub"),
    ("checksum", "high_half", "the high half is a stub"),
]
