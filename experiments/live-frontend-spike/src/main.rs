//! Spike S0: what does a Rust frontend cost before a hot function can be
//! replaced?
//!
//! This measures one thing and deliberately not the next thing. It generates
//! no machine code, links nothing, and executes nothing it compiled. The
//! question is the one BLINKER_LIVE_SPEC_V2 §2.1 asks: after a small edit, how
//! long until we hold validated codegen MIR for the changed function, the set
//! of monomorphized items that changed with it, and enough ABI and layout
//! facts to decide whether replacing it is safe.
//!
//! Four paths, because one number would not be a result
//! ----------------------------------------------------
//!
//! - **A** — an ordinary compiler process per revision. Run by re-executing
//!   this binary, so that A and B stop at exactly the same point and their
//!   difference is process startup and nothing else. Comparing a `cargo check`
//!   against an in-process session would measure the stopping point, not the
//!   residency.
//! - **B** — the same sessions inside one process. `TyCtxt` does not survive
//!   between them and this does not pretend otherwise; what survives is the
//!   process, its loaded dylibs, and the incremental directory's page cache.
//! - **C** — whole-crate `collect_and_partition_mono_items`, the route
//!   ordinary compilation takes. A control, not a proposal.
//! - **D** — discovery from the hot root outward: start at the concrete
//!   instance the previous build already knew about, take its new MIR, and
//!   walk only what it reaches. This is the one the product depends on.
//!
//! What "validated MIR" means here
//! -------------------------------
//!
//! §6 is explicit that an early compiler point is not the answer. Every path
//! runs the full analysis phase — resolution, type checking, trait selection,
//! borrow check — and then forces `optimized_mir`, which is the MIR a codegen
//! backend consumes. Nothing is reported as "MIR ready" that a backend could
//! not compile.

#![feature(rustc_private)]

extern crate rustc_abi;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

use std::path::{Path, PathBuf};
use std::time::Instant;

use rustc_data_structures::fx::FxHashSet;
use rustc_driver::{Callbacks, Compilation};
use rustc_hir::attrs::Linkage;
use rustc_hir::def_id::{DefId, DefIdSet, LocalDefId};
use rustc_middle::mono::{
    CodegenUnit, MonoItem, MonoItemData, MonoItemPartitions, Visibility,
};
use rustc_middle::mir::TerminatorKind;
use rustc_middle::ty::{self, TyCtxt, TypingEnv};

mod classify;
mod differential;
mod live;

// R1's runtime, included by path rather than copied: one source of truth for
// the arena and the generation table, and the end-to-end path must exercise
// the same code R1 benchmarked or the two numbers are about different things.
#[path = "../../live-r1/src/arena.rs"]
mod arena;
#[path = "../../live-r1/src/generation.rs"]
mod generation;
use classify::{classify, contract_of, Contract, Verdict};

/// One revision's measurement, as V2 §8 asks for it.
#[derive(Debug, Default, Clone, serde::Serialize)]
struct Record {
    fixture: String,
    edit: String,
    path: String,
    iteration: usize,
    /// Wall time of the whole compiler session, in this process.
    session_ms: f64,
    /// Up to and including macro expansion.
    expand_ms: f64,
    /// Resolution, type check, trait selection, borrow check.
    analysis_ms: f64,
    /// Forcing `optimized_mir` for the hot root: the MIR a backend consumes.
    hot_mir_ms: f64,
    /// Path D: walking what the hot root reaches.
    hot_closure_ms: f64,
    /// Path C: `collect_and_partition_mono_items` over the whole crate.
    whole_crate_mono_ms: f64,
    /// ABI and layout facts for the hot root (§7).
    abi_layout_ms: f64,
    /// Building the downstream contract (S0c).
    classify_ms: f64,
    /// Everything a replacement needs, excluding process startup.
    total_required_frontend_ms: f64,
    mono_items_examined: usize,
    whole_crate_mono_items: usize,
    hot_root_found: bool,
    /// Path D's members, by backend symbol name, root first.
    closure_symbols: Vec<String>,
    /// The subset of those the backend was told to emit — Path D's members
    /// minus the ones that already exist in the base image. The object's
    /// external definitions must equal this set exactly (§29).
    universe_symbols: Vec<String>,
    /// What this revision promises downstream (S0c). `None` when the session
    /// failed or the hot root was not found.
    contract: Option<Contract>,
    /// The comparison against the previous revision, filled in by the caller
    /// once both contracts exist.
    verdict: Option<Verdict>,
    /// Non-empty when the session failed; the record is kept either way.
    error: Option<String>,
}

/// Which measurements a run performs. C and D are both inside one session:
/// running them in separate sessions would compare two different compilations
/// rather than two algorithms.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    /// Force the hot root's MIR only.
    Root,
    /// ... and walk the hot root's reachable closure.
    HotClosure,
    /// ... and collect the whole crate's mono items.
    WholeCrate,
    /// Both, so the two can be compared within one compilation.
    Both,
}

impl Mode {
    fn parse(text: &str) -> Mode {
        match text {
            "root" => Mode::Root,
            "hot" | "D" | "d" => Mode::HotClosure,
            "whole" | "C" | "c" => Mode::WholeCrate,
            _ => Mode::Both,
        }
    }

    fn wants_closure(self) -> bool {
        matches!(self, Mode::HotClosure | Mode::Both)
    }

    fn wants_whole_crate(self) -> bool {
        matches!(self, Mode::WholeCrate | Mode::Both)
    }
}

