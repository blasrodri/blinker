//! The consumer of cg_clif's live output sink — machine code with no object
//! file in the path.
//!
//! §27 made the codegen universe exactly Path D's closure by overriding a
//! query, which needed no backend change at all. This needs one, and the reason
//! is structural rather than a matter of taste: a rustc backend's contract is
//! "compile this crate into modules that a linker consumes", and there is no
//! query, flag or hook that means "give me the bytes". `CodegenBackend`
//! exposes `codegen_crate` and `join_codegen`; the object file is produced
//! between them, inside cg_clif's AOT driver.
//!
//! So the seam is inside cg_clif, after `Context::compile` and before its
//! object sink, and the patch is an *output sink* — roughly a hundred lines,
//! none of it in codegen. With no sink installed the backend is an unmodified
//! drop-in.
//!
//! What this changes about the measurement
//! ---------------------------------------
//!
//! Two things, and the second matters more than the first.
//!
//! **No subtraction.** §33 had to establish the backend's cost by subtracting
//! an empty-universe run from a closure-only run, and on the largest fixture
//! that produced −0.23 ms — a subtraction reaching its noise floor. The timers
//! are now inside the backend, around the work itself.
//!
//! **Publication happens before the compiler finishes.** The artifact is
//! delivered from inside codegen, so the patch is live while rustc still has
//! its object file to write and its incremental session to finalize. That
//! splits one number into two that mean different things: how long until the
//! edit is active, and how long until the compiler is ready for the next one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::arena::Arena;
use crate::generation::Runtime;

/// Mirrors `live_sink::LiveReloc` in the backend.
#[repr(C)]
struct LiveReloc {
    offset: u32,
    kind: u32,
    addend: i64,
    name: *const u8,
    name_len: usize,
}

/// Mirrors `live_sink::LiveFunction`.
#[repr(C)]
struct LiveFunction {
    name: *const u8,
    name_len: usize,
    code: *const u8,
    code_len: usize,
    align: u32,
    relocs: *const LiveReloc,
    reloc_count: usize,
}

/// Mirrors `live_sink::LiveTimings`. Nanoseconds.
#[repr(C)]
#[derive(Default, Clone, Copy, Debug, serde::Serialize)]
pub struct Timings {
    pub mir_to_clif_ns: u64,
    pub cranelift_ns: u64,
    pub extract_ns: u64,
    pub object_define_ns: u64,
    pub object_write_ns: u64,
    pub functions: u32,
    pub code_bytes: u32,
    pub clif_clone_ns: u64,
}

/// What one delivery produced.
#[derive(Default)]
pub struct Outcome {
    /// The published root, as an address. A `*const u8` would make `Outcome`
    /// non-`Send` and so unable to live in the static the callback writes to;
    /// the pointer is reconstructed by the one caller that dereferences it,
    /// under the same lifetime argument as everything else in the arena.
    pub entry: usize,
    /// Edit → the published code is callable. The product's latency.
    pub active_ms: f64,
    /// Reserve, copy, relocate, i-cache, slots, swap.
    pub publish_ms: f64,
    pub code_bytes: usize,
    pub relocations: usize,
    pub timings: Timings,
    pub error: Option<String>,
}

/// The current revision's publication target.
///
/// A callback crossing an `extern "C"` boundary cannot capture, so what it
/// needs is parked beside it. Revisions are strictly sequential in this
/// harness — one compiler session at a time — and `begin`/`finish` bracket
/// exactly one of them.
static ARENA: AtomicUsize = AtomicUsize::new(0);
static RUNTIME: AtomicUsize = AtomicUsize::new(0);
static IMAGE: AtomicUsize = AtomicUsize::new(0);
static STARTED: Mutex<Option<Instant>> = Mutex::new(None);
/// The hot root's name, so the artifact can be ordered root-first.
static ROOT: Mutex<String> = Mutex::new(String::new());
static OUTCOME: Mutex<Option<Outcome>> = Mutex::new(None);

/// Deliberate defects for §31's controls.
///
/// The sink builds its artifact inside a callback, so the controls cannot
/// reach in and damage it the way they do on the object path. They are applied
/// here instead — same defects, same required outcomes, on the artifact path
/// that is actually being used.
static SABOTAGE: Mutex<crate::live::PatchOptions> = Mutex::new(crate::live::PatchOptions {
    omit_closure_member: false,
    corrupt_relocation: false,
    ignore_classifier: false,
});

pub fn set_sabotage(options: crate::live::PatchOptions) {
    *SABOTAGE.lock().expect("not poisoned") = options;
}

/// Whether the sink was installed. `None` until `install` is called.
static INSTALLED: Mutex<Option<Result<(), String>>> = Mutex::new(None);

