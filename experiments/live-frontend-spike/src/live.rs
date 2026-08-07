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
    /// External definitions in the patch object, which must equal the codegen
    /// universe exactly (§29).
    pub object_defines: usize,
    pub code_bytes: usize,
    pub relocations: usize,
    /// Edit → the code is live. Set only on the sink path, where publication
    /// happens inside codegen rather than after the session.
    pub active_ms: f64,
    /// The backend's own timers, when the live sink supplied them (§34).
    pub timings: Option<crate::sink::Timings>,
    /// This revision's contract, which is the *next* revision's baseline.
    ///
    /// Carried out of the session rather than recomputed by a second one. The
    /// first version of §35 ran an extra contract-only session between
    /// revisions, and it cost more than it looked: without `--emit=obj` it left
    /// the codegen cache in a different state, so G1 and G2 measured 54 and 50
    /// ms against G0's 20 — an artefact of the harness, not of the second edit.
    /// It also tried to *link* a binary fixture whose `main` the closure-only
    /// universe had not codegened.
    #[serde(skip)]
    pub contract: Option<crate::classify::Contract>,
    /// What the published function returned. The point of the exercise.
    pub returned: Option<i64>,
    pub error: Option<String>,
}

/// Symbols a patch object may define beyond Path D's closure.
///
/// Deliberately tiny, and deliberately explicit. Closure-only codegen made the
/// object contain exactly what was asked for — 5 external definitions for
/// `blinker-lib` where whole-crate codegen produced 922 — and the value of
/// that is only realised if it is *asserted*. An object with an unexpected
/// definition is an object whose contents nobody predicted, which is the same
/// condition as an unexplained relocation and gets the same answer: refuse.
///
/// Nothing is in it today. It exists so that the first entry has to be argued
/// for rather than absorbed.
const RUNTIME_SUPPORT: &[&str] = &[];

/// A function lifted out of a cg_clif object, or delivered by its live sink.
pub struct Lifted {
    pub name: String,
    pub code: Vec<u8>,
    /// `(offset within this function, symbol name, kind, pc_relative, addend)`
    pub relocations: Vec<(u64, String, blinker_macho::Arm64RelocationKind, bool, i64)>,
}

/// Pull the named functions out of an object's `__TEXT,__text`.
///
/// A symbol's extent is the distance to the next symbol in the same section,
/// because Mach-O does not record function sizes. Sorting is therefore not a
/// convenience: without it a function's bytes run to whichever symbol happened
/// to come next in the table.
fn lift(object: &Path, wanted: &[String]) -> Result<(Vec<Lifted>, Vec<String>), String> {
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

    // What the object actually defines, for the caller's set-equality check.
    // Local symbols are excluded because they are not definitions in the sense
    // that matters here: `ltmp0`, `Ldata1` and their kin are assembler
    // bookkeeping that nothing outside the object can refer to. The predicate
    // is the linker's own `can_satisfy_reference`, so the two products agree on
    // what counts as a definition.
    let external: Vec<String> = defined
        .iter()
        .filter(|s| s.can_satisfy_reference())
        .map(|s| s.name.trim_start_matches('_').to_string())
        .collect();

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
            .map(|r| {
                let target = match r.target {
                    blinker_macho::RelocationTarget::Symbol(id) => match parsed.symbol(id) {
                        Some(symbol) => symbol.name.clone(),
                        // Same rule: a relocation whose target cannot even be
                        // named is not one to drop on the floor.
                        None => {
                            return Err(format!(
                                "{name} has a relocation at +{:#x} against an \
                                 unknown symbol",
                                r.offset - start
                            ))
                        }
                    },
                    // Section-relative: a constant pool, a jump table, or a
                    // string literal. Not in the first DIRECT class.
                    //
                    // This used to `return None`, which *dropped* the
                    // relocation: the patch was published with that field
                    // unpatched and the code read whatever cg_clif had left
                    // there. Refusing to lift a function is a rejected patch;
                    // silently skipping one of its relocations is a wrong
                    // answer at full speed. An unexplainable relocation must
                    // reject the patch, never be omitted from it.
                    blinker_macho::RelocationTarget::Section(_) => {
                        return Err(format!(
                            "{name} has a section-relative relocation at +{:#x}, \
                             which this DIRECT class does not support",
                            r.offset - start
                        ))
                    }
                };
                Ok((r.offset - start, target, r.kind, r.pc_relative, r.addend))
            })
            .collect::<Result<Vec<_>, String>>()?;
        out.push(Lifted {
            name: symbol.name.clone(),
            code,
            relocations,
        });
    }
    Ok((out, external))
}