struct Measure {
    hot: String,
    mode: Mode,
    /// Replace the crate's codegen universe with Path D's closure (§27).
    closure_only: bool,
    /// Whether an empty whole-crate collection is a bug or the truth.
    expects_mono_roots: bool,
    /// Set by Path D: whether every instance it reached resolved in this crate.
    closure_is_local: bool,
    /// Statics the closure reads, by symbol path.
    statics: std::collections::BTreeSet<String>,
    /// The closure's own members, by def path (§32).
    closure_paths: std::collections::BTreeSet<String>,
    started: Instant,
    expansion_done: Option<Instant>,
    record: Record,
}

/// The hot root's name, for the query override.
///
/// A query provider is a bare `fn` and cannot capture, so the one thing the
/// override needs from the caller is passed beside it. Sessions in this harness
/// are sequential, and a mutex rather than a `static mut` says so honestly.
static PATCH_ROOT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The discriminating control for the closure-only measurement.
///
/// The residual this harness calls `codegen` is *everything after analysis* —
/// lowering, Cranelift, writing the object, finalizing the incremental session,
/// and dropping the arena. Shrinking the universe from a whole crate to four
/// functions and watching the residual fall does not say how much of the
/// remainder is the four functions and how much is fixed cost that no amount
/// of narrowing will remove.
///
/// So a third mode compiles *nothing at all* and still emits an object. The
/// difference between it and closure-only is the closure's real backend cost;
/// what is left underneath it is what the compiler charges for finishing a
/// session, and is not a codegen number.
static EMPTY_UNIVERSE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The symbols the codegen universe was told to emit, as the backend will name
/// them.
///
/// Not the same list as `Record::closure_symbols`, which also holds instances
/// that resolve to the base image and are therefore *not* codegened here. This
/// is the set the object is checked against for equality, so it has to be the
/// set the backend was actually given.
static UNIVERSE_SYMBOLS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Path D's closure, as the crate's entire codegen universe.
///
/// This is the answer to the last loose number in §22–26. `cg_clif` is a rustc
/// *backend*: it compiles whatever `collect_and_partition_mono_items` hands it,
/// which is ordinarily every monomorphized item in the crate. That is why the
/// end-to-end codegen column was 14–59 ms for an edit whose patch closure is
/// four to six functions.
///
/// The backend does not need to be forked to fix that. The universe is a
/// *query*, and `Config::override_queries` replaces it. So the compiler runs
/// its ordinary frontend — the developer's validation, which is real work that
/// has to happen — and then codegens exactly the instances Path D found, in one
/// codegen unit, and nothing else.
///
/// Two consequences worth naming, because they are the point:
///
/// - The object now contains *exactly* the patch closure. Under whole-crate
///   codegen, a Path D member that Path D had failed to find would still have
///   been in the object and would still have linked, and the harness would not
///   have noticed. Now a missing member is an unresolvable relocation. The
///   closure-only path therefore checks Path D's completeness as a side effect
///   of being faster.
/// - Everything is given external linkage. Ordinary partitioning would make
///   most of these internal, and an internal symbol is not in the object's
///   symbol table, which is where the closure is lifted from.
fn patch_universe<'tcx>(tcx: TyCtxt<'tcx>, (): ()) -> MonoItemPartitions<'tcx> {
    let name = PATCH_ROOT.lock().expect("not poisoned").clone().unwrap_or_default();
    let mut instances: Vec<ty::Instance<'tcx>> = Vec::new();
    let mut defined = DefIdSet::default();
    let empty = EMPTY_UNIVERSE.load(std::sync::atomic::Ordering::Relaxed);
    if let Some(root) = find_hot_root(tcx, &name).filter(|_| !empty) {
        // Recomputed rather than carried over from `after_analysis`, because an
        // `Instance<'tcx>` cannot outlive the `TyCtxt` it names and so cannot be
        // parked in a static. S0 measured this walk at 0.05 ms and the queries
        // underneath it are already cached by now, so the duplicate lands in
        // the codegen column and is smaller than the column's own noise.
        let mut walk = Closure::default();
        hot_root_closure(tcx, root.to_def_id(), &mut walk);
        instances = walk.instances;
    }

    let mut unit = CodegenUnit::new(rustc_span::Symbol::intern("live_patch"));
    let mut emitted = Vec::with_capacity(instances.len());
    for instance in instances {
        emitted.push(tcx.symbol_name(instance).name.to_string());
        defined.insert(instance.def_id());
        unit.items_mut().insert(
            MonoItem::Fn(instance),
            MonoItemData {
                inlined: false,
                linkage: Linkage::External,
                visibility: Visibility::Default,
                size_estimate: 0,
            },
        );
    }
    unit.compute_size_estimate();
    *UNIVERSE_SYMBOLS.lock().expect("not poisoned") = emitted;

    MonoItemPartitions {
        codegen_units: tcx.arena.alloc_from_iter([unit]),
        all_mono_items: tcx.arena.alloc(defined),
    }
}