/// Load the patched backend and install the callback.
///
/// The harness `dlopen`s the backend itself rather than waiting for rustc to
/// load it. It has to: the callback must be in place *before* codegen runs,
/// and rustc loads the backend during the session. `dlopen` on the same path
/// yields the same image, so the statics the callback reads are the same ones
/// the backend writes.
pub fn install(backend: &std::path::Path) -> Result<(), String> {
    let mut installed = INSTALLED.lock().expect("not poisoned");
    if let Some(result) = installed.as_ref() {
        return result.clone();
    }
    let outcome = (|| {
        let path = std::ffi::CString::new(backend.as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())?;
        // SAFETY: a path the caller supplied and this process can read.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            return Err(format!("cannot load the backend at {}", backend.display()));
        }
        let name = std::ffi::CString::new("blinker_live_set_sink").expect("literal");
        // SAFETY: an open handle and a null-terminated name.
        let symbol = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if symbol.is_null() {
            return Err(
                "the backend has no blinker_live_set_sink — it is not the patched one".into(),
            );
        }
        // SAFETY: the backend declares this signature in `live_sink.rs`, and
        // the two declarations are kept beside each other deliberately.
        let set: extern "C" fn(Option<extern "C" fn(*const LiveFunction, usize, *const Timings)>) =
            unsafe { std::mem::transmute(symbol) };
        set(Some(deliver));
        Ok(())
    })();
    *installed = Some(outcome.clone());
    outcome
}

/// Bracket one revision. `started` is when the edit was written, not when the
/// compiler session began: the product's clock starts at the edit.
pub fn begin(
    arena: &Arena,
    runtime: &Runtime,
    image: Option<*mut libc::c_void>,
    started: Instant,
    root: &str,
) {
    *ROOT.lock().expect("not poisoned") = root.to_string();
    ARENA.store(arena as *const Arena as usize, Ordering::Release);
    RUNTIME.store(runtime as *const Runtime as usize, Ordering::Release);
    IMAGE.store(image.map_or(0, |i| i as usize), Ordering::Release);
    *STARTED.lock().expect("not poisoned") = Some(started);
    *OUTCOME.lock().expect("not poisoned") = None;
}

/// End the bracket and take what the callback recorded.
pub fn finish() -> Option<Outcome> {
    ARENA.store(0, Ordering::Release);
    RUNTIME.store(0, Ordering::Release);
    IMAGE.store(0, Ordering::Release);
    let mut outcome = OUTCOME.lock().expect("not poisoned").take();
    if let Some(outcome) = outcome.as_mut() {
        // The object path's cost, read back from the backend. It is recorded
        // after the callback returns, so it cannot travel with the artifact.
        let (mut define, mut write) = (0u64, 0u64);
        let name = std::ffi::CString::new("blinker_live_object_cost").expect("literal");
        // SAFETY: the backend is loaded; a null symbol is handled.
        let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        if !symbol.is_null() {
            // SAFETY: the signature is declared beside the definition.
            let cost: extern "C" fn(*mut u64, *mut u64) = unsafe { std::mem::transmute(symbol) };
            cost(&mut define, &mut write);
        }
        outcome.timings.object_define_ns = define;
        outcome.timings.object_write_ns = write;
    }
    outcome
}

/// Cranelift's relocation kinds, as the backend numbered them.
///
/// Returns `(kind, pc_relative)` for the publisher, or `None` for a kind
/// nobody has considered — which refuses the patch rather than guessing. The
/// backend maps its own enum to these numbers for the same reason: an
/// unrecognised kind has to arrive *as* unrecognised.
fn relocation(kind: u32) -> Option<(blinker_macho::Arm64RelocationKind, bool)> {
    use blinker_macho::Arm64RelocationKind as Kind;
    Some(match kind {
        1 => (Kind::Unsigned, false),
        2 => (Kind::Branch26, true),
        3 => (Kind::GotLoadPage21, true),
        4 => (Kind::GotLoadPageOff12, false),
        5 => (Kind::Page21, true),
        6 => (Kind::PageOff12, false),
        _ => return None,
    })
}

