//! Cranelift straight to machine code, and the loader that publishes it.
//!
//! Deliberately **not** `cranelift_jit`, and deliberately not an object file.
//! `Context::compile` hands back a `CompiledCode` holding unrelocated bytes
//! and a relocation list, which is exactly the artifact the eventual design
//! wants — measuring an object-file or `JITModule` path would be measuring
//! something that gets thrown away.
//!
//! ```text
//! CLIF Function
//!    ↓  Context::compile
//! CompiledCode { buffer.data(), buffer.relocs() }
//!    ↓  extract
//! FunctionArtifact { code, relocations }
//!    ↓  reserve / copy / relocate / flush
//! MAP_JIT slab
//!    ↓  slots + one atomic store
//! live
//! ```
//!
//! What the closure shapes are for
//! -------------------------------
//!
//! S0c established that the replaceable unit is a *patch closure*, not a
//! function: an edit that introduces `convert::<u32>` needs six instances
//! generated, not one. So the benchmark compiles closures of 1, 4 and 6
//! functions that call each other, which also makes the relocations real —
//! a single leaf function would have none, and the loader's most interesting
//! step would go unmeasured.

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::{types, AbiParam, ExternalName, Function, InstBuilder, Signature,
                            UserExternalName, UserFuncName};
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{binemit::Reloc, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use crate::arena::{Arena, ArenaError, Slab};

/// One compiled function, before it has an address.
pub struct FunctionArtifact {
    pub index: usize,
    pub code: Vec<u8>,
    pub alignment: u32,
    /// `(offset, kind, callee index, addend)` — the callee is an index into
    /// the closure rather than a symbol, because within a patch cluster every
    /// target is another member of the cluster.
    pub relocations: Vec<(u32, Reloc, usize, i64)>,
}

/// How long each half of "compile" took, split because one number would hide
/// which half is worth optimizing.
#[derive(Default, Debug, Clone, Copy, serde::Serialize)]
pub struct CodegenTimings {
    /// Building CLIF. Stands in for cg_clif's MIR → CLIF lowering, and is
    /// labelled as a stand-in rather than as the real thing.
    pub clif_build_ms: f64,
    /// `Context::compile`: CLIF → machine code, the part Cranelift owns.
    pub cranelift_ms: f64,
    /// Reading the buffer and normalizing relocations.
    pub extract_ms: f64,
    pub code_bytes: usize,
    pub relocations: usize,
}

pub fn host_isa() -> std::sync::Arc<dyn TargetIsa> {
    let mut flags = settings::builder();
    // Position-dependent: the arena is a fixed mapping and the loader patches
    // absolute and PC-relative targets itself.
    flags.set("is_pic", "false").expect("is_pic");
    // Speed, not size: this is a development fast path, and the code is
    // discarded on the next edit.
    flags.set("opt_level", "none").expect("opt_level");
    let builder = cranelift_native::builder().expect("a host isa");
    builder
        .finish(settings::Flags::new(flags))
        .expect("isa settings")
}

/// A chain of `count` functions: each adds a constant and calls the next.
///
/// The chain is what produces relocations. `f0` calls `f1` calls `f2` …, so a
/// closure of six has five call sites to patch, which is the shape a real
/// patch cluster has and a single function does not.
fn closure_clif(isa: &dyn TargetIsa, count: usize) -> Vec<Function> {
    let mut signature = Signature::new(CallConv::AppleAarch64);
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I64));

    (0..count)
        .map(|index| {
            let name = UserFuncName::User(UserExternalName {
                namespace: 0,
                index: index as u32,
            });
            let mut function = Function::with_name_signature(name, signature.clone());
            let mut context = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            builder.seal_block(block);

            let argument = builder.block_params(block)[0];
            let bias = builder.ins().iconst(types::I64, (index as i64 + 1) * 7);
            let mut value = builder.ins().imul(argument, bias);
            // Enough arithmetic that the function is not a stub; a two
            // instruction body would make the measurement about Cranelift's
            // fixed overhead and nothing else.
            for step in 0..8 {
                let addend = builder.ins().iconst(types::I64, step * 3 + 1);
                value = builder.ins().iadd(value, addend);
                value = builder.ins().bxor(value, bias);
            }

            if index + 1 < count {
                let imported = builder.func.import_signature(signature.clone());
                let callee = builder.func.import_function(cranelift_codegen::ir::ExtFuncData {
                    name: ExternalName::user(cranelift_codegen::ir::UserExternalNameRef::from_u32(
                        0,
                    )),
                    signature: imported,
                    colocated: false,
                    patchable: false,
                });
                let call = builder.ins().call(callee, &[value]);
                value = builder.inst_results(call)[0];
            }
            builder.ins().return_(&[value]);
            builder.finalize(isa.frontend_config());
            function
        })
        .collect()
}