impl Callbacks for Measure {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        if self.closure_only {
            *PATCH_ROOT.lock().expect("not poisoned") = Some(self.hot.clone());
            config.override_queries = Some(|_session, providers| {
                providers.queries.collect_and_partition_mono_items = patch_universe;
            });
        }
    }

    fn after_expansion(&mut self, _compiler: &rustc_interface::interface::Compiler, _tcx: TyCtxt<'_>) -> Compilation {
        self.expansion_done = Some(Instant::now());
        self.record.expand_ms = self.started.elapsed().as_secs_f64() * 1e3;
        Compilation::Continue
    }

    fn after_analysis(&mut self, _compiler: &rustc_interface::interface::Compiler, tcx: TyCtxt<'_>) -> Compilation {
        let analysis_done = Instant::now();
        let since_expansion = self
            .expansion_done
            .map(|at| analysis_done.duration_since(at).as_secs_f64() * 1e3)
            .unwrap_or_default();
        self.record.analysis_ms = since_expansion;

        let Some(root) = find_hot_root(tcx, &self.hot) else {
            self.record.error = Some(format!("no function named {} in the crate", self.hot));
            return Compilation::Stop;
        };
        self.record.hot_root_found = true;

        // The MIR a backend would consume, not the MIR the borrow checker
        // produced. Forcing the query is the measurement: it is a no-op if
        // analysis already demanded it and the real cost if it did not.
        let at = Instant::now();
        let body = tcx.optimized_mir(root.to_def_id());
        std::hint::black_box(body.basic_blocks.len());
        self.record.hot_mir_ms = at.elapsed().as_secs_f64() * 1e3;

        if self.mode.wants_closure() {
            let at = Instant::now();
            let mut walk = Closure::default();
            let (examined, local) = hot_root_closure(tcx, root.to_def_id(), &mut walk);
            self.statics = walk.statics;
            self.closure_paths = walk.paths;
            self.record.closure_symbols = walk.symbols;
            self.record.hot_closure_ms = at.elapsed().as_secs_f64() * 1e3;
            self.record.mono_items_examined = examined;
            self.closure_is_local = local;
        }

        let at = Instant::now();
        let facts = abi_and_layout(tcx, root.to_def_id());
        self.record.abi_layout_ms = at.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(facts);

        // The contract, timed separately: S0 measured gathering the ABI and
        // layout facts at 0.002 ms, and the classifier is those queries plus
        // the rest of the downstream-visible surface.
        let at = Instant::now();
        let (size, local) = (self.record.mono_items_examined, self.closure_is_local);
        // Every static the crate defines, not only those this body reads. The
        // question a replacement has to answer is whether the storage exists
        // in the base image, not whether this function previously used it —
        // asking the narrower question made reading an existing static look
        // like introducing one.
        let defined: std::collections::BTreeSet<String> = tcx
            .hir_crate_items(())
            .definitions()
            .filter(|def_id| {
                matches!(
                    tcx.def_kind(*def_id),
                    rustc_hir::def::DefKind::Static { .. }
                )
            })
            .map(|def_id| tcx.def_path_str(def_id.to_def_id()))
            .collect();
        self.record.contract = Some(contract_of(
            tcx,
            root,
            size,
            local,
            &self.statics,
            &defined,
            &self.closure_paths,
            body_fingerprints(tcx),
        ));
        self.record.classify_ms = at.elapsed().as_secs_f64() * 1e3;

        if self.mode.wants_whole_crate() {
            let at = Instant::now();
            let partitions = tcx.collect_and_partition_mono_items(());
            let total: usize = partitions
                .codegen_units
                .iter()
                .map(|unit| unit.items().len())
                .sum();
            self.record.whole_crate_mono_ms = at.elapsed().as_secs_f64() * 1e3;
            self.record.whole_crate_mono_items = total;
            // A library crate has no monomorphization roots — its generics are
            // instantiated by whoever uses it — so collection over one finds
            // nothing in no time, and Path C is not a measurement there. For a
            // binary an empty result means the run is wrong: the first version
            // of this spike reported 0 items for a 300-function crate and the
            // number was read past. Which of the two it is comes from the
            // fixture, so the check knows what it is looking at.
            if total == 0 && self.expects_mono_roots {
                self.record.error = Some(
                    "whole-crate collection found no mono items in a binary crate".into(),
                );
            }
        }

        // Everything a replacement needs. Whole-crate collection is excluded
        // deliberately: it is the control being measured against, not a cost
        // the product would pay.
        self.record.total_required_frontend_ms = self.record.expand_ms
            + self.record.analysis_ms
            + self.record.hot_mir_ms
            + self.record.hot_closure_ms
            + self.record.abi_layout_ms;

        // Continue, even though everything this spike measures is already
        // recorded above.
        //
        // Returning `Stop` here is the obvious thing and it silently destroys
        // the experiment. rustc writes its dependency graph when a session
        // *completes*; a session that stops after analysis leaves its
        // incremental directory as `s-…-working` and finalizes nothing. Every
        // run then starts cold, and the first version of this reported a flat
        // 200 ms of analysis across four consecutive iterations of the same
        // edit — a number that looked like a result and was an artefact of
        // the harness. The phases above are timed before this returns, so
        // letting compilation finish costs wall time and no accuracy.
        Compilation::Continue
    }
}

/// The hot root, by the last segment of its path.
///
/// By name rather than by `DefId`: a `DefId` is index-shaped and meaningless
/// across compiler sessions, which is the same reason the linker cannot key a
/// contribution on an `ObjectId`.
fn find_hot_root(tcx: TyCtxt<'_>, name: &str) -> Option<LocalDefId> {
    let wanted = name.rsplit("::").next().unwrap_or(name);
    // Every definition in the crate, not just the free ones. A trait or
    // inherent method is an `AssocFn` owned by an impl and never appears in
    // `hir_free_items`, so searching only those made the classifier's
    // `AssociatedFn` guard untestable — the adversarial case reported "no
    // function named compute in the crate" rather than exercising it.
    tcx.hir_crate_items(())
        .definitions()
        .find(|def_id| {
            matches!(
                tcx.def_kind(*def_id),
                rustc_hir::def::DefKind::Fn | rustc_hir::def::DefKind::AssocFn
            ) && tcx.item_name(def_id.to_def_id()).as_str() == wanted
                // A trait declares `compute` and an impl defines it. Both are
                // `AssocFn` with the same name and only the definition has a
                // body, so picking the declaration aborts the session in
                // `optimized_mir`.
                && tcx.is_mir_available(def_id.to_def_id())
        })
}

