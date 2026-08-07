//! The M1 gate fixture: a small state machine with a real bug in it.
//!
//! What it has to be
//! -----------------
//!
//! The gate is "a human can fix this using only the agent API", so the crate
//! has to contain a bug of the kind that is actually annoying — one where the
//! symptom is a wrong number and the cause is three call frames away, so that
//! finding it means asking the program questions rather than reading it once.
//!
//! It also has to stay inside the artifact class §43 names. Everything here is
//! arithmetic over bytes reached through a raw pointer: no indexing, so no
//! bounds check, so no panic location; no string literals; no formatting. Those
//! are constant data, a live patch has nowhere to put constant data yet, and a
//! fixture that needed it would be testing the refusal rather than the API.
//!
//! The bug
//! -------
//!
//! `scan` accumulates decimal digits and adds a number to the running total
//! when it meets a separator. Input that *ends* on a digit never meets a final
//! separator, so the last number is dropped. `parse_pair` then reports one
//! number where two were written.
//!
//! It is a flush-at-end-of-input bug: the most common shape of state-machine
//! defect there is, and one that every test whose input happens to end in a
//! separator will pass.

#![allow(dead_code)]

/// Byte classes, so the state machine has something to be a machine over.
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

/// One byte of a buffer, by integer offset rather than `ptr::add`.
///
/// `p.add(i)` is the obvious spelling and it does not work here. It carries a
/// UB precondition check, and at `-Copt-level=0` nothing folds that away —
/// neither `-Cdebug-assertions=off` nor `-Zub-checks=no` — so the call survives
/// and it takes a `core::panic::Location`. A `Location` is constant data, and
/// §43's artifact class has nowhere to put constant data. Integer offset
/// arithmetic compiles to a single `ldrb` and carries nothing.
#[inline(never)]
unsafe fn byte_at(data: *const u8, i: u64) -> u8 {
    // SAFETY: the caller guarantees `i` is within the buffer.
    unsafe { *((data as usize).wrapping_add(i as usize) as *const u8) }
}

/// One digit folded into an accumulator.
///
/// `wrapping_sub`, not `-`. A plain subtraction carries an overflow check whose
/// failure path needs a panic location, a panic location is constant data, and
/// the patch then references a `.Ldata0` that nothing can resolve. Which is the
/// artifact rule doing its job: the crate compiled and linked perfectly well,
/// and it was the *live* path that could not carry it.
#[no_mangle]
#[inline(never)]
pub extern "C" fn accumulate(acc: u64, b: u8) -> u64 {
    acc.wrapping_mul(10).wrapping_add(b.wrapping_sub(b'0') as u64)
}