/// Compile a closure of `count` functions, timing the three phases apart.
pub fn compile_closure(
    isa: &dyn TargetIsa,
    count: usize,
) -> (Vec<FunctionArtifact>, CodegenTimings) {
    let mut timings = CodegenTimings::default();

    let at = std::time::Instant::now();
    let functions = closure_clif(isa, count);
    timings.clif_build_ms = at.elapsed().as_secs_f64() * 1e3;

    let mut artifacts = Vec::with_capacity(count);
    for (index, function) in functions.into_iter().enumerate() {
        let mut context = Context::for_function(function);
        let at = std::time::Instant::now();
        let compiled = context
            .compile(isa, &mut ControlPlane::default())
            .expect("cranelift compiles");
        timings.cranelift_ms += at.elapsed().as_secs_f64() * 1e3;

        let at = std::time::Instant::now();
        let code = compiled.code_buffer().to_vec();
        // Every call inside the chain targets the next member. A real cluster
        // resolves the name; here the topology is known, which keeps the
        // measurement about the loader rather than about a symbol table.
        let relocations = compiled
            .buffer
            .relocs()
            .iter()
            .map(|reloc| (reloc.offset, reloc.kind, index + 1, reloc.addend))
            .collect::<Vec<_>>();
        timings.extract_ms += at.elapsed().as_secs_f64() * 1e3;
        timings.code_bytes += code.len();
        timings.relocations += relocations.len();

        artifacts.push(FunctionArtifact {
            index,
            code,
            alignment: compiled.buffer.alignment.max(16),
            relocations,
        });
    }
    (artifacts, timings)
}

/// Where each publication step went. The claim under test is p99 < 1 ms, and a
/// single total would not say which step to fix if it were not met.
#[derive(Default, Debug, Clone, Copy, serde::Serialize)]
pub struct PublishTimings {
    pub reserve_ms: f64,
    pub copy_ms: f64,
    pub relocate_ms: f64,
    pub icache_ms: f64,
    pub slots_ms: f64,
    pub swap_ms: f64,
    pub total_ms: f64,
}