/// Every function this crate defines, and a fingerprint of its body.
///
/// The hole this closes was found by the runtime differential, and it was found
/// the only way it could have been. `edit_outside_closure` changes `diff_entry`
/// — a function that is not the hot root and not in its closure. Every field
/// S0c compared was identical, because every field S0c compared is *about the
/// hot root*. The patch published, the base image kept its old `diff_entry`,
/// and the program returned 7 where a clean rebuild returned 8.
///
/// No amount of care about the root's signature, ABI, symbol or MIR
/// observability can see that, because the root did not change. The missing
/// premise is not about the root at all: **a live patch replaces the closure,
/// so nothing outside the closure may have changed.**
///
/// The fingerprints are rustc's own. It already hashes each HIR owner including
/// bodies, for its own incremental machinery, so this reads a number that has
/// been computed rather than computing one — which is why §32 measures the
/// check at a fraction of a millisecond on crates with hundreds of functions.
fn body_fingerprints(tcx: TyCtxt<'_>) -> std::collections::BTreeMap<String, String> {
    use rustc_hir::def::DefKind;
    tcx.hir_crate_items(())
        .definitions()
        .filter(|def_id| {
            matches!(tcx.def_kind(*def_id), DefKind::Fn | DefKind::AssocFn)
                && tcx.is_mir_available(def_id.to_def_id())
        })
        .map(|def_id| {
            let owner = rustc_hir::OwnerId { def_id };
            let hash = tcx.hir_owner_nodes(owner).opt_hash;
            (
                // `def_path_str`, not `def_path`, cost 3.7 ms — flat across a
                // 40-function crate and a 900-function one, which is the shape
                // of one crate-wide query rather than of per-item work. It
                // renders the *visible* path, which needs `visible_parent_map`
                // over the whole crate graph. The identity wanted here is the
                // definition path, and that is local data.
                tcx.def_path(def_id.to_def_id()).to_string_no_crate_verbose(),
                // `None` is recorded as such rather than as a constant. Two
                // revisions that both fail to produce a fingerprint would
                // otherwise compare equal, and the check would pass by being
                // unable to run — `classify` refuses instead.
                match hash {
                    Some(hash) => format!("{hash:?}"),
                    None => String::new(),
                },
            )
        })
        .collect()
}

/// What a Path D walk produces.
///
/// Five out-parameters had accumulated, and the fifth — the closure's def
/// paths, which §32 needs — was the one that made it worth naming.
#[derive(Default)]
struct Closure<'tcx> {
    /// The members, by the symbol the backend will give them, root first. This
    /// is what makes Path D actionable rather than merely countable: publishing
    /// a patch cluster means finding *these* functions in the generated code.
    symbols: Vec<String>,
    /// The members that are defined in this crate, by def path. The unit in
    /// which "this edit changed only what the patch replaces" is decided.
    paths: std::collections::BTreeSet<String>,
    /// Statics the closure reads, by symbol path.
    statics: std::collections::BTreeSet<String>,
    /// The members as instances, when the caller is `patch_universe` and is
    /// about to hand them to the backend as the codegen universe.
    instances: Vec<ty::Instance<'tcx>>,
}

/// Walk what the hot root reaches, and only that.
///
/// This is the smallest algorithm that answers "what changed with it": start
/// at the concrete instance, read its codegen MIR, resolve every call it
/// makes, and repeat. It is deliberately not rustc's collector — the point of
/// the measurement is that the collector starts from every root in the crate
/// and this starts from one.
///
/// Returns the number of distinct instances examined, which is the number the
/// whole-crate figure has to be compared against.
fn hot_root_closure<'tcx>(
    tcx: TyCtxt<'tcx>,
    root: DefId,
    out: &mut Closure<'tcx>,
) -> (usize, bool) {
    let typing_env = TypingEnv::fully_monomorphized();
    let Ok(Some(root)) = ty::Instance::try_resolve(
        tcx,
        typing_env,
        root,
        ty::GenericArgs::identity_for_item(tcx, root),
    ) else {
        return (0, false);
    };

    let mut seen: FxHashSet<ty::Instance<'_>> = FxHashSet::default();
    // Whether every instance reached is one this crate can generate code for.
    // An instance whose MIR lives in another crate is one a direct
    // replacement cannot produce, so the closure is not local and the edit
    // cannot be published.
    let mut local = true;
    let mut worklist = vec![root];
    while let Some(instance) = worklist.pop() {
        // An intrinsic is not an ordinary function, and there are two kinds.
        //
        // Asking for `instance_mir` on either is an ICE rather than an error —
        // "intrinsics have no instance MIR" — which the differential suite hit
        // on its first run with a mutation that called `rotate_left`. But
        // skipping both is also wrong, and wrong in the more dangerous
        // direction: an intrinsic that is *not* `must_be_overridden` has a
        // fallback body, and a backend that does not implement it directly
        // emits an ordinary call. cg_clif does exactly that for
        // `rotate_left`, so skipping it left the object with a call to a
        // function nobody had generated.
        //
        // rustc's own collector resolves such an intrinsic to its fallback
        // `Item`, and so does this. The other kind — `must_be_overridden` —
        // has no body by construction and every backend expands it in place.
        // `Virtual` is skipped for a related reason: a `dyn` call names a
        // vtable slot rather than a body.
        let instance = match instance.def {
            ty::InstanceKind::Intrinsic(def_id) => {
                match tcx.intrinsic(def_id) {
                    Some(intrinsic) if !intrinsic.must_be_overridden => ty::Instance {
                        def: ty::InstanceKind::Item(def_id),
                        args: instance.args,
                    },
                    _ => continue,
                }
            }
            ty::InstanceKind::Virtual(_, _) => continue,
            _ => instance,
        };
        if !seen.insert(instance) {
            continue;
        }
        // The root first, then everything it reaches: the caller publishes
        // slot 0 as the entry point.
        out.symbols.push(tcx.symbol_name(instance).name.to_string());
        // Only what this crate can see. An instance defined elsewhere and
        // already compiled into the base image needs no new code, which is
        // exactly the property that makes this closure small.
        if !tcx.is_mir_available(instance.def_id()) {
            // Defined elsewhere and already compiled into the base image:
            // nothing new is needed for it, which is exactly the property
            // that keeps this closure small.
            if !instance.def_id().is_local() {
                continue;
            }
            local = false;
            continue;
        }
        // Past the check above, so: MIR is available here and the backend can
        // generate this one. That is the same predicate the closure's own
        // locality test uses, which is deliberate — the set the backend is told
        // to compile and the set the classifier reasoned about are the same set.
        out.instances.push(instance);
        if let Some(local) = instance.def_id().as_local() {
            // The same rendering `body_fingerprints` uses. When these two
            // disagreed, every changed body looked like it lay outside the
            // closure and every edit was refused — and because the summary had
            // no verdict column, that read as a silent `None` rather than as a
            // refusal. Both are fixed here.
            out.paths
                .insert(tcx.def_path(local.to_def_id()).to_string_no_crate_verbose());
        }
        let body = tcx.instance_mir(instance.def);
        // Statics the body reads. They are not in the call graph — a static
        // reaches MIR as a constant holding a pointer into its allocation —
        // so a walk that follows only `Call` terminators cannot see one. That
        // is not a cosmetic gap: a body that introduces a new static needs
        // storage that the base image does not have, and the classifier
        // called such an edit DIRECT until this was added.
        // Every constant in the body, through the visitor rustc provides.
        // `required_consts()` holds only those whose evaluation the body's
        // validity depends on, and a plain `static` read is not one of them —
        // it appears as an ordinary operand. Scanning the required set alone
        // therefore found no statics at all, and the classifier went on
        // calling a new-static edit DIRECT.
        {
            use rustc_middle::mir::visit::Visitor;
            StaticScan { tcx, out: &mut out.statics }.visit_body(body);
        }
        for block in body.basic_blocks.iter() {
            let Some(terminator) = &block.terminator else {
                continue;
            };
            let TerminatorKind::Call { func, .. } = &terminator.kind else {
                continue;
            };
            let callee = func.ty(&body.local_decls, tcx);
            let callee = instance.instantiate_mir_and_normalize_erasing_regions(
                tcx,
                typing_env,
                ty::EarlyBinder::bind(tcx, callee),
            );
            let ty::FnDef(def_id, args) = *callee.kind() else {
                continue;
            };
            // `FnDef` carries its arguments inside a `Binder`. The instance
            // was instantiated and normalized just above, so there is nothing
            // left bound — the same assertion rustc's own collector makes.
            let Some(args) = args.no_bound_vars() else {
                continue;
            };
            if let Ok(Some(resolved)) = ty::Instance::try_resolve(tcx, typing_env, def_id, args) {
                worklist.push(resolved);
            }
        }
    }
    (seen.len(), local)
}

