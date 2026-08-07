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
    /// The verdict with its reason still attached.
    ///
    /// `verdict` above is a label, and a label is enough to decide with and not
    /// enough to *act* on: `changed_outside_closure` without the list of
    /// functions tells an agent that something moved and not what, which is the
    /// compiler-prose problem the agent API exists to remove.
    pub reason: Option<crate::classify::Verdict>,
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
    /// Every closure member's arena address, by symbol, when a candidate was
    /// accepted. This is how a probe finds a function other than the root, and
    /// how `run_affected` finds a test's freshly generated copy.
    #[serde(skip)]
    pub entries: Vec<(String, usize)>,
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

/// A constant the patch closure references, delivered beside the code (§46).
///
/// The bytes behind `.Ldata0`: a `core::panic::Location`, a string literal, an
/// array literal, a jump table. Until §46 a relocation naming one of these was
/// unresolvable and the patch was refused, which is why §43 lists "constant
/// data" as an artifact class and why the M1 fixture had to be written around
/// `ptr::add`.
pub struct Constant {
    pub name: String,
    pub bytes: Vec<u8>,
    pub align: u32,
    /// `(offset within these bytes, the constant it addresses)`. Absolute
    /// pointer-sized writes; that is the only kind a data object produces.
    pub relocations: Vec<(u64, String)>,
    /// Facts from the backend, on which the rule below is decided.
    pub local: bool,
    pub writable: bool,
    pub tls: bool,
    /// Relocations writing the address of a *function*, or naming nothing the
    /// backend could identify.
    pub function_relocs: usize,
}