/// The sink. Called from inside cg_clif, before it writes anything.
extern "C" fn deliver(functions: *const LiveFunction, count: usize, timings: *const Timings) {
    let arena = ARENA.load(Ordering::Acquire);
    let runtime = RUNTIME.load(Ordering::Acquire);
    if arena == 0 || runtime == 0 {
        // No revision is in progress. An ordinary compilation that happens to
        // load this backend is not a patch and must not be treated as one.
        return;
    }
    // SAFETY: `begin` stored pointers to values that outlive the session, and
    // `finish` clears them before those values are dropped.
    let arena: &Arena = unsafe { &*(arena as *const Arena) };
    let runtime: &Runtime = unsafe { &*(runtime as *const Runtime) };
    let image = match IMAGE.load(Ordering::Acquire) {
        0 => None,
        address => Some(address as *mut libc::c_void),
    };
    // SAFETY: the backend passes a valid array of `count` entries.
    let functions = unsafe { std::slice::from_raw_parts(functions, count) };
    // SAFETY: the backend passes one `Timings`.
    let timings = unsafe { *timings };

    let mut outcome = Outcome {
        timings,
        ..Default::default()
    };
    let mut lifted = Vec::with_capacity(functions.len());
    for function in functions {
        // SAFETY: pointer and length come from an owned `String`/`Vec` in the
        // backend that outlives this call.
        let name = unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                function.name,
                function.name_len,
            ))
        };
        let code =
            unsafe { std::slice::from_raw_parts(function.code, function.code_len) }.to_vec();
        let relocations = unsafe {
            std::slice::from_raw_parts(function.relocs, function.reloc_count)
        };
        let mut collected = Vec::with_capacity(relocations.len());
        for relocation_entry in relocations {
            let Some((kind, pc_relative)) = relocation(relocation_entry.kind) else {
                outcome.error = Some(format!(
                    "{name} has a relocation of a kind this class does not support"
                ));
                *OUTCOME.lock().expect("not poisoned") = Some(outcome);
                return;
            };
            if relocation_entry.name.is_null() {
                outcome.error =
                    Some(format!("{name} has a relocation the backend could not name"));
                *OUTCOME.lock().expect("not poisoned") = Some(outcome);
                return;
            }
            let target = unsafe {
                std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                    relocation_entry.name,
                    relocation_entry.name_len,
                ))
            };
            collected.push((
                relocation_entry.offset as u64,
                target.to_string(),
                kind,
                pc_relative,
                relocation_entry.addend,
            ));
        }
        lifted.push(crate::live::Lifted {
            name: name.to_string(),
            code,
            relocations: collected,
        });
    }

    // Slot 0 must be the hot root, and the sink does not deliver it first.
    // cg_clif emits a codegen unit's items in deterministic order — which is
    // by symbol name, not by Path D's order — so which function landed in slot
    // 0 depended on how the crate's names happened to mangle. For two fixtures
    // the root sorted first and everything worked; for `diff_fixture` the root
    // is `#[no_mangle]` and sorted *after* `blend`, so the published entry
    // point was `blend`. It returned 7 for probe (0, 0), which is a perfectly
    // plausible number and the wrong function.
    let root = ROOT.lock().expect("not poisoned").clone();
    match lifted.iter().position(|f| crate::live::symbol_matches(&f.name, &root)) {
        Some(index) => lifted.swap(0, index),
        None => {
            outcome.error = Some(format!("the sink delivered no function named {root}"));
            *OUTCOME.lock().expect("not poisoned") = Some(outcome);
            return;
        }
    }

    let sabotage = *SABOTAGE.lock().expect("not poisoned");
    if sabotage.omit_closure_member {
        let victim = lifted.iter().skip(1).map(|f| f.name.clone()).find(|name| {
            lifted
                .iter()
                .any(|other| other.name != *name && other.relocations.iter().any(|r| r.1 == *name))
        });
        match victim {
            Some(victim) => lifted.retain(|f| f.name != victim),
            None => {
                outcome.error =
                    Some("control not applicable: no member is called by another".into());
                *OUTCOME.lock().expect("not poisoned") = Some(outcome);
                return;
            }
        }
    }
    if sabotage.corrupt_relocation {
        for function in &mut lifted {
            if let Some(relocation) = function.relocations.first_mut() {
                relocation.1 = "_a_symbol_that_does_not_exist".into();
                break;
            }
        }
    }

    match crate::live::publish(arena, runtime, &lifted, image) {
        Ok((entry, publish_ms, bytes, relocations)) => {
            outcome.entry = entry as usize;
            outcome.publish_ms = publish_ms;
            outcome.code_bytes = bytes;
            outcome.relocations = relocations;
        }
        Err(error) => outcome.error = Some(error),
    }
    // The clock stops here, and only here. Everything the compiler does after
    // this — the object file, the incremental session, the rest of codegen —
    // is work the developer is no longer waiting on.
    if let Some(started) = *STARTED.lock().expect("not poisoned") {
        outcome.active_ms = started.elapsed().as_secs_f64() * 1e3;
    }
    *OUTCOME.lock().expect("not poisoned") = Some(outcome);
}