/// The object must define Path D's closure and nothing else.
///
/// Set equality, not containment. Checking only that every closure member is
/// present would pass on the 922-symbol whole-crate object just as happily as
/// on the 5-symbol one, and the whole point of overriding the codegen universe
/// is that the object's contents are now *predicted*. A prediction that is
/// never compared against the outcome is a comment.
///
/// Both directions have teeth. An unexpected definition means the backend
/// emitted something Path D did not account for, so the closure is not what
/// the classifier reasoned about. A missing definition means the backend
/// declined to emit something Path D promised, so the patch is incomplete.
fn check_universe(defined: &[String], expected: &[String]) -> Result<(), String> {
    use std::collections::BTreeSet;
    // Both sides lose the Mach-O underscore first. `tcx.symbol_name` returns
    // the *linkage* name, which on this target already carries the prefix, and
    // the object's symbol table carries it too — but the two are not guaranteed
    // to agree on it and comparing raw strings made every symbol simultaneously
    // unexpected and missing. Normalizing where the sets are built, rather than
    // where they are produced, keeps the rule in one place.
    let bare = |s: &str| s.trim_start_matches('_').to_string();
    let defined: BTreeSet<String> = defined.iter().map(|s| bare(s)).collect();
    let expected: BTreeSet<String> = expected
        .iter()
        .map(|s| bare(s))
        .chain(RUNTIME_SUPPORT.iter().map(|s| bare(s)))
        .collect();

    let unexpected: Vec<&String> = defined.difference(&expected).collect();
    let missing: Vec<&String> = expected.difference(&defined).collect();
    if unexpected.is_empty() && missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the patch object defines {} symbols against {} expected: \
         {} unexpected {unexpected:?}, {} missing {missing:?}",
        defined.len(),
        expected.len(),
        unexpected.len(),
        missing.len(),
    ))
}

/// Mach-O prefixes every symbol with an underscore, and Rust mangles.
pub fn symbol_matches(symbol: &str, wanted: &str) -> bool {
    let bare = symbol.strip_prefix('_').unwrap_or(symbol);
    bare == wanted || bare.contains(wanted)
}