/// Visits every constant in a body, looking for statics.
struct StaticScan<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    out: &'a mut std::collections::BTreeSet<String>,
}

impl<'tcx> rustc_middle::mir::visit::Visitor<'tcx> for StaticScan<'_, 'tcx> {
    fn visit_const_operand(
        &mut self,
        constant: &rustc_middle::mir::ConstOperand<'tcx>,
        _: rustc_middle::mir::Location,
    ) {
        match constant.const_ {
            rustc_middle::mir::Const::Val(value, _) => {
                collect_statics(self.tcx, value, self.out)
            }
            // An unevaluated constant is *not* a hazard, and treating it as
            // one made every integer cast fall back: debug MIR inserts
            // `u32::MIN`/`u32::MAX` checks around `as` casts, and those are
            // associated consts in `core`. A const is a value the backend
            // materialises into the code or into read-only data it emits with
            // the function; unlike a static it has no address in the base
            // image that must already exist. Statics are the hazard, and only
            // statics are collected here.
            _ => {}
        }
    }
}

/// Statics reachable from a constant, mirroring the collector's `collect_alloc`.
///
/// Recursive through `GlobalAlloc::Memory` because a constant may be a
/// reference to an allocation that itself holds pointers to statics; a
/// one-level check would miss `&&SOME_STATIC`.
fn collect_statics(
    tcx: TyCtxt<'_>,
    value: rustc_middle::mir::ConstValue,
    out: &mut std::collections::BTreeSet<String>,
) {
    use rustc_middle::mir::interpret::{GlobalAlloc, Scalar};
    let mut allocs = match value {
        rustc_middle::mir::ConstValue::Scalar(Scalar::Ptr(pointer, _)) => {
            vec![pointer.provenance.alloc_id()]
        }
        rustc_middle::mir::ConstValue::Indirect { alloc_id, .. } => vec![alloc_id],
        rustc_middle::mir::ConstValue::Slice { alloc_id, .. } => vec![alloc_id],
        _ => Vec::new(),
    };
    let mut seen = std::collections::BTreeSet::new();
    while let Some(alloc_id) = allocs.pop() {
        if !seen.insert(alloc_id) {
            continue;
        }
        match tcx.global_alloc(alloc_id) {
            GlobalAlloc::Static(def_id) => {
                out.insert(tcx.def_path_str(def_id));
            }
            GlobalAlloc::Memory(alloc) => {
                for provenance in alloc.inner().provenance().ptrs().values() {
                    allocs.push(provenance.alloc_id());
                }
            }
            _ => {}
        }
    }
}