/// Sum every decimal number in `data`.
///
/// The buggy one. Walked by offset rather than indexed, because indexing emits
/// a bounds check whose failure path needs a panic location, and a panic
/// location is constant data.
#[no_mangle]
#[inline(never)]
pub extern "C" fn scan(data: *const u8, len: u64) -> u64 {
    let mut total: u64 = 0;
    let mut acc: u64 = 0;
    let mut in_number: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: the caller passes a pointer to at least `len` bytes.
        let b = unsafe { byte_at(data, i) };
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

/// How many numbers `scan` would have added. Shares `scan`'s state machine and
/// therefore, deliberately, `scan`'s bug — so that fixing one and not the other
/// is a mistake the tests can catch.
#[no_mangle]
#[inline(never)]
pub extern "C" fn count_numbers(data: *const u8, len: u64) -> u64 {
    let mut count: u64 = 0;
    let mut in_number: u64 = 0;
    let mut i: u64 = 0;
    while i < len {
        // SAFETY: as above.
        let b = unsafe { byte_at(data, i) };
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

/// The entry point a caller uses: sum, scaled.
#[no_mangle]
#[inline(never)]
pub extern "C" fn total_scaled(data: *const u8, len: u64, scale: u64) -> u64 {
    scan(data, len).wrapping_mul(scale)
}

// ---------------------------------------------------------------------------
// The suite.
//
// `extern "C" fn(*mut u64) -> u64`: write what you expected, return what you
// got. Two numbers rather than a pass/fail code, because the observation an
// agent acts on is `{"expected": 30, "actual": 20}` and a boolean makes it go
// looking for the difference somewhere this API cannot answer.
//
// Each input is built by writing bytes into a `u64` on the stack. The obvious
// `[b'1', b'2', ...]` does not work and the failure is worth recording: cg_clif
// materialises an array literal as a rodata blob and the patch then carries a
// relocation to `.Ldata0`, which is the constant-data artifact class §43
// refuses. The refusal was correct and the fixture was wrong — §31's lesson,
// for the second time.
// ---------------------------------------------------------------------------

/// "12,18" — ends on a digit, so the final 18 is the one that gets lost.
#[no_mangle]
pub extern "C" fn test_trailing_number(expected: *mut u64) -> u64 {
    let mut buf: u64 = 0;
    let p = (&mut buf) as *mut u64 as *mut u8;
    // SAFETY: `buf` is eight bytes of this frame and the writes stay inside it;
    // `expected` is a writable `u64` the caller owns.
    unsafe {
        *p = b'1';
        *((p as usize + 1) as *mut u8) = b'2';
        *((p as usize + 2) as *mut u8) = b',';
        *((p as usize + 3) as *mut u8) = b'1';
        *((p as usize + 4) as *mut u8) = b'8';
        *expected = 30;
    }
    scan(p as *const u8, 5)
}

/// "12,18," — ends on a separator, so the buggy machine gets this one right.
/// It is here to be the control: a fix that breaks it is not a fix.
#[no_mangle]
pub extern "C" fn test_trailing_separator(expected: *mut u64) -> u64 {
    let mut buf: u64 = 0;
    let p = (&mut buf) as *mut u64 as *mut u8;
    // SAFETY: as above.
    unsafe {
        *p = b'1';
        *((p as usize + 1) as *mut u8) = b'2';
        *((p as usize + 2) as *mut u8) = b',';
        *((p as usize + 3) as *mut u8) = b'1';
        *((p as usize + 4) as *mut u8) = b'8';
        *((p as usize + 5) as *mut u8) = b',';
        *expected = 30;
    }
    scan(p as *const u8, 6)
}

/// The empty input still sums to nothing.
#[no_mangle]
pub extern "C" fn test_empty(expected: *mut u64) -> u64 {
    let mut buf: u64 = 0;
    let p = (&mut buf) as *mut u64 as *mut u8;
    // SAFETY: as above.
    unsafe {
        *p = b'0';
        *expected = 0;
    }
    scan(p as *const u8, 0)
}

/// The scaled entry point, through the same broken scan.
#[no_mangle]
pub extern "C" fn test_scaled(expected: *mut u64) -> u64 {
    let mut buf: u64 = 0;
    let p = (&mut buf) as *mut u64 as *mut u8;
    // SAFETY: as above.
    unsafe {
        *p = b'7';
        *((p as usize + 1) as *mut u8) = b',';
        *((p as usize + 2) as *mut u8) = b'3';
        *expected = 30;
    }
    total_scaled(p as *const u8, 3, 3)
}

/// Counting shares the machine and so shares the bug.
#[no_mangle]
pub extern "C" fn test_count(expected: *mut u64) -> u64 {
    let mut buf: u64 = 0;
    let p = (&mut buf) as *mut u64 as *mut u8;
    // SAFETY: as above.
    unsafe {
        *p = b'1';
        *((p as usize + 1) as *mut u8) = b'2';
        *((p as usize + 2) as *mut u8) = b',';
        *((p as usize + 3) as *mut u8) = b'1';
        *((p as usize + 4) as *mut u8) = b'8';
        *expected = 2;
    }
    count_numbers(p as *const u8, 5)
}

/// A test that reaches neither `scan` nor `count_numbers`.
///
/// The discriminating control for `run_affected`. Without it, "the selected
/// tests are the affected ones" is a claim with nothing to fail: a selector
/// that returned every test would look identical.
#[no_mangle]
pub extern "C" fn test_classify_only(expected: *mut u64) -> u64 {
    // SAFETY: a writable `u64` the caller owns.
    unsafe { *expected = 3 };
    (classify_byte(b'4') + classify_byte(b',') + classify_byte(b'x')) as u64
}