/// Publish the lifted functions and return the closure's root.
///
/// Relocations to symbols outside the closure are resolved against the base
/// image, or failing that the running process. A target that cannot be
/// resolved fails the publication rather than being patched with a guess, and
/// the failure happens before anything is made executable — the arena's
/// `publish` is the last step, not the first.
pub fn publish(
    arena: &Arena,
    runtime: &Runtime,
    lifted: &[Lifted],
    image: Option<*mut libc::c_void>,
) -> Result<(*const u8, f64, usize, usize), String> {
    use blinker_macho::Arm64RelocationKind as Kind;
    let at = Instant::now();

    let code: usize = lifted.iter().map(|f| f.code.len().next_multiple_of(16)).sum();
    // Every distinct symbol the closure reaches through the GOT needs an
    // eight-byte slot holding its address. Sized up front so the slab does not
    // have to grow while it is being relocated.
    let got_targets: Vec<String> = {
        let mut names: Vec<String> = lifted
            .iter()
            .flat_map(|f| &f.relocations)
            .filter(|(_, _, kind, _, _)| {
                matches!(kind, Kind::GotLoadPage21 | Kind::GotLoadPageOff12 | Kind::PointerToGot)
            })
            .map(|(_, target, _, _, _)| target.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let got_base = code.next_multiple_of(8);
    let total = got_base + got_targets.len() * 8;
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

    // Fill the GOT before any instruction referring to it is patched.
    for (index, target) in got_targets.iter().enumerate() {
        let address = match lifted.iter().position(|f| f.name == *target) {
            // SAFETY: an offset inside the slab.
            Some(member) => (unsafe { slab.ptr.add(offsets[member]) }) as u64,
            None => resolve(target, image)
                .ok_or_else(|| format!("cannot resolve {target} for the GOT"))?,
        };
        // SAFETY: a slot this function just reserved.
        unsafe { arena.write(&slab, got_base + index * 8, &address.to_le_bytes()) };
    }
    let got_slot = |target: &str| -> Option<usize> {
        got_targets
            .iter()
            .position(|t| t == target)
            .map(|index| got_base + index * 8)
    };

    let mut applied = 0usize;
    for (index, function) in lifted.iter().enumerate() {
        for (offset, target, kind, pc_relative, addend) in &function.relocations {
            let site = offsets[index] + *offset as usize;
            // GOT-mediated references point at the slot, not at the symbol:
            // that is the whole purpose of a GOT, and rewriting the `adrp` to
            // reach the symbol directly would silently change what the `ldr`
            // beside it loads.
            let through_got = matches!(
                kind,
                Kind::GotLoadPage21 | Kind::GotLoadPageOff12 | Kind::PointerToGot
            );
            // Inside the closure first, then the base image, then the process.
            let address = if through_got {
                let slot = got_slot(target).ok_or_else(|| format!("no GOT slot for {target}"))?;
                // SAFETY: a slot inside the slab.
                (unsafe { slab.ptr.add(slot) }) as u64
            } else {
                match lifted.iter().position(|f| f.name == *target) {
                    // SAFETY: an offset inside the slab.
                    Some(member) => (unsafe { slab.ptr.add(offsets[member]) }) as u64,
                    None => resolve(target, image)
                        .ok_or_else(|| format!("cannot resolve {target}"))?,
                }
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
                // `adrp Xd, page` — the target's 4 KiB page relative to the
                // instruction's own page. The immediate is split across two
                // fields, `immlo` at bits 30:29 and `immhi` at 23:5, which is
                // why this cannot be written as one mask.
                (Kind::Page21 | Kind::GotLoadPage21, true) => {
                    // SAFETY: the site holds an instruction cg_clif wrote.
                    let here = (unsafe { slab.ptr.add(site) }) as i64;
                    let pages = (value >> 12) - (here >> 12);
                    if !(-(1 << 20)..(1 << 20)).contains(&pages) {
                        return Err(format!("{target} is {pages} pages away, out of adrp range"));
                    }
                    let word =
                        unsafe { std::ptr::read_unaligned(slab.ptr.add(site).cast::<u32>()) };
                    let pages = pages as u32;
                    let patched = (word & !0x60ff_ffe0)
                        | ((pages & 0x3) << 29)
                        | (((pages >> 2) & 0x7_ffff) << 5);
                    unsafe { arena.write(&slab, site, &patched.to_le_bytes()) };
                }
                // The `add`/`ldr` beside it. The 12-bit immediate is *scaled*
                // by the access size for a load, and unscaled for an `add`;
                // getting that wrong reads the right page at the wrong offset,
                // which is a wrong answer rather than a crash. The size comes
                // from bits 31:30 of the instruction itself.
                (Kind::PageOff12 | Kind::GotLoadPageOff12, false) => {
                    let word =
                        unsafe { std::ptr::read_unaligned(slab.ptr.add(site).cast::<u32>()) };
                    let within = (value as u64) & 0xfff;
                    let is_load_store = (word & 0x3b00_0000) == 0x3900_0000;
                    let immediate = if is_load_store {
                        let scale = word >> 30;
                        if within & ((1 << scale) - 1) != 0 {
                            return Err(format!(
                                "{target} is not {}-byte aligned for its load",
                                1u32 << scale
                            ));
                        }
                        (within as u32) >> scale
                    } else {
                        within as u32
                    };
                    let patched = (word & !0x003f_fc00) | ((immediate & 0xfff) << 10);
                    unsafe { arena.write(&slab, site, &patched.to_le_bytes()) };
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

    let entry = scope(runtime, |generation| generation.implementation(0).expect("slot 0"));
    Ok((entry, publish_ms, total, applied))
}

/// Publish, then call the closure's root.
///
/// The signature is written down rather than assumed. `spike_hot_root` takes
/// `SpikeReading { value: u64, scale: u32 }` — a 16-byte aggregate, two
/// registers under AAPCS — and the first version of this called it as
/// `fn(i64) -> i64`. It returned 105866, then 72866: `scale` was whatever `x1`
/// happened to hold. It looked like a working demonstration and was reading
/// uninitialised register state.
fn publish_and_call(
    arena: &Arena,
    runtime: &Runtime,
    lifted: &[Lifted],
    argument: i64,
) -> Result<(i64, f64, usize, usize), String> {
    let (entry, publish_ms, total, applied) = publish(arena, runtime, lifted, None)?;
    // SAFETY: the fixture is generated, so the signature is known.
    let f: extern "C" fn(u64, u32) -> u64 = unsafe { std::mem::transmute(entry) };
    Ok((f(argument as u64, 5) as i64, publish_ms, total, applied))
}

/// A symbol in the base image, or failing that in the running process.
///
/// `image` is asked first and it matters that it is: the differential loads
/// each revision's base image with `RTLD_LOCAL`, so its symbols are reachable
/// through its own handle and not through `RTLD_DEFAULT`. Asking the process
/// first would also mean that when several revisions are loaded at once, the
/// oldest one answers for all of them.
fn resolve(name: &str, image: Option<*mut libc::c_void>) -> Option<u64> {
    let bare = name.strip_prefix('_').unwrap_or(name);
    let c = std::ffi::CString::new(bare).ok()?;
    for handle in image.into_iter().chain([libc::RTLD_DEFAULT]) {
        // SAFETY: an open handle and a null-terminated name; `dlsym` returns
        // null when it fails.
        let address = unsafe { libc::dlsym(handle, c.as_ptr()) };
        if !address.is_null() {
            return Some(address as u64);
        }
    }
    None
}

/// Deliberate defects, for the negative controls (§31).
///
/// A suite that has never failed is a suite nobody has shown *can* fail. These
/// exist so that each safety property is demonstrated by breaking it.
#[derive(Debug, Default, Clone, Copy)]
pub struct PatchOptions {
    /// Drop a member of the patch closure, as an incomplete Path D would.
    pub omit_closure_member: bool,
    /// Point a relocation at a symbol that does not exist.
    pub corrupt_relocation: bool,
    /// Publish an edit the classifier refused.
    pub ignore_classifier: bool,
}

/// A published patch, or the reason there is not one.
#[derive(Debug, Default)]
pub struct Patch {
    pub verdict: String,
    pub closure_size: usize,
    /// The patch closure's root, live in the arena.
    pub entry: Option<*const u8>,
    pub error: Option<String>,
}

/// Build and publish a patch, without calling it.
///
/// `revision` measures latency and calls the result; this one is for the
/// differential, which installs the result into a base image and drives probes
/// through it. They share every stage below.
#[allow(clippy::too_many_arguments)]
pub fn patch(
    arena: &Arena,
    runtime: &Runtime,
    fixture: &crate::Fixture,
    target_file: &Path,
    incremental: &Path,
    hot: &str,
    before: Option<&crate::classify::Contract>,
    out_dir: &Path,
    image: Option<*mut libc::c_void>,
    backend: Option<&Path>,
    options: PatchOptions,
) -> Patch {
    let mut patch = Patch::default();
    // The sink path publishes from inside codegen, so the arena and runtime
    // have to be reachable from the callback before the session starts.
    if backend.is_some() {
        crate::sink::begin(arena, runtime, image, Instant::now(), hot);
    }
    let session = crate::run_session_with(
        fixture,
        target_file,
        incremental,
        hot,
        crate::Mode::HotClosure,
        &[
            match backend {
                Some(path) => format!("-Zcodegen-backend={}", path.display()),
                None => "-Zcodegen-backend=cranelift".into(),
            },
            "--emit=obj".into(),
            format!("--out-dir={}", out_dir.display()),
            "-Cmetadata=diff".into(),
            // Debug assertions off, and this is a limitation being stated
            // rather than a convenience. At `-Copt-level=0` they are on, and
            // they make every raw-pointer write emit a null and an alignment
            // check that call `core::panicking::…(&Location)`. A `&Location`
            // is constant data, reached by a relocation against an anonymous
            // symbol in `__const`, and lifting constant data into the arena is
            // not something this DIRECT class does. So a patch that can panic
            // is out of class today — which is also why §30's observations
            // cover no panic, no stdout, and no callback.
            "-Cdebug-assertions=off".into(),
        ],
        true,
    );
    patch.closure_size = session.mono_items_examined;
    // The sink path is finished here: the artifact was published from inside
    // codegen, so there is nothing left to lift.
    if backend.is_some() {
        let outcome = crate::sink::finish();
        if let Some(error) = session.error {
            patch.error = Some(error);
            return patch;
        }
        let (Some(after), Some(before)) = (session.contract.as_ref(), before) else {
            patch.error = Some("no contract to compare".into());
            return patch;
        };
        let verdict = crate::classify::classify(before, after);
        patch.verdict = verdict.label();
        if !verdict.is_direct() && !options.ignore_classifier {
            patch.error = Some(format!("the classifier refused: {verdict:?}"));
            return patch;
        }
        match outcome {
            Some(outcome) if outcome.error.is_some() => patch.error = outcome.error,
            Some(outcome) if outcome.entry != 0 => {
                patch.entry = Some(outcome.entry as *const u8)
            }
            _ => patch.error = Some("the sink delivered nothing".into()),
        }
        return patch;
    }
    if let Some(error) = session.error {
        patch.error = Some(error);
        return patch;
    }
    let (Some(after), Some(before)) = (session.contract.as_ref(), before) else {
        patch.error = Some("no contract to compare".into());
        return patch;
    };
    let verdict = crate::classify::classify(before, after);
    patch.verdict = verdict.label();
    if !verdict.is_direct() && !options.ignore_classifier {
        patch.error = Some(format!("the classifier refused: {verdict:?}"));
        return patch;
    }

    let wanted = session.closure_symbols.clone();
    let mut lifted = Vec::new();
    let mut defines = Vec::new();
    let mut last = None;
    for candidate in objects(out_dir) {
        match lift(&candidate, &wanted) {
            Ok((found, external))
                if found.iter().any(|f| symbol_matches(&f.name, &wanted[0])) =>
            {
                lifted = found;
                defines = external;
                break;
            }
            Ok(_) => {}
            Err(error) => last = Some(error),
        }
    }
    if lifted.is_empty() {
        patch.error = Some(last.unwrap_or_else(|| format!("{hot} is in no object produced")));
        return patch;
    }
    if options.omit_closure_member {
        // The victim has to be a member some *other* member calls, or the
        // control proves nothing. The first version dropped Path D's last
        // symbol, and for two of the mutations nothing referred to it — so the
        // patch published, agreed with the clean rebuild, and the control
        // reported a failure to fail. An omission that changes no observable
        // behaviour is not an incomplete closure; it is a closure that was
        // larger than it needed to be.
        let victim = lifted
            .iter()
            .skip(1)
            .map(|f| f.name.clone())
            .find(|name| {
                lifted
                    .iter()
                    .any(|other| other.name != *name && other.relocations.iter().any(|r| r.1 == *name))
            });
        match victim {
            Some(victim) => lifted.retain(|f| f.name != victim),
            None => {
                patch.error =
                    Some("control not applicable: no member is called by another".into());
                return patch;
            }
        }
    }
    // Skipped when a member was deliberately omitted: the check would catch the
    // sabotage here, and the property under test is that an *incomplete
    // closure* is caught even by a system that got as far as relocating it.
    if !options.omit_closure_member && !session.universe_symbols.is_empty() {
        if let Err(error) = check_universe(&defines, &session.universe_symbols) {
            patch.error = Some(error);
            return patch;
        }
    }
    if options.corrupt_relocation {
        for function in &mut lifted {
            if let Some(relocation) = function.relocations.first_mut() {
                relocation.1 = "_a_symbol_that_does_not_exist".into();
                break;
            }
        }
    }

    match publish(arena, runtime, &lifted, image) {
        Ok((entry, _, _, _)) => patch.entry = Some(entry),
        Err(error) => patch.error = Some(error),
    }
    patch
}

/// Every object in a directory.
fn objects(directory: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "o"))
        .collect()
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
    let candidates: Vec<PathBuf> = std::fs::read_dir(object.parent().unwrap_or(object))
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
    let mut defines: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    for candidate in &candidates {
        match lift(candidate, &wanted) {
            // The root is `wanted[0]`, and an object that does not define it is
            // some other codegen unit rather than the one being patched.
            Ok((found, external))
                if found.iter().any(|f| symbol_matches(&f.name, &wanted[0])) =>
            {
                lifted = found;
                defines = external;
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
    // Set equality against what the codegen universe was told to emit. Only
    // meaningful when the universe was overridden — under whole-crate codegen
    // the object legitimately holds the entire crate, and `universe_symbols` is
    // empty because the override never ran.
    if !session.universe_symbols.is_empty() {
        if let Err(error) = check_universe(&defines, &session.universe_symbols) {
            record.error = Some(error);
            record.total_ms = whole.elapsed().as_secs_f64() * 1e3;
            return record;
        }
    }
    record.object_defines = defines.len();
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

/// One revision through cg_clif's live sink: no object file in the path.
///
/// The difference from `revision` is where publication happens. There, the
/// compiler session ran to completion, an object was found on disk, the
/// closure was lifted out of it and only then published. Here the artifact is
/// delivered from inside codegen and published on the spot, so the edit is
/// active while rustc is still writing its object and finalizing its
/// incremental session.
///
/// That is why this returns two times rather than one, and they answer
/// different questions: `active_ms` is what a developer waits for; `total_ms`
/// is when the compiler is ready for the next revision.
#[allow(clippy::too_many_arguments)]
pub fn sink_revision(
    arena: &Arena,
    runtime: &Runtime,
    fixture: &crate::Fixture,
    target_file: &Path,
    incremental: &Path,
    hot: &str,
    edited: &str,
    before: Option<&crate::classify::Contract>,
    out_dir: &Path,
    image: Option<*mut libc::c_void>,
    extra: &[String],
) -> LiveRecord {
    let mut record = LiveRecord {
        fixture: fixture.name.clone(),
        ..Default::default()
    };
    let whole = Instant::now();
    std::fs::write(target_file, edited).expect("write the edit");

    // The clock starts at the edit, and the sink stops it from inside codegen.
    crate::sink::begin(arena, runtime, image, whole, hot);
    let mut arguments = vec![
        "--emit=obj".to_string(),
        format!("--out-dir={}", out_dir.display()),
    ];
    arguments.extend(extra.iter().cloned());
    let session = crate::run_session_with(
        fixture,
        target_file,
        incremental,
        hot,
        crate::Mode::HotClosure,
        &arguments,
        true,
    );
    let outcome = crate::sink::finish();

    record.expand_ms = session.expand_ms;
    record.analysis_ms = session.analysis_ms;
    record.hot_mir_ms = session.hot_mir_ms;
    record.validate_ms = session.expand_ms + session.analysis_ms + session.hot_mir_ms;
    record.classify_ms = session.classify_ms;
    record.closure_ms = session.hot_closure_ms;
    record.closure_size = session.mono_items_examined;
    record.codegen_ms =
        (session.session_ms - record.validate_ms - record.closure_ms - record.classify_ms)
            .max(0.0);
    record.total_ms = whole.elapsed().as_secs_f64() * 1e3;
    record.contract = session.contract.clone();

    if let Some(error) = session.error {
        record.error = Some(error);
        return record;
    }
    // The verdict is still computed, and still decides. The sink publishes
    // whatever cg_clif compiled, so a FALLBACK has to be turned into a
    // rollback rather than prevented — which is why this runs before the
    // artifact is reported and why a refused revision is rolled back below.
    let verdict = match (session.contract.as_ref(), before) {
        (Some(after), Some(before)) => crate::classify::classify(before, after),
        _ => {
            record.error = Some("no contract to compare".into());
            return record;
        }
    };
    record.verdict = verdict.label();

    let Some(outcome) = outcome else {
        if verdict.is_direct() {
            record.error = Some("the sink delivered nothing".into());
        }
        return record;
    };
    record.publish_ms = outcome.publish_ms;
    record.code_bytes = outcome.code_bytes;
    record.relocations = outcome.relocations;
    record.active_ms = outcome.active_ms;
    record.timings = Some(outcome.timings);
    record.object_defines = outcome.timings.functions as usize;
    if let Some(error) = outcome.error {
        record.error = Some(error);
        return record;
    }
    if !verdict.is_direct() {
        // Published, then refused. The generation table's rollback is what
        // makes this recoverable rather than a hole: the classifier's answer
        // still governs what the program runs.
        // Back to the generation this revision superseded. `parent` is on the
        // generation itself, which is what makes a rollback a fact about the
        // published history rather than a count the caller has to keep.
        let parent = scope(runtime, |generation| generation.parent);
        runtime.rollback_code(parent);
        return record;
    }
    if outcome.entry != 0 {
        // SAFETY: an address in the arena, of the fixture's declared signature.
        let f: extern "C" fn(u64, u32) -> u64 =
            unsafe { std::mem::transmute(outcome.entry as *const u8) };
        record.returned = Some(f(3, 5) as i64);
    }
    record
}