/// The facts §7 requires before a function may be replaced in place.
///
/// Returned as a tuple rather than hashed: S0 measures what it costs to
/// *establish* them, and a persistent hash format is a design decision for
/// the milestone that keeps them.
fn abi_and_layout(tcx: TyCtxt<'_>, root: DefId) -> (usize, usize) {
    let typing_env = TypingEnv::fully_monomorphized();
    let signature = tcx.fn_sig(root).instantiate_identity().skip_normalization();
    let signature = tcx.instantiate_bound_regions_with_erased(signature);

    let mut sized = 0usize;
    let mut bytes = 0usize;
    for ty in signature.inputs().iter().copied().chain([signature.output()]) {
        if let Ok(layout) = tcx.layout_of(typing_env.as_query_input(ty)) {
            sized += 1;
            bytes += layout.size.bytes() as usize;
        }
    }
    (sized, bytes)
}

/// Drop `name` and `name=value` and `name value` from an argument list.
fn strip_pairs(args: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if names.contains(&arg.as_str()) {
            skip = true;
            continue;
        }
        if names.iter().any(|n| arg.starts_with(&format!("{n}="))) {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// The sysroot, asked for once.
///
/// This used to spawn `rustc --print=sysroot` on every session. It is harness
/// overhead and not a cost the product would pay, and it was outside the
/// session timer — so the end-to-end total came out 44 ms above the sum of its
/// own stages. A total that exceeds its parts is the harness measuring itself.
fn sysroot() -> &'static str {
    static SYSROOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SYSROOT.get_or_init(|| {
        let out = std::process::Command::new("rustc")
            .arg("--print=sysroot")
            .output()
            .expect("rustc runs");
        String::from_utf8(out.stdout).expect("utf8").trim().to_string()
    })
}

/// What a fixture is: a crate, the file an edit lands in, and the function
/// standing in for a hot root.
///
/// Read from `fixture.json` rather than hardcoded, because `medium` and
/// `large` are real crates whose rustc invocation comes from cargo and cannot
/// be guessed. `args` is that invocation when one was captured; when it is
/// empty the driver builds a self-contained one, which only works for a crate
/// with no dependencies.
#[derive(Debug, Clone, serde::Deserialize)]
struct Fixture {
    name: String,
    #[serde(rename = "crate")]
    crate_path: String,
    file: String,
    hot: String,
    #[serde(default = "default_crate_name")]
    crate_name: String,
    #[serde(default = "default_crate_type")]
    crate_type: String,
    #[serde(default)]
    args: Vec<String>,
}

fn default_crate_name() -> String {
    "fixture".into()
}

fn default_crate_type() -> String {
    "bin".into()
}

/// One compiler session over `file`, stopping after analysis.
fn run_session(
    fixture: &Fixture,
    file: &Path,
    incremental: &Path,
    hot: &str,
    mode: Mode,
) -> Record {
    run_session_with(fixture, file, incremental, hot, mode, &[], false)
}

/// The same session, with extra rustc arguments.
///
/// The end-to-end path adds `-Zcodegen-backend=cranelift --emit=obj`, so that
/// one compiler run produces the validation, the classification, the closure
/// *and* the machine code — which is the point: a developer pays for one
/// compilation, not two.
fn run_session_with(
    fixture: &Fixture,
    file: &Path,
    incremental: &Path,
    hot: &str,
    mode: Mode,
    extra: &[String],
    closure_only: bool,
) -> Record {
    let mut args: Vec<String> = if fixture.args.is_empty() {
        vec![
            "rustc".into(),
            file.display().to_string(),
            "--edition=2021".into(),
            format!("--crate-type={}", fixture.crate_type),
            format!("--crate-name={}", fixture.crate_name),
            "-Copt-level=0".into(),
            "-Cdebuginfo=0".into(),
            "--emit=metadata".into(),
            format!("--out-dir={}", incremental.display()),
        ]
    } else {
        fixture.args.clone()
    };
    args.push(format!("--sysroot={}", sysroot()));
    args.push(format!("-Cincremental={}", incremental.display()));
    // Deny nothing and render nothing: a diagnostic that is printed is a
    // diagnostic that is timed, and lint output is not what this measures.
    args.push("--cap-lints=allow".into());
    if !extra.is_empty() {
        // `--emit`, `--out-dir` and `-o` are two-token flags in the captured
        // cargo invocation, and adding a second `--emit` does not override the
        // first — rustc simply writes what the earlier one asked for and the
        // object never appears. Removing the flag alone leaves its *path* as a
        // positional input, which rustc reports as "multiple input filenames".
        // Both tokens go, or neither.
        args = strip_pairs(&args, &["--emit", "--out-dir", "-o"]);
    }
    args.extend(extra.iter().cloned());

    // Cleared, not merely overwritten: if the override never runs — the session
    // failed, or the mode is whole-crate — a stale list from the previous
    // session would be checked against this session's object, and the two would
    // agree often enough to be worse than no check.
    UNIVERSE_SYMBOLS.lock().expect("not poisoned").clear();

    let mut measure = Measure {
        hot: hot.to_string(),
        mode,
        closure_only,
        expects_mono_roots: fixture.crate_type == "bin",
        closure_is_local: true,
        statics: std::collections::BTreeSet::new(),
        closure_paths: std::collections::BTreeSet::new(),
        started: Instant::now(),
        expansion_done: None,
        record: Record::default(),
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rustc_driver::run_compiler(&args, &mut measure);
    }));
    measure.record.session_ms = measure.started.elapsed().as_secs_f64() * 1e3;
    measure.record.universe_symbols = UNIVERSE_SYMBOLS.lock().expect("not poisoned").clone();
    if outcome.is_err() && measure.record.error.is_none() {
        measure.record.error = Some("the compiler session failed".into());
    }
    measure.record
}

struct Options {
    fixture: Fixture,
    variants: PathBuf,
    target_file: PathBuf,
    incremental: PathBuf,
    hot: String,
    edits: Vec<String>,
    iterations: usize,
    warmup: usize,
    mode: Mode,
    path: String,
    out: Option<PathBuf>,
    once: bool,
    end_to_end: bool,
    closure_only: bool,
    /// The runtime differential (§30), and which defect it injects.
    differential: Option<differential::Sabotage>,
}

