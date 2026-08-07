//! The end-to-end path, in one process.
//!
//! ```text
//! source edit
//!   → rustc validation          (a real compiler session)
//!   → DIRECT classifier          (S0c)
//!   → Path D closure             (S0)
//!   → cg_clif machine code       (-Zcodegen-backend=cranelift)
//!   → MAP_JIT arena              (R1)
//!   → the next call returns the new value
//! ```
//!
//! Every stage is the real one. Nothing here is a stand-in except where this
//! comment says so, and there is exactly one such place.
//!
//! Codegen goes through an object file, and that is a measured upper bound
//! -----------------------------------------------------------------------
//!
//! `rustc_codegen_cranelift` is a rustc *backend*: it exposes "compile this
//! crate", not "lower this `Instance`". Reaching in to lower a single instance
//! means forking it, which V2 §9.2 wants eventually and which is not what R1
//! is for. So this asks rustc for `--emit=obj` with the Cranelift backend and
//! then lifts the closure's functions out of the object with blinker's own
//! Mach-O parser.
//!
//! That inflates the number, and by a knowable amount: cg_clif compiles the
//! *whole crate* where the product would lower 4–6 instances. R1 §5 measured
//! the whole-crate backend at 17 ms and R1 §2 measured Cranelift at 0.12 ms
//! for a six-function closure, so the gap between what this reports and what
//! the product would pay is most of that 17 ms. The number below is therefore
//! an upper bound that is known to be loose, which is worth more than an
//! estimate that is not measured at all.
//!
//! The object file is *not* in the eventual design and is not being proposed
//! as one.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::arena::Arena;
use crate::generation::{scope, Runtime};

/// One end-to-end revision, with every stage timed apart.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LiveRecord {
    pub fixture: String,
    pub edit: String,
    /// rustc: expansion, analysis, and the hot root's codegen MIR.
    pub validate_ms: f64,
    /// The three parts of `validate_ms`, kept apart because the closure-only
    /// experiment moved this column and a single number could not say why.
    pub expand_ms: f64,
    pub analysis_ms: f64,
    pub hot_mir_ms: f64,
    /// S0c.
    pub classify_ms: f64,
    /// S0's Path D.
    pub closure_ms: f64,
    /// cg_clif, whole crate — the upper bound described in the module docs.
    pub codegen_ms: f64,
    /// Lifting the closure's functions out of the object.
    pub extract_ms: f64,
    /// R1: reserve, copy, relocate, i-cache, slots, swap.
    pub publish_ms: f64,
    pub total_ms: f64,
    pub verdict: String,
    pub closure_size: usize,
    pub code_bytes: usize,
    pub relocations: usize,
    /// What the published function returned. The point of the exercise.
    pub returned: Option<i64>,
    pub error: Option<String>,
}

/// A function lifted out of a cg_clif object.
struct Lifted {
    name: String,
    code: Vec<u8>,
    /// `(offset within this function, symbol name, kind, pc_relative, addend)`
    relocations: Vec<(u64, String, blinker_macho::Arm64RelocationKind, bool, i64)>,
}

/// Pull the named functions out of an object's `__TEXT,__text`.
///
/// A symbol's extent is the distance to the next symbol in the same section,
/// because Mach-O does not record function sizes. Sorting is therefore not a
/// convenience: without it a function's bytes run to whichever symbol happened
/// to come next in the table.
fn lift(object: &Path, wanted: &[String]) -> Result<Vec<Lifted>, String> {
    let parsed = blinker_macho::parse_object_file(object, blinker_macho::ObjectId(0))
        .map_err(|e| format!("cannot parse {}: {e}", object.display()))?;
    let bytes = std::fs::read(object).map_err(|e| e.to_string())?;

    let text = parsed
        .sections
        .iter()
        .find(|s| s.segment == "__TEXT" && s.name == "__text")
        .ok_or("the object has no __TEXT,__text")?;
    let base = text.file_offset.ok_or("__text occupies no file bytes")?;

    let mut defined: Vec<&blinker_macho::InputSymbol> = parsed
        .symbols
        .iter()
        .filter(|s| s.section == Some(text.id) && s.strength.is_definition())
        .collect();
    defined.sort_by_key(|s| s.value);

    let mut out = Vec::new();
    for name in wanted {
        let Some(position) = defined.iter().position(|s| symbol_matches(&s.name, name)) else {
            continue;
        };
        let symbol = defined[position];
        let start = symbol.value - text.vm_address;
        let end = defined
            .get(position + 1)
            .map(|next| next.value - text.vm_address)
            .unwrap_or(text.size);
        let from = (base + start) as usize;
        let to = (base + end) as usize;
        let code = bytes
            .get(from..to)
            .ok_or_else(|| format!("{name} lies outside the file"))?
            .to_vec();

        // Relocations that fall inside this function, rebased to it.
        let relocations = parsed
            .relocations
            .iter()
            .filter(|r| r.section == text.id && r.offset >= start && r.offset < end)
            .filter_map(|r| {
                let target = match r.target {
                    blinker_macho::RelocationTarget::Symbol(id) => {
                        parsed.symbol(id).map(|s| s.name.clone())?
                    }
                    // Section-relative: a constant pool or jump table. Not in
                    // the first DIRECT class, and refused rather than guessed.
                    blinker_macho::RelocationTarget::Section(_) => return None,
                };
                Some((r.offset - start, target, r.kind, r.pc_relative, r.addend))
            })
            .collect();
        out.push(Lifted {
            name: symbol.name.clone(),
            code,
            relocations,
        });
    }
    Ok(out)
}