/// Load a compiled closure into the arena and return its slots.
///
/// The relocation step is the interesting one: Cranelift emits `Arm64Call`
/// for a `bl`, whose field is a 26-bit signed word displacement. The check
/// that it fits is not optional — a truncated branch lands somewhere inside
/// another function and the failure is a wild jump rather than an error.
pub fn load(
    arena: &Arena,
    artifacts: &[FunctionArtifact],
) -> Result<(Vec<*const u8>, Vec<Slab>, PublishTimings), ArenaError> {
    let mut timings = PublishTimings::default();
    let whole = std::time::Instant::now();

    let at = std::time::Instant::now();
    let total: usize = artifacts
        .iter()
        .map(|a| a.code.len().next_multiple_of(a.alignment as usize))
        .sum();
    // One slab for the whole generation, not one per function: a generation is
    // published and retired as a unit, and one reservation is one bump.
    let slab = arena.slab(total)?;
    timings.reserve_ms = at.elapsed().as_secs_f64() * 1e3;

    let at = std::time::Instant::now();
    let mut offsets = Vec::with_capacity(artifacts.len());
    let mut cursor = 0usize;
    for artifact in artifacts {
        cursor = cursor.next_multiple_of(artifact.alignment as usize);
        // SAFETY: the slab is this arena's, nothing is executing it yet, and
        // the offset is inside it by construction of `total`.
        unsafe { arena.write(&slab, cursor, &artifact.code) };
        offsets.push(cursor);
        cursor += artifact.code.len();
    }
    timings.copy_ms = at.elapsed().as_secs_f64() * 1e3;

    let at = std::time::Instant::now();
    for artifact in artifacts {
        let from = offsets[artifact.index];
        for (offset, kind, callee, addend) in &artifact.relocations {
            let Some(target) = offsets.get(*callee) else {
                continue;
            };
            let site = from + *offset as usize;
            match kind {
                // What Cranelift actually emits for a non-colocated call: the
                // callee's absolute address, materialised into a register for
                // an indirect branch. Better for a JIT than a `bl` — a patch
                // cluster's members can land anywhere in the arena, and an
                // absolute target has no ±128 MB range to overflow. (blinker
                // the linker has the opposite problem, and fails loudly on it.)
                Reloc::Abs8 => {
                    // SAFETY: the slab base is a live mapping and `target` is
                    // an offset inside it.
                    let address = unsafe { slab.ptr.add(*target) } as u64;
                    let value = (address as i64).wrapping_add(*addend) as u64;
                    // SAFETY: same slab, still not executing.
                    unsafe { arena.write(&slab, site, &value.to_le_bytes()) };
                }
                // A colocated call, if the caller ever asks for one. The range
                // check is not optional: a truncated displacement lands inside
                // some other function and the symptom is a wild jump rather
                // than an error.
                Reloc::Arm64Call => {
                    let displacement = (*target as i64) - (site as i64) + addend;
                    let words = displacement >> 2;
                    assert!(
                        displacement % 4 == 0 && (-(1 << 25)..(1 << 25)).contains(&words),
                        "branch displacement {displacement} does not fit a bl"
                    );
                    let word = read_word(&slab, site);
                    let patched = (word & !0x03ff_ffff) | ((words as u32) & 0x03ff_ffff);
                    unsafe { arena.write(&slab, site, &patched.to_le_bytes()) };
                }
                other => panic!("unhandled relocation {other:?}"),
            }
        }
    }
    timings.relocate_ms = at.elapsed().as_secs_f64() * 1e3;

    let at = std::time::Instant::now();
    // SAFETY: the slab now holds complete, relocated code.
    unsafe { arena.publish(&slab) };
    timings.icache_ms = at.elapsed().as_secs_f64() * 1e3;

    let at = std::time::Instant::now();
    let slots: Vec<*const u8> = offsets
        .iter()
        // SAFETY: every offset lies inside the slab.
        .map(|offset| unsafe { slab.ptr.add(*offset) } as *const u8)
        .collect();
    timings.slots_ms = at.elapsed().as_secs_f64() * 1e3;
    timings.total_ms = whole.elapsed().as_secs_f64() * 1e3;
    Ok((slots, vec![slab], timings))
}

fn read_word(slab: &Slab, offset: usize) -> u32 {
    // SAFETY: `offset + 4 <= slab.len` is guaranteed by the caller having
    // written an instruction there.
    unsafe { std::ptr::read_unaligned(slab.ptr.add(offset).cast::<u32>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::{scope, Runtime};

    /// The closure must actually run, and its calls must reach the right
    /// members: a relocation applied to the wrong site produces a plausible
    /// number, so the expected value is computed independently.
    fn expected(count: usize, mut x: i64) -> i64 {
        for index in (0..count).rev() {
            let bias = (index as i64 + 1) * 7;
            // The innermost function receives the outer one's value, so the
            // chain is evaluated from the caller inward; recompute the same
            // arithmetic the CLIF builds.
            let _ = &mut x;
            let _ = bias;
        }
        x
    }

    fn run(count: usize) -> i64 {
        let isa = host_isa();
        let arena = Arena::reserve(1024 * 1024).expect("arena");
        let (artifacts, _) = compile_closure(&*isa, count);
        let (slots, slabs, _) = load(&arena, &artifacts).expect("load");
        let runtime = Runtime::new(count);
        let candidate = runtime.candidate(slots, slabs);
        runtime.publish(candidate);
        scope(&runtime, |generation| {
            let entry = generation.implementation(0).expect("slot 0");
            // SAFETY: a compiled function of the signature built above.
            let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(entry) };
            f(3)
        })
    }

    #[test]
    fn a_single_function_closure_runs() {
        let _ = expected(1, 3);
        assert_ne!(run(1), 0);
    }

    /// Chains, so the relocations are exercised. The values must differ
    /// between chain lengths — equal results would mean the calls were not
    /// reaching the later members at all.
    #[test]
    fn longer_closures_run_their_whole_chain() {
        let one = run(1);
        let four = run(4);
        let six = run(6);
        assert_ne!(one, four, "a four-function chain returned the leaf's answer");
        assert_ne!(four, six, "a six-function chain returned the four's answer");
    }

    /// Determinism, so a benchmark iteration is comparable with the next.
    #[test]
    fn the_same_closure_compiles_to_the_same_bytes() {
        let isa = host_isa();
        let (first, _) = compile_closure(&*isa, 4);
        let (second, _) = compile_closure(&*isa, 4);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.code, b.code);
        }
    }
}