fn parse_options() -> Options {
    let mut args = std::env::args().skip(1);
    let mut fixture = PathBuf::from("fixtures/small");
    let mut hot: Option<String> = None;
    let mut edits: Vec<String> = Vec::new();
    let mut iterations = 30;
    let mut warmup = 3;
    let mut mode = Mode::Both;
    let mut path = "B".to_string();
    let mut out = None;
    let mut once = false;
    let mut end_to_end = false;
    let mut closure_only = false;
    let mut differential = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => fixture = PathBuf::from(args.next().expect("--fixture takes a path")),
            "--hot" => hot = Some(args.next().expect("--hot takes a name")),
            "--edit" => edits.push(args.next().expect("--edit takes a name")),
            "--iterations" => iterations = args.next().expect("count").parse().expect("number"),
            "--warmup" => warmup = args.next().expect("count").parse().expect("number"),
            "--mode" => mode = Mode::parse(&args.next().expect("--mode takes a mode")),
            "--path" => path = args.next().expect("--path takes a label"),
            "--out" => out = Some(PathBuf::from(args.next().expect("--out takes a path"))),
            // Path A: one session, then exit, so the caller times the process.
            "--once" => once = true,
            // Start and exit. The difference between this and `--once` is the
            // compiler work; this on its own is execve plus loading 150 MB of
            // rustc dylibs, which is a cost a resident process pays once and
            // an ordinary one pays per edit.
            // The end-to-end path: edit, validate, classify, generate,
            // publish, call.
            "--live" => end_to_end = true,
            // Codegen exactly Path D's closure rather than the whole crate.
            // A flag rather than the only behaviour, because the difference
            // between the two is the measurement.
            "--closure-only" => closure_only = true,
            // The runtime differential. The argument names the defect to
            // inject: `none` is the suite proper, the rest are its controls.
            "--differential" => {
                let which = args.next().unwrap_or_else(|| "none".into());
                differential = Some(
                    differential::Sabotage::parse(&which)
                        .unwrap_or_else(|| panic!("unknown sabotage {which}")),
                );
            }
            // The control: an empty codegen universe, still emitting an
            // object. Everything it costs after analysis is fixed cost.
            "--empty-universe" => {
                closure_only = true;
                EMPTY_UNIVERSE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            "--noop" => {
                println!("[]");
                std::process::exit(0);
            }
            other => panic!("unknown argument {other}"),
        }
    }
    if edits.is_empty() {
        edits = vec!["body_arith".into()];
    }
    let described = std::fs::read_to_string(fixture.join("fixture.json"))
        .unwrap_or_else(|_| panic!("no fixture.json under {}", fixture.display()));
    let described: Fixture = serde_json::from_str(&described).expect("fixture.json parses");
    let root = PathBuf::from(&described.crate_path);
    Options {
        variants: root.join("variants"),
        target_file: root.join(&described.file),
        incremental: root.join("target/spike-incremental"),
        hot: hot.unwrap_or_else(|| described.hot.clone()),
        fixture: described,
        edits,
        iterations,
        warmup,
        mode,
        path,
        out,
        once,
        end_to_end,
        closure_only,
        differential,
    }
}

fn main() {
    let options = parse_options();
    std::fs::create_dir_all(&options.incremental).expect("incremental directory");
    rustc_driver::install_ice_hook("https://github.com/blasrodri/blinker", |_| ());

    if let Some(sabotage) = options.differential {
        let trials = differential::run(&options, sabotage);
        let ok = differential::report(&trials, sabotage, options.out.as_ref());
        std::process::exit(if ok { 0 } else { 1 });
    }
    if options.end_to_end {
        return live_main(&options);
    }

    let mut records: Vec<Record> = Vec::new();
    for edit in &options.edits {
        // Every revision is written from the pristine text, never from what
        // the last one left: an edit applied on top of an edit measures a
        // program nobody wrote.
        let pristine = std::fs::read_to_string(options.variants.join("pristine.rs"))
            .expect("the pristine variant exists");
        let edited = std::fs::read_to_string(options.variants.join(format!("{edit}.rs")))
            .unwrap_or_else(|_| panic!("no variant named {edit}"));

        let total = if options.once {
            1
        } else {
            options.warmup + options.iterations
        };
        for iteration in 0..total {
            // Alternate, so every timed iteration is a real edit against a
            // warm incremental directory rather than a recompile of text the
            // compiler has already seen this run. Timing only the edited half.
            //
            // Skipped for Path A, where the caller is timing the whole
            // process: running two compilations there and calling the result
            // "process startup" would charge a compile to `execve`.
            // The pristine revision is compiled first for two reasons: it
            // leaves the incremental directory warm, so the timed session
            // measures a real edit rather than a cold crate, and it supplies
            // the *before* contract. A verdict is a relation between two
            // revisions and cannot be computed inside one compilation.
            let mut before = None;
            if !options.once {
                std::fs::write(&options.target_file, &pristine).expect("write pristine");
                let warm = run_session(
                    &options.fixture,
                    &options.target_file,
                    &options.incremental,
                    &options.hot,
                    Mode::HotClosure,
                );
                before = warm.contract;
            }

            std::fs::write(&options.target_file, &edited).expect("write edited");
            let mut record = run_session(
                &options.fixture,
                &options.target_file,
                &options.incremental,
                &options.hot,
                options.mode,
            );
            record.verdict = match (&before, &record.contract) {
                (Some(before), Some(after)) => Some(classify(before, after)),
                // No `before` means no comparison is possible, and a verdict
                // that guessed would be worse than none.
                _ => None,
            };
            record.fixture = options.fixture.name.clone();
            record.edit = edit.clone();
            record.path = options.path.clone();
            record.iteration = iteration;
            if iteration >= options.warmup || options.once {
                records.push(record);
            }
        }
        std::fs::write(&options.target_file, &pristine).expect("restore pristine");
    }

    let json = serde_json::to_string_pretty(&records).expect("serialize");
    match &options.out {
        Some(path) => std::fs::write(path, json).expect("write results"),
        None => println!("{json}"),
    }
}