/// Mach-O prefixes every symbol with an underscore, and Rust mangles.
fn symbol_matches(symbol: &str, wanted: &str) -> bool {
    let bare = symbol.strip_prefix('_').unwrap_or(symbol);
    bare == wanted || bare.contains(wanted)
}

/// Publish the lifted functions and call the first one.
///
/// Relocations to symbols outside the closure are resolved against the
/// *running process* by `dlsym`, which is what a real base image would supply
/// from its own symbol table. A target that cannot be resolved fails the
/// publication rather than being patched with a guess.
fn publish_and_call(
    arena: &Arena,
    runtime: &Runtime,
    lifted: &[Lifted],
    argument: i64,
) -> Result<(i64, f64, usize, usize), String> {
    use blinker_macho::Arm64RelocationKind as Kind;
    let at = Instant::now();

    let total: usize = lifted.iter().map(|f| f.code.len().next_multiple_of(16)).sum();
    let slab = arena.slab(total).map_err(|e| e.to_string())?;

    let mut offsets = Vec::with_capacity(lifted.len());
    let mut cursor = 0usize;
    for function in lifted {
        cursor = cursor.next_multiple_of(16);
        // SAFETY: this arena's slab, nothing executing it, offset inside it.
        unsafe { arena.write(&slab, cursor, &function.code) };
        offsets.push(cursor);
        cursor += function.code.len();
    }

    let mut applied = 0usize;
    for (index, function) in lifted.iter().enumerate() {
        for (offset, target, kind, pc_relative, addend) in &function.relocations {
            let site = offsets[index] + *offset as usize;
            // Inside the closure first, then the process.
            let address = match lifted.iter().position(|f| f.name == *target) {
                // SAFETY: an offset inside the slab.
                Some(member) => (unsafe { slab.ptr.add(offsets[member]) }) as u64,
                None => resolve(target)
                    .ok_or_else(|| format!("cannot resolve {target}"))?,
            };
            let value = (address as i64).wrapping_add(*addend);
            match (kind, pc_relative) {
                (Kind::Branch26, true) => {
                    // SAFETY: the site holds an instruction this object wrote.
                    let here = (unsafe { slab.ptr.add(site) }) as i64;
                    let displacement = value - here;
                    let words = displacement >> 2;
                    if displacement % 4 != 0 || !(-(1 << 25)..(1 << 25)).contains(&words) {
                        return Err(format!(
                            "{target} is {displacement} bytes away, out of range for a bl"
                        ));
                    }
                    let word = unsafe {
                        std::ptr::read_unaligned(slab.ptr.add(site).cast::<u32>())
                    };
                    let patched = (word & !0x03ff_ffff) | ((words as u32) & 0x03ff_ffff);
                    unsafe { arena.write(&slab, site, &patched.to_le_bytes()) };
                }
                (Kind::Unsigned, false) => {
                    unsafe { arena.write(&slab, site, &(value as u64).to_le_bytes()) };
                }
                other => return Err(format!("unhandled relocation {other:?}")),
            }
            applied += 1;
        }
    }

    // SAFETY: the slab holds complete, relocated code.
    unsafe { arena.publish(&slab) };
    // SAFETY: every offset lies inside the slab.
    let slots: Vec<*const u8> = offsets
        .iter()
        .map(|o| unsafe { slab.ptr.add(*o) } as *const u8)
        .collect();
    let candidate = runtime.candidate(slots, vec![slab]);
    runtime.publish(candidate);
    let publish_ms = at.elapsed().as_secs_f64() * 1e3;

    let returned = scope(runtime, |generation| {
        let pointer = generation.implementation(0).expect("slot 0");
        // `spike_hot_root(reading: SpikeReading) -> u64`, where `SpikeReading`
        // is `{ value: u64, scale: u32 }`. A 16-byte aggregate goes in two
        // registers under AAPCS, so the C signature is `(u64, u32) -> u64`.
        //
        // Calling it as `fn(i64) -> i64` — which the first version did —
        // leaves `scale` as whatever was in `x1`, and the "result" changes
        // from run to run. It looked like a working demo and was reading
        // uninitialised register state.
        //
        // SAFETY: the fixture is generated, so the signature is known rather
        // than assumed.
        let f: extern "C" fn(u64, u32) -> u64 = unsafe { std::mem::transmute(pointer) };
        f(argument as u64, 5) as i64
    });
    Ok((returned, publish_ms, total, applied))
}

