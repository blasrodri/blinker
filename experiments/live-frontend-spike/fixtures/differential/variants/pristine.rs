//! The runtime-differential fixture (§30).
//!
//! Unlike the latency fixtures, this crate exists to be *observed*, not timed.
//! It is deliberately self-contained: the clean path builds it as a `cdylib`
//! with the ordinary LLVM backend and a real linker, so the two sides of the
//! comparison share no code at all below the source text.
//!
//! It has a gate, and the gate is the point
//! ----------------------------------------
//!
//! A differential that only ever calls the patched function cannot detect the
//! errors the classifier exists to prevent, because every one of those errors
//! is about code that was *not* patched — a caller that inlined an old body, a
//! constant folded into a neighbour, a layout another function still assumes.
//!
//! So the live path loads this crate as a real base image, publishes the patch
//! over it, and drives probes through `diff_entry`, which lives in the base
//! image and reaches the patch through `DIFF_GATE`. Everything the entry point
//! touches other than the patch closure is the *old* compiled code, which is
//! exactly the situation a live patch creates and exactly what has to be
//! checked against a clean rebuild.
#![allow(dead_code)]

/// A static that exists in every revision, so that reading one is an ordinary
/// edit rather than an introduction. S0c distinguishes those, and §30's
/// `read_static` mutation is the runtime half of that distinction.
#[no_mangle]
pub static DIFF_BASE: u64 = 1_000;

#[derive(Clone, Copy)]
pub struct Reading {
    pub value: u64,
    pub scale: u32,
}

impl Reading {
    #[inline(never)]
    pub fn total(&self) -> u64 {
        self.value.wrapping_mul(self.scale as u64)
    }
}

/// A second patchable function, reachable from the hot root and so inside its
/// closure. `extern "C"` and `#[no_mangle]` so that both a probe and a clean
/// rebuild can call it by name.
///
/// The concurrency scenarios need more than one gate. A scope that captures a
/// generation and then calls a single function through it proves very little:
/// the pointer was read once and could not have changed. Two functions, read
/// from the captured generation on either side of a barrier, is what makes
/// "the scope holds its generation" a claim with something to fail.
#[no_mangle]
#[inline(never)]
pub extern "C" fn diff_second(x: u64) -> u64 {
    x.wrapping_mul(3).wrapping_add(1)
}

#[inline(never)]
fn blend(x: u64) -> u64 {
    diff_second(x).wrapping_add(7)
}

/// The hot root. `extern "C"` so that both paths call it through one signature
/// that is written down rather than assumed — §25's lesson.
#[no_mangle]
#[inline(never)]
pub extern "C" fn diff_root(value: u64, scale: u32, out: *mut u64) -> u64 {
    let reading = Reading { value, scale };
    let total = reading.total();
    let mixed = blend(total);
    // The memory output: a differential that compared return values alone
    // would miss a patch that got the write wrong.
    unsafe { *out = mixed ^ 0xAAAA };
    mixed
}

pub type Root = extern "C" fn(u64, u32, *mut u64) -> u64;

/// Where a published patch is installed. Null in a clean build, which is why
/// the clean path needs no cooperation from the harness beyond loading it.
#[no_mangle]
pub static mut DIFF_GATE: Option<Root> = None;

/// The probe entry point, compiled once into the base image and never patched.
#[no_mangle]
pub extern "C" fn diff_entry(value: u64, scale: u32, out: *mut u64) -> u64 {
    let through = unsafe { DIFF_GATE }.unwrap_or(diff_root);
    through(value, scale, out)
}