/// Whether a constant may be copied into a live patch.
///
/// A patch that carries a constant creates a **second copy** of it: the base
/// image has its own, compiled in, and the unpatched functions there go on
/// using it. For read-only bytes that nothing compares by address, two
/// identical copies are indistinguishable from one — which is what makes this
/// sound, and is also the whole of the argument, so the conditions are exactly
/// the ones that argument needs.
///
/// - **`Linkage::Local`.** Nothing outside the object can name it, so no other
///   translation unit holds a reference this patch would have to keep
///   consistent. An exported constant is a `static`, and a `static` is state.
/// - **Not writable.** Duplicating mutable storage is the one thing a live
///   patch must never do: two copies of a counter is two counters. The
///   classifier already refuses a body that reads a *new* static; this is the
///   same rule enforced one layer down, where the bytes are.
/// - **Not thread-local.** A second TLS key is a second variable.
/// - **No function addresses in the bytes.** A vtable or a function-pointer
///   table is precisely the case where address identity is observable —
///   `ptr::eq` on two `dyn Trait` references compares vtable pointers, and two
///   vtables for one type would make it answer no. This is also where a
///   relocation the backend could not name is counted, because an unexplained
///   target is exactly as unjustifiable as a known-bad one.
///
/// Returns the reason it is refused, or `None` if it may be carried.
pub fn constant_is_carryable(constant: &Constant) -> Option<String> {
    if !constant.local {
        return Some(format!(
            "{} is not Linkage::Local, so it is a static rather than a constant and \
             copying it would duplicate state",
            constant.name
        ));
    }
    if constant.writable {
        return Some(format!("{} is writable", constant.name));
    }
    if constant.tls {
        return Some(format!("{} is thread-local", constant.name));
    }
    if constant.function_relocs > 0 {
        return Some(format!(
            "{} holds {} function address(es) — a vtable or function-pointer table, whose \
             identity is observable through `ptr::eq`",
            constant.name, constant.function_relocs
        ));
    }
    None
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

/// A generation that exists but is not reachable.
///
/// The arena holds its code, it is fully relocated, its i-cache is flushed and
/// its slot table is built — and `Runtime::current` has never heard of it. That
/// separation is the point. cg_clif delivers an artifact from inside codegen,
/// before anything has classified the revision, so an artifact that publishes
/// on arrival is globally visible for the whole window in which it is being
/// decided about. Under the sequential soak that window was harmless; a request
/// arriving inside it would execute code nothing had accepted.
///
/// So the backend may only build one of these. Exactly one function makes a
/// generation reachable on the forward path — `Runtime::publish` — and it is
/// called after the classifier and the artifact rules have both said yes.
pub struct Staged {
    generation: Box<crate::generation::Generation>,
    /// Every closure member's arena address, by symbol, so a caller can find a
    /// gate other than the root. Addresses rather than pointers because this
    /// crosses a thread boundary.
    pub entries: Vec<(String, usize)>,
    pub publish_ms: f64,
    pub code_bytes: usize,
    pub relocations: usize,
}

impl Staged {
    pub fn entry(&self) -> usize {
        self.entries.first().map(|(_, address)| *address).unwrap_or(0)
    }

    /// The one call that makes a candidate reachable.
    pub fn commit(self, runtime: &Runtime) -> u64 {
        runtime.publish(self.generation)
    }
}

/// Build a candidate from the lifted functions. Nothing becomes visible.
///
/// Relocations to symbols outside the closure are resolved against the base
/// image, or failing that the running process. A target that cannot be
/// resolved fails the publication rather than being patched with a guess, and
/// the failure happens before anything is made executable — the arena's
/// `publish` is the last step, not the first.
pub fn stage(
    arena: &Arena,
    runtime: &Runtime,
    lifted: &[Lifted],
    constants: &[Constant],
    image: Option<*mut libc::c_void>,
) -> Result<Staged, String> {
    use blinker_macho::Arm64RelocationKind as Kind;
    let at = Instant::now();

    // Every constant the patch is about to carry has to earn its place first,
    // and before anything is written rather than after. `stage` is the last
    // point at which refusing costs nothing.
    for constant in constants {
        if let Some(why) = constant_is_carryable(constant) {
            return Err(format!("this patch cannot carry constant data: {why}"));
        }
    }

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
    // Layout: code, then constants, then the GOT. Constants sit after the code
    // rather than in a region of their own, so they are in the same `MAP_JIT`
    // mapping and therefore executable. That is not a weakening — every byte in
    // this arena already is — but it is not where they belong either, and a
    // read-only region for them is the obvious next piece of hygiene.
    let mut data_offsets = Vec::with_capacity(constants.len());
    let mut data_cursor = code;
    for constant in constants {
        // Its own alignment, not a fixed one: a `[u64; N]` literal wants eight
        // and a jump table can want more, and a misaligned constant is a
        // correctness bug that only shows up on some targets.
        data_cursor = data_cursor.next_multiple_of(constant.align.max(1) as usize);
        data_offsets.push(data_cursor);
        data_cursor += constant.bytes.len();
    }
    let got_base = data_cursor.next_multiple_of(8);
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
    for (constant, offset) in constants.iter().zip(&data_offsets) {
        // SAFETY: an offset this function reserved inside its own slab.
        unsafe { arena.write(&slab, *offset, &constant.bytes) };
    }
    // A constant that addresses another constant — a `Location` holding a
    // pointer to its file name — is patched before anything reads either.
    for (constant, offset) in constants.iter().zip(&data_offsets) {
        for (at, target) in &constant.relocations {
            let Some(index) = constants.iter().position(|c| c.name == *target) else {
                return Err(format!(
                    "{} addresses {target}, which the backend did not deliver",
                    constant.name
                ));
            };
            // SAFETY: an offset inside the slab.
            let address = (unsafe { slab.ptr.add(data_offsets[index]) }) as u64;
            // SAFETY: eight bytes inside this constant, which was just written.
            unsafe { arena.write(&slab, offset + *at as usize, &address.to_le_bytes()) };
        }
    }
    let constant_address = |name: &str| -> Option<u64> {
        constants
            .iter()
            .position(|c| c.name == name)
            // SAFETY: an offset inside the slab.
            .map(|index| (unsafe { slab.ptr.add(data_offsets[index]) }) as u64)
    };

    // Fill the GOT before any instruction referring to it is patched.
    for (index, target) in got_targets.iter().enumerate() {
        let address = match lifted.iter().position(|f| f.name == *target) {
            // SAFETY: an offset inside the slab.
            Some(member) => (unsafe { slab.ptr.add(offsets[member]) }) as u64,
            // Named with the function that wants it, because "cannot resolve
            // .Ldata0" is a fact about a symbol nobody wrote and the actionable
            // question is which body needed constant data. Working that out by
            // bisecting the fixture took longer than this line.
            None => constant_address(target).or_else(|| resolve(target, image)).ok_or_else(|| {
                let wants: Vec<&str> = lifted
                    .iter()
                    .filter(|f| f.relocations.iter().any(|(_, t, _, _, _)| t == target))
                    .map(|f| f.name.as_str())
                    .collect();
                format!("cannot resolve {target} for the GOT, wanted by {}", wants.join(", "))
            })?,
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
                    // Closure member, then carried constant, then base image,
                    // then the process — §29's relocation invariant with one
                    // new category, added explicitly rather than by widening
                    // what "the base image" is taken to mean.
                    None => constant_address(target)
                        .or_else(|| resolve(target, image))
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
    let entries = lifted
        .iter()
        .zip(&slots)
        .map(|(function, slot)| (function.name.clone(), *slot as usize))
        .collect();
    let generation = runtime.candidate(slots, vec![slab]);
    let publish_ms = at.elapsed().as_secs_f64() * 1e3;

    Ok(Staged {
        generation,
        entries,
        publish_ms,
        code_bytes: total,
        relocations: applied,
    })
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
    // No constants: this is the object path, which lifts `__TEXT,__text` off
    // disk and has never looked at `__DATA,__const`. §46 gave the *sink* path
    // constant data, and leaving this one without it keeps it as the control
    // rather than quietly making the two paths differ in two ways at once.
    let staged = stage(arena, runtime, lifted, &[], None)?;
    let (publish_ms, bytes, relocations) =
        (staged.publish_ms, staged.code_bytes, staged.relocations);
    let entry = staged.entry();
    staged.commit(runtime);
    // SAFETY: the fixture is generated, so the signature is known.
    let f: extern "C" fn(u64, u32) -> u64 = unsafe { std::mem::transmute(entry as *const u8) };
    Ok((f(argument as u64, 5) as i64, publish_ms, bytes, relocations))
}

/// A symbol in the base image, or failing that in the running process.
///
/// `image` is asked first and it matters that it is: the differential loads
/// each revision's base image with `RTLD_LOCAL`, so its symbols are reachable
/// through its own handle and not through `RTLD_DEFAULT`. Asking the process
/// first would also mean that when several revisions are loaded at once, the
/// oldest one answers for all of them.
fn resolve(name: &str, image: Option<*mut libc::c_void>) -> Option<u64> {
    // Both spellings, and the order matters.
    //
    // `dlsym` wants the name without Mach-O's leading underscore, so a symbol
    // lifted from an object file has to have it stripped. But a name delivered
    // by the sink is cg_clif's *linkage* name, which carries no such prefix —
    // and Rust's v0 mangling begins `_R`. Stripping unconditionally turned
    // `_RNvNt…panic_const_div_by_zero` into `RNvNt…`, which resolves nowhere.
    //
    // Nothing caught it until §46 because until then every base-image reference
    // a patch made was to a `#[no_mangle]` symbol, where the two spellings are
    // the same string.
    let stripped = name.strip_prefix('_');
    for candidate in [Some(name), stripped].into_iter().flatten() {
        let Ok(c) = std::ffi::CString::new(candidate) else {
            continue;
        };
        for handle in image.into_iter().chain([libc::RTLD_DEFAULT]) {
            // SAFETY: an open handle and a null-terminated name; `dlsym`
            // returns null when it fails.
            let address = unsafe { libc::dlsym(handle, c.as_ptr()) };
            if !address.is_null() {
                return Some(address as u64);
            }
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
    /// Flip a byte in a carried constant (§46). The patch stays fully
    /// resolvable, so nothing refuses it — the answer is simply wrong, which is
    /// what makes this the control that proves the bytes are *read*.
    pub corrupt_constant: bool,
    /// Drop a carried constant, as an incomplete transitive walk would.
    pub omit_constant: bool,
}

/// A candidate patch, or the reason there is not one.
#[derive(Default)]
pub struct Patch {
    pub verdict: String,
    pub closure_size: usize,
    /// This revision's contract, for the next revision to be compared against.
    pub contract: Option<crate::classify::Contract>,
    /// The candidate, built and not reachable. `patch` never commits: the
    /// caller does, once it has decided. That is the whole architecture in one
    /// field — there is no path from "the backend produced code" to "the
    /// program runs it" that does not go through a caller saying so.
    pub staged: Option<Staged>,
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
            // Debug assertions off, and the reason has changed.
            //
            // It used to be a limitation: at `-Copt-level=0` they are on, every
            // raw-pointer write emits checks that call
            // `core::panicking::…(&Location)`, a `&Location` is constant data,
            // and a patch could not carry constant data. §46 removed that —
            // `checked_div` publishes a panic path and agrees with a clean
            // rebuild.
            //
            // It stays off because **both sides must be compiled the same
            // way**. The clean image is built with it off, and a differential
            // whose two halves disagree about which checks exist is comparing
            // two programs rather than two compilations of one.
            "-Cdebug-assertions=off".into(),
        ],
        true,
    );
    patch.closure_size = session.mono_items_examined;
    patch.contract = session.contract.clone();
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
            Some(outcome) => {
                // §29's set equality, on the sink path.
                //
                // It was only ever enforced on the object path below, and the
                // omitted-member control passed anyway — because a member the
                // patch had dropped left a relocation nothing could resolve.
                // §46 removed that accident when the base image became a Rust
                // `dylib`: every symbol is exported now, so the missing member
                // resolves *silently* against the base image's older copy.
                //
                // Which is the exact failure this invariant is for, and it was
                // one crate-type flag away the whole time. It is checked here,
                // deterministically, rather than left to a behavioural
                // difference that only appears when the dropped member happens
                // to be one this revision changed.
                match check_universe(&outcome.defined, &session.universe_symbols) {
                    Err(error) if !session.universe_symbols.is_empty() => {
                        patch.error = Some(error)
                    }
                    _ => match outcome.staged {
                        Some(staged) => patch.staged = Some(staged),
                        None => patch.error = Some("the sink staged nothing".into()),
                    },
                }
            }
            None => patch.error = Some("the sink delivered nothing".into()),
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

    match stage(arena, runtime, &lifted, &[], image) {
        Ok(staged) => patch.staged = Some(staged),
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
    let (mut record, staged) = sink_candidate(
        arena,
        runtime,
        fixture,
        target_file,
        incremental,
        hot,
        edited,
        before,
        out_dir,
        image,
        extra,
        &crate::Extras::default(),
    );
    let Some(staged) = staged else {
        return record;
    };
    let entry = staged.entry();
    staged.commit(runtime);
    // SAFETY: an address in the arena, of the fixture's declared signature.
    let f: extern "C" fn(u64, u32) -> u64 = unsafe { std::mem::transmute(entry as *const u8) };
    record.returned = Some(f(3, 5) as i64);
    record
}

/// The same revision, stopping one step short of publication.
///
/// This is the split §40 argued for, made available to a caller: everything up
/// to and including the acceptance decision happens here, and the caller is
/// handed a `Staged` that nothing can reach yet. `sink_revision` above is the
/// thin wrapper that commits immediately, so the measured paths run exactly the
/// sequence they were measured running.
///
/// The agent protocol needs the halves apart, because the useful thing an agent
/// does between them is *probe the candidate*. A patch that has to be published
/// before it can be observed forces every experiment to be a deployment.
#[allow(clippy::too_many_arguments)]
pub fn sink_candidate(
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
    extras: &crate::Extras,
) -> (LiveRecord, Option<Staged>) {
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
    let session = crate::run_session_full(
        fixture,
        target_file,
        incremental,
        hot,
        crate::Mode::HotClosure,
        &arguments,
        true,
        extras,
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

    // One acceptance point, and one rollback for everything else.
    //
    // The sink publishes from inside codegen, before anything has classified
    // the revision — it has to, because that is where the artifact exists. So
    // *every* path that does not accept the patch has to undo it, not just the
    // FALLBACK one. The first version returned early on a missing contract
    // without rolling back, and the soak caught it on the revision after every
    // refused one: the program was running new code that nothing had approved,
    // while the harness believed nothing had been published.
    if let Some(outcome) = outcome.as_ref() {
        record.publish_ms = outcome.publish_ms;
        record.code_bytes = outcome.code_bytes;
        record.relocations = outcome.relocations;
        record.active_ms = outcome.active_ms;
        record.timings = Some(outcome.timings);
        record.object_defines = outcome.timings.functions as usize;
    }
    let staged = outcome
        .as_ref()
        .is_some_and(|outcome| outcome.staged.is_some() && outcome.error.is_none());

    let accept = (|| {
        if let Some(error) = session.error {
            return Err(error);
        }
        let (Some(after), Some(before)) = (session.contract.as_ref(), before) else {
            return Err("no previous revision to compare against".into());
        };
        let verdict = crate::classify::classify(before, after);
        record.verdict = verdict.label();
        record.reason = Some(verdict.clone());
        if !verdict.is_direct() {
            // Not an error: a refusal is the classifier working.
            return Ok(false);
        }
        if let Some(error) = outcome.as_ref().and_then(|outcome| outcome.error.clone()) {
            return Err(error);
        }
        if !staged {
            return Err("the sink staged nothing".into());
        }
        // §29's set equality, on the path that actually runs.
        //
        // It was only ever enforced on the object path. The sink path got away
        // with it because an omitted closure member left an unresolvable
        // relocation, so the control passed — for a reason it did not claim.
        // §46 removed that accident: the base image became a Rust `dylib`, which
        // exports every symbol, so a missing member now resolves *silently*
        // against the base image's older copy of it. That is precisely the
        // failure this invariant exists to prevent, and it was one crate-type
        // flag away the whole time.
        if let Some(outcome) = outcome.as_ref() {
            if !session.universe_symbols.is_empty() {
                check_universe(&outcome.defined, &session.universe_symbols)?;
            }
        }
        Ok(true)
    })();

    let accepted = match accept {
        Ok(accepted) => accepted,
        Err(error) => {
            record.error = Some(error);
            false
        }
    };
    // A refused candidate is *discarded*, not rolled back. Its arena slab stays
    // allocated until reclamation exists — R1 deferred that — but nothing ever
    // pointed at it, so there is no window in which it could have run.
    let staged = outcome.filter(|_| accepted).and_then(|outcome| outcome.staged);
    if let Some(staged) = &staged {
        record.entries = staged.entries.clone();
    }
    (record, staged)
}

/// The session's own index, when one was asked for.
///
/// Returned beside the record rather than inside it: an `Index` is the agent
/// protocol's data and every measured path serializes `LiveRecord` to JSON.
pub fn index_session(
    fixture: &crate::Fixture,
    target_file: &Path,
    incremental: &Path,
    hot: &str,
    extra: &[String],
) -> (Option<crate::index::Index>, Option<crate::classify::Contract>, f64, Option<String>) {
    let at = Instant::now();
    let session = crate::run_session_full(
        fixture,
        target_file,
        incremental,
        hot,
        crate::Mode::HotClosure,
        extra,
        true,
        &crate::Extras { roots: Vec::new(), index: true },
    );
    (
        session.index,
        session.contract,
        at.elapsed().as_secs_f64() * 1e3,
        session.error,
    )
}

#[cfg(test)]
mod constant_tests {
    use super::*;

    fn readonly_local() -> Constant {
        Constant {
            name: ".Ldata0".into(),
            bytes: vec![1, 2, 3, 4],
            align: 8,
            relocations: Vec::new(),
            local: true,
            writable: false,
            tls: false,
            function_relocs: 0,
        }
    }

    #[test]
    fn a_private_read_only_constant_may_be_carried() {
        assert_eq!(constant_is_carryable(&readonly_local()), None);
    }

    #[test]
    fn an_exported_constant_is_a_static_and_is_refused() {
        // The distinction the whole rule turns on: a `Linkage::Local` blob is
        // private to one object and duplicating it is invisible; anything
        // nameable from outside is state, and a second copy of state is two
        // variables where the program has one.
        let refused = constant_is_carryable(&Constant { local: false, ..readonly_local() });
        assert!(refused.expect("refused").contains("static"));
    }

    #[test]
    fn writable_and_thread_local_are_refused() {
        assert!(constant_is_carryable(&Constant { writable: true, ..readonly_local() }).is_some());
        assert!(constant_is_carryable(&Constant { tls: true, ..readonly_local() }).is_some());
    }

    #[test]
    fn a_constant_holding_a_function_address_is_refused() {
        // A vtable. `ptr::eq` on two `dyn Trait` references compares vtable
        // pointers, so two vtables for one type is observable — the one case
        // where duplicating read-only bytes is not equivalent to sharing them.
        let refused = constant_is_carryable(&Constant {
            function_relocs: 1,
            ..readonly_local()
        });
        assert!(refused.expect("refused").contains("vtable"));
    }
}