/// The end-to-end benchmark: a real edit becoming live code, repeatedly.
fn live_main(options: &Options) {
    let arena = arena::Arena::reserve(256 * 1024 * 1024).expect("arena");
    let runtime = generation::Runtime::new(8);
    let object = options.incremental.join("live.o");
    // Whole-crate codegen leaves one object per codegen unit and closure-only
    // leaves one; switching modes in the same directory would otherwise leave
    // the other mode's objects behind for the newest-file rule to trip over.
    if let Ok(entries) = std::fs::read_dir(&options.incremental) {
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|e| e == "o") {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    let pristine = std::fs::read_to_string(options.variants.join("pristine.rs"))
        .expect("the pristine variant exists");
    let mut records: Vec<live::LiveRecord> = Vec::new();

    for edit in &options.edits {
        let edited = std::fs::read_to_string(options.variants.join(format!("{edit}.rs")))
            .unwrap_or_else(|_| panic!("no variant named {edit}"));
        for iteration in 0..(options.warmup + options.iterations) {
            // The pristine revision first: it warms the incremental directory
            // and supplies the *before* contract a verdict is a comparison
            // against. Its cost is the developer's previous build, not this
            // edit's, so it is not in the number.
            std::fs::write(&options.target_file, &pristine).expect("write pristine");
            let base = run_session_with(
                &options.fixture,
                &options.target_file,
                &options.incremental,
                &options.hot,
                Mode::HotClosure,
                &[
                    "-Zcodegen-backend=cranelift".into(),
                    "--emit=obj".into(),
                    format!("--out-dir={}", object.parent().unwrap_or(&object).display()),
                ],
                options.closure_only,
            );
            let mut record = live::revision(
                &arena,
                &runtime,
                &options.fixture,
                &options.target_file,
                &options.incremental,
                &options.hot,
                &edited,
                base.contract.as_ref(),
                &object,
                options.closure_only,
            );
            record.edit = edit.clone();
            // The proof, not a demonstration: with `value = 3, scale = 5` the
            // hot root's total is 15, so the pristine body returns
            // `15 * 7 + 1 = 106` and `body_arith` returns `15 * 11 + 2 = 167`.
            // A published function that returns anything else has not
            // replaced what the source says it replaced.
            let expected: Option<i64> = match edit.as_str() {
                "body_arith" => Some(167),
                "pristine" => Some(106),
                _ => None,
            };
            if let (Some(expected), Some(returned)) = (expected, record.returned) {
                if expected != returned {
                    record.error = Some(format!(
                        "the published function returned {returned}, expected {expected}"
                    ));
                }
            }
            if iteration >= options.warmup {
                records.push(record);
            }
        }
        std::fs::write(&options.target_file, &pristine).expect("restore pristine");
    }

    let median = |mut values: Vec<f64>| -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        values[values.len() / 2]
    };
    let good: Vec<&live::LiveRecord> = records.iter().filter(|r| r.error.is_none()).collect();
    println!("\n  {} of {} revisions completed", good.len(), records.len());
    for record in records.iter().filter(|r| r.error.is_some()) {
        println!("    {} FAILED: {}", record.edit, record.error.clone().unwrap_or_default());
    }
    // Timings come from every record, not only the ones that published. The
    // empty-universe control deliberately produces no callable code, and its
    // whole purpose is the timing of a session that codegens nothing; reporting
    // only successful revisions would discard exactly the control. Whether a
    // revision published is the `returned` column and the FAILED lines above.
    println!(
        "\n  {:<20} {:>7} {:>9} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9}  {}",
        "edit", "expand", "analysis", "hot mir", "classify", "closure", "codegen", "extract",
        "publish", "verdict / returned"
    );
    for edit in &options.edits {
        let rows: Vec<&live::LiveRecord> =
            records.iter().filter(|r| r.edit == *edit).collect();
        if rows.is_empty() {
            continue;
        }
        println!(
            "  {:<20} {:>7.2} {:>9.2} {:>8.2} {:>9.3} {:>9.3} {:>9.2} {:>9.3} {:>9.4}  {}",
            edit,
            median(rows.iter().map(|r| r.expand_ms).collect()),
            median(rows.iter().map(|r| r.analysis_ms).collect()),
            median(rows.iter().map(|r| r.hot_mir_ms).collect()),
            median(rows.iter().map(|r| r.classify_ms).collect()),
            median(rows.iter().map(|r| r.closure_ms).collect()),
            median(rows.iter().map(|r| r.codegen_ms).collect()),
            median(rows.iter().map(|r| r.extract_ms).collect()),
            median(rows.iter().map(|r| r.publish_ms).collect()),
            format!("{} {:?}", rows[0].verdict, rows[0].returned),
        );
    }
    if !good.is_empty() {
        println!(
            "\n  end to end, p50: {:.2} ms   (min {:.2}, max {:.2})",
            median(good.iter().map(|r| r.total_ms).collect()),
            good.iter().map(|r| r.total_ms).fold(f64::MAX, f64::min),
            good.iter().map(|r| r.total_ms).fold(0.0, f64::max),
        );
    }
    let json = serde_json::to_string_pretty(&records).expect("serialize");
    match &options.out {
        Some(path) => std::fs::write(path, json).expect("write"),
        None => {}
    }
}