/// A symbol in the running process, which stands in for the base image.
fn resolve(name: &str) -> Option<u64> {
    let bare = name.strip_prefix('_').unwrap_or(name);
    let c = std::ffi::CString::new(bare).ok()?;
    // SAFETY: a null-terminated name; `dlsym` returns null when it fails.
    let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!address.is_null()).then(|| address as u64)
}

/// One complete revision: edit, validate, classify, generate, publish, call.
#[allow(clippy::too_many_arguments)]
pub fn revision(
    arena: &Arena,
    runtime: &Runtime,
    fixture: &crate::Fixture,
    target_file: &Path,
    incremental: &Path,
    hot: &str,
    edited: &str,
    before: Option<&crate::classify::Contract>,
    object: &PathBuf,
    closure_only: bool,
) -> LiveRecord {
    let mut record = LiveRecord {
        fixture: fixture.name.clone(),
        ..Default::default()
    };
    let whole = Instant::now();

    std::fs::write(target_file, edited).expect("write the edit");
    // One session: validation, the hot root's MIR, Path D, the contract, and
    // — because the backend is Cranelift and the emit is an object — the
    // machine code, all from the compiler run the developer was going to pay
    // for anyway.
    let session = crate::run_session_with(
        fixture,
        target_file,
        incremental,
        hot,
        crate::Mode::HotClosure,
        &[
            "-Zcodegen-backend=cranelift".into(),
            "--emit=obj".into(),
            format!("--out-dir={}", object.parent().unwrap_or(object).display()),
        ],
        closure_only,
    );
    record.expand_ms = session.expand_ms;
    record.analysis_ms = session.analysis_ms;
    record.hot_mir_ms = session.hot_mir_ms;
    record.validate_ms = session.expand_ms + session.analysis_ms + session.hot_mir_ms;
    record.closure_ms = session.hot_closure_ms;
    record.classify_ms = session.classify_ms;
    record.closure_size = session.mono_items_examined;
    let session = session;
    // Everything the session cost that was not one of the phases above is the
    // backend: cg_clif lowering the whole crate and writing the object.
    record.codegen_ms = (session.session_ms - record.validate_ms - record.closure_ms
        - record.classify_ms)
        .max(0.0);

    if let Some(error) = session.error {
        record.error = Some(error);
        record.total_ms = whole.elapsed().as_secs_f64() * 1e3;
        return record;
    }
    let Some(after) = session.contract else {
        record.error = Some("no contract".into());
        return record;
    };
    let verdict = match before {
        Some(before) => crate::classify::classify(before, &after),
        None => {
            record.error = Some("no previous revision to compare against".into());
            return record;
        }
    };
    record.verdict = verdict.label();
    if !verdict.is_direct() {
        // A FALLBACK is a correct outcome, not a failure: it means the edit
        // goes down the ordinary rebuild path.
        record.total_ms = whole.elapsed().as_secs_f64() * 1e3;
        return record;
    }

    let at = Instant::now();
    // rustc names its objects itself, and there may be more than one of them:
    // a binary crate gets a separate codegen unit for the allocator shim, and
    // whole-crate codegen of a large crate gets several. So the object is
    // chosen by *what it defines*, not by name, order, or timestamp — the same
    // rule the linker learned in findings 230 and 241, where a name and a
    // position both turned out not to be identities. Selecting the newest file
    // picked the allocator shim for the `small` fixture and reported the hot
    // root missing from a compilation that had in fact produced it.
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(object.parent().unwrap_or(object))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "o"))
        .collect();
    // The whole closure, not just the root. `u64::wrapping_add` is a generic
    // instance compiled into *this* crate's object, not a symbol the process
    // exports, so lifting only the root left a call to it unresolvable — which
    // is the patch-cluster property S0c established, arriving as an error.
    let wanted = if session.closure_symbols.is_empty() {
        vec![hot.to_string()]
    } else {
        session.closure_symbols.clone()
    };
    let mut lifted = Vec::new();
    let mut last: Option<String> = None;
    for candidate in &candidates {
        match lift(candidate, &wanted) {
            // The root is `wanted[0]`, and an object that does not define it is
            // some other codegen unit rather than the one being patched.
            Ok(found) if found.iter().any(|f| symbol_matches(&f.name, &wanted[0])) => {
                lifted = found;
                break;
            }
            Ok(_) => {}
            Err(error) => last = Some(error),
        }
    }
    if lifted.is_empty() {
        record.error = Some(match last {
            Some(error) => error,
            None if candidates.is_empty() => "the compiler produced no object".into(),
            None => format!(
                "{hot} is defined in none of the {} objects produced",
                candidates.len()
            ),
        });
        return record;
    }
    record.extract_ms = at.elapsed().as_secs_f64() * 1e3;

    match publish_and_call(arena, runtime, &lifted, 3) {
        Ok((returned, publish_ms, bytes, relocations)) => {
            record.publish_ms = publish_ms;
            record.code_bytes = bytes;
            record.relocations = relocations;
            record.returned = Some(returned);
        }
        Err(error) => record.error = Some(error),
    }
    record.total_ms = whole.elapsed().as_secs_f64() * 1e3;
    record
}
