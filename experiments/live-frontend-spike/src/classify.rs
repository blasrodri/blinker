//! Is this edit safe to publish as a direct code replacement?
//!
//! The invariant
//! -------------
//!
//! **DIRECT only when the changed body is not observable through rmeta except
//! as the address of the function itself.**
//!
//! That is a narrower and far more defensible claim than "the public interface
//! is unchanged". Enumerating a Rust crate's public API from scratch means
//! guessing at what rustc considers downstream-visible, and every item guessed
//! wrong is a silently wrong program. Instead this asks rustc the question it
//! already asks itself.
//!
//! `rustc_metadata::rmeta::encoder::should_encode_mir` decides whether a body
//! is serialized into the crate's metadata, and therefore whether any
//! downstream crate can inline it, monomorphize it, or const-evaluate it:
//!
//! ```text
//! DefKind::AnonConst | AssocConst | Const   => (true,  false)   // CTFE MIR
//! DefKind::Closure if is_coroutine          => (false, true)    // layout
//! DefKind::SyntheticCoroutineBody           => (false, true)
//! DefKind::AssocFn | Fn | Closure           => {
//!     opt = always_encode_mir
//!        || (should_codegen() && reachable(def_id)
//!            && (generics.requires_monomorphization() || cross_crate_inlinable(def_id)))
//!     opt = opt && !constness(def_id).is_always_const()
//!     (is_const_fn(def_id), opt)
//! }
//! _                                         => (false, false)
//! ```
//!
//! [`mir_is_downstream_observable`] is that function, reimplemented against the
//! same queries. When it says a body would be encoded, this classifier refuses,
//! and the refusal is sound for the same reason rustc's encoding is correct.
//!
//! Everything else is a guard
//! --------------------------
//!
//! The rmeta rule answers "can another crate see this body?". It does not
//! answer "does replacing this function keep the program's meaning?" — a
//! signature change is invisible to `should_encode_mir` and fatal to a direct
//! replacement. So the remaining predicates compare the *contract*: signature,
//! generics, predicates, codegen attributes, calling convention, argument and
//! return layouts, and symbol identity, before and after.
//!
//! Two sessions, not one
//! ---------------------
//!
//! A contract cannot be checked inside a single compilation, because "changed"
//! is a relation between two revisions. The driver compiles the pristine source
//! and the edited source, takes a [`Contract`] from each, and compares them
//! here. Anything a contract does not capture is a hole in the classifier, so
//! the fields are deliberately coarse and stringly: an over-eager difference
//! costs a fallback, and a missed one costs a wrong program.

use rustc_hir::def::DefKind;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_middle::ty::{self, TyCtxt, TypingEnv};

/// What a direct replacement promises not to change.
///
/// Every field is compared for equality across the two revisions. They are
/// rendered rather than hashed so that a `FALLBACK` can say *what* differed;
/// a classifier that only reports a verdict cannot be debugged, and every
/// missed fast path is a thing the product wants to measure.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Contract {
    pub def_kind: String,
    pub fn_sig: String,
    pub type_of: String,
    pub generics: String,
    pub predicates: String,
    pub codegen_attrs: String,
    pub fn_abi: String,
    pub symbol_name: String,
    /// Whether rustc would put this body in the crate's metadata, and why.
    pub mir_observable: bool,
    pub requires_monomorphization: bool,
    pub cross_crate_inlinable: bool,
    pub is_const_fn: bool,
    pub always_const: bool,
    pub is_coroutine: bool,
    pub reachable: bool,
    pub inline_attr: String,
    pub has_opaque_in_signature: bool,
    pub is_associated: bool,
    /// Path D's closure: how many instances the changed body reaches, and
    /// whether every one of them resolved inside this crate.
    pub closure_size: usize,
    /// The closure's members, by `DefPathHash`. The set a live patch replaces.
    pub closure_paths: std::collections::BTreeSet<String>,
    /// Every function the crate defines, keyed by `DefPathHash`, holding a
    /// fingerprint of its body and a readable path (§32).
    pub bodies: std::collections::BTreeMap<String, (String, String)>,
    /// Every `const` the crate defines, by `DefPathHash` (§48).
    pub consts: std::collections::BTreeMap<String, (String, String)>,
    pub closure_is_local: bool,
    /// Statics the closure reads. A body that introduces a new one needs
    /// storage the base image does not have, and no signature or ABI
    /// comparison can see that.
    pub statics: Vec<String>,
    /// Every static the crate defines, which is what decides whether one the
    /// body reads has storage in the base image.
    pub crate_statics: Vec<String>,
}

/// Why an edit cannot be published directly. One reason, the first that fires,
/// so the order below is the order a reader should think in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The function vanished, or was never there.
    HotRootMissing,
    /// A function outside the patch closure changed in the same revision, so
    /// the base image would keep its old compiled code (§32).
    ChangedOutsideClosure { functions: Vec<String> },
    /// A `const` changed (§48). It is folded into every use site, so the base
    /// image's copy of every function that reads it is now wrong — and unlike a
    /// changed function body, nothing in the closure names it.
    ConstChanged { consts: Vec<String> },
    /// The crate's body fingerprints could not be read, so the check above
    /// cannot be performed and the edit is refused rather than assumed safe.
    BodiesUnavailable,
    /// Not an ordinary free function.
    NotAnOrdinaryFn { def_kind: String },
    /// A trait or inherent method. Excluded from the first DIRECT class
    /// because a method's identity runs through its trait's vtable and its
    /// impl's coherence, neither of which this compares.
    AssociatedFn,
    /// Generic, so downstream crates monomorphize it and rustc ships its MIR.
    RequiresMonomorphization,
    /// `#[inline]`, `#[inline(always)]`, or small enough that rustc decided to
    /// make it cross-crate inlinable. Its body is in the metadata and a
    /// downstream crate may hold a copy.
    CrossCrateInlinable,
    /// `const fn`: callable at compile time, so its body is CTFE MIR.
    ConstEvaluable,
    /// `async fn` or a coroutine: its body determines a state machine's layout.
    Coroutine,
    /// The signature mentions `impl Trait`, whose hidden type is part of the
    /// downstream contract.
    OpaqueInSignature,
    /// rustc would encode this body into the crate metadata. The catch-all for
    /// the rule above, and the reason this classifier is sound.
    CrossCrateMirObservable,
    /// Something the contract compares changed. `field` names which.
    ContractChanged { field: String, before: String, after: String },
    /// Path D could not close the changed set inside this crate.
    ClosureNotLocal,
    /// The body reads a static the previous revision did not. Its storage
    /// does not exist in the base image, so the replacement would reference a
    /// symbol that is not there.
    NewStaticRequired { statics: Vec<String> },
    /// `#[inline(never)]` is not present, so nothing stops a future rustc from
    /// inlining the body into a caller this replacement cannot reach.
    InlineNeverNotEnforced { inline_attr: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Direct,
    Fallback { reason: Reason },
}

impl Verdict {
    pub fn is_direct(&self) -> bool {
        matches!(self, Verdict::Direct)
    }

    pub fn label(&self) -> String {
        match self {
            Verdict::Direct => "DIRECT".into(),
            Verdict::Fallback { reason } => {
                let name = serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| match v {
                        serde_json::Value::String(s) => Some(s),
                        serde_json::Value::Object(map) => {
                            map.keys().next().map(|k| k.to_string())
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| "unknown".into());
                format!("FALLBACK({name})")
            }
        }
    }
}

/// rustc's own rule for whether a body reaches another crate, reimplemented.
///
/// Mirrors `rustc_metadata::rmeta::encoder::should_encode_mir`. Kept as one
/// function with the original beside it in the module docs, because the moment
/// the two drift this classifier is unsound and the drift must be visible.
///
/// Returns `(encode_ctfe, encode_optimized)`.
fn mir_is_downstream_observable(tcx: TyCtxt<'_>, def_id: LocalDefId) -> (bool, bool) {
    match tcx.def_kind(def_id) {
        DefKind::Ctor(_, _) => (true, false),
        DefKind::AnonConst | DefKind::AssocConst { .. } | DefKind::Const { .. } => (true, false),
        DefKind::Closure if tcx.is_coroutine(def_id.to_def_id()) => (false, true),
        DefKind::SyntheticCoroutineBody => (false, true),
        DefKind::AssocFn | DefKind::Fn | DefKind::Closure => {
            let opt = tcx.sess.opts.unstable_opts.always_encode_mir
                || (tcx.sess.opts.output_types.should_codegen()
                    && tcx.reachable_set(()).contains(&def_id)
                    && (tcx.generics_of(def_id).requires_monomorphization(tcx)
                        || tcx.cross_crate_inlinable(def_id)));
            let opt = opt
                && !matches!(
                    tcx.constness(def_id),
                    rustc_hir::Constness::Const { always: true }
                );
            (tcx.is_const_fn(def_id.to_def_id()), opt)
        }
        _ => (false, false),
    }
}

/// Whether any type in the signature is an opaque `impl Trait`.
fn mentions_opaque(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    let signature = tcx.fn_sig(def_id).instantiate_identity().skip_normalization();
    let signature = tcx.instantiate_bound_regions_with_erased(signature);
    signature
        .inputs()
        .iter()
        .copied()
        .chain([signature.output()])
        .any(|ty| {
            ty.walk().any(|part| {
                part.as_type()
                    // `TyKind::Alias(IsRigid, AliasTy)`: the alias kind lives
                    // on the `AliasTy`, and `is_opaque` is rustc's own test.
                    .is_some_and(|t| {
                        matches!(t.kind(), ty::Alias(_, alias) if alias.is_opaque())
                    })
            })
        })
}

/// Everything the classifier needs about one revision of one function.
pub fn contract_of(
    tcx: TyCtxt<'_>,
    def_id: LocalDefId,
    closure_size: usize,
    closure_is_local: bool,
    statics: &std::collections::BTreeSet<String>,
    crate_statics: &std::collections::BTreeSet<String>,
    closure_paths: &std::collections::BTreeSet<String>,
    bodies: std::collections::BTreeMap<String, (String, String)>,
    consts: std::collections::BTreeMap<String, (String, String)>,
) -> Contract {
    let global = def_id.to_def_id();
    let def_kind = tcx.def_kind(def_id);
    let (ctfe, optimized) = mir_is_downstream_observable(tcx, def_id);

    let typing_env = TypingEnv::fully_monomorphized();
    let fn_abi = match ty::Instance::try_resolve(
        tcx,
        typing_env,
        global,
        ty::GenericArgs::identity_for_item(tcx, global),
    ) {
        Ok(Some(instance)) => {
            // The query directly rather than the `FnAbiOf` helper trait: the
            // helper needs a context that implements error handling, and this
            // wants the error rather than a diagnostic.
            match tcx.fn_abi_of_instance(
                typing_env.as_query_input((instance, ty::List::empty())),
            ) {
                Ok(abi) => format!(
                    "conv={:?} args=[{}] ret={:?}",
                    abi.conv,
                    abi.args
                        .iter()
                        .map(|arg| format!("{:?}/{:?}", arg.layout.size, arg.mode))
                        .collect::<Vec<_>>()
                        .join(","),
                    abi.ret.mode
                ),
                Err(error) => format!("unavailable: {error:?}"),
            }
        }
        _ => "unresolvable".into(),
    };

    let symbol_name = match ty::Instance::try_resolve(
        tcx,
        typing_env,
        global,
        ty::GenericArgs::identity_for_item(tcx, global),
    ) {
        Ok(Some(instance)) => tcx.symbol_name(instance).name.to_string(),
        _ => String::new(),
    };

    let attrs = tcx.codegen_fn_attrs(def_id);
    Contract {
        def_kind: format!("{def_kind:?}"),
        fn_sig: stable(&format!(
            "{:?}",
            tcx.fn_sig(global).instantiate_identity().skip_normalization()
        )),
        type_of: stable(&format!("{:?}", tcx.type_of(global).instantiate_identity())),
        generics: stable(&format!("{:?}", tcx.generics_of(global).own_params)),
        predicates: stable(&format!("{:?}", tcx.predicates_of(global).predicates)),
        // Every codegen-relevant attribute in one string: `#[target_feature]`,
        // `#[no_mangle]`, `#[export_name]`, `#[linkage]`, `#[cold]`, the
        // inline hint, and the flag set. A target-feature change alters the
        // instructions rustc may emit and is invisible to the signature.
        codegen_attrs: format!(
            "flags={:?} inline={:?} symbol={:?} section={:?} target_features={:?} \
             linkage={:?} optimize={:?} instruction_set={:?} sanitizers={:?}",
            attrs.flags,
            attrs.inline,
            attrs.symbol_name,
            attrs.link_section,
            attrs
                .target_features
                .iter()
                .map(|f| f.name.to_string())
                .collect::<Vec<_>>(),
            attrs.linkage,
            attrs.optimize,
            attrs.instruction_set,
            attrs.sanitizers,
        ),
        fn_abi: stable(&fn_abi),
        symbol_name,
        mir_observable: ctfe || optimized,
        requires_monomorphization: tcx.generics_of(global).requires_monomorphization(tcx),
        cross_crate_inlinable: tcx.cross_crate_inlinable(def_id),
        is_const_fn: tcx.is_const_fn(global),
        always_const: matches!(
            tcx.constness(def_id),
            rustc_hir::Constness::Const { always: true }
        ),
        is_coroutine: tcx.is_coroutine(global),
        reachable: tcx.reachable_set(()).contains(&def_id),
        inline_attr: format!("{:?}", attrs.inline),
        has_opaque_in_signature: matches!(def_kind, DefKind::Fn | DefKind::AssocFn)
            && mentions_opaque(tcx, global),
        is_associated: matches!(def_kind, DefKind::AssocFn),
        closure_size,
        closure_paths: closure_paths.clone(),
        bodies,
        consts,
        closure_is_local,
        statics: statics.iter().cloned().collect(),
        crate_statics: crate_statics.iter().cloned().collect(),
    }
}

/// The decision, from the two revisions' contracts.
///
/// Ordered so that the *categorical* refusals come before the comparative
/// ones: "this kind of function can never be replaced" is a better
/// explanation than "its ABI string differs", even when both are true.
/// Render a compiler Debug string without the parts that are positions rather
/// than identities.
///
/// `{:?}` on a type prints `DefId(0:7 ~ diff_fixture[b607]::diff_root)`. The
/// `0:7` is a *definition index*, assigned by the order items appear in the
/// crate, so adding any function above `diff_root` renumbers it — and the
/// contract then reports that `diff_root`'s type changed when nothing about it
/// changed at all. The runtime differential found this: `new_local_helper` and
/// `new_generic` were both refused with `field: "type_of"`, quoting two
/// spellings of the same function.
///
/// This is the linker's finding 230 and finding 241 for a third time. A local's
/// name was not an identity; an archive member's name was not an identity; a
/// definition's index is not an identity either. The path is, so the path is
/// what the contract compares.
///
/// The crate's disambiguator hash goes too — it tracks compilation flags, not
/// source — but the crate *name* stays, because two crates with the same item
/// path are genuinely different items.
fn stable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("DefId(") {
        out.push_str(&rest[..at]);
        out.push_str("DefId(");
        let inner = &rest[at + "DefId(".len()..];
        let Some(close) = inner.find(')') else {
            out.push_str(inner);
            return out;
        };
        let body = &inner[..close];
        // Everything after the `~` is the path; everything before it is the
        // index. When there is no `~` the whole body is kept rather than
        // guessed at.
        let path = body.split_once(" ~ ").map(|(_, path)| path).unwrap_or(body);
        let mut scrubbed = String::with_capacity(path.len());
        let mut depth = 0usize;
        for ch in path.chars() {
            match ch {
                '[' => depth += 1,
                ']' if depth > 0 => depth -= 1,
                _ if depth == 0 => scrubbed.push(ch),
                _ => {}
            }
        }
        out.push_str(&scrubbed);
        out.push(')');
        rest = &inner[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod stable_tests {
    use super::stable;

    #[test]
    fn an_index_is_not_an_identity() {
        let before = "FnDef(DefId(0:7 ~ diff_fixture[b607]::diff_root), Binder)";
        let after = "FnDef(DefId(0:9 ~ diff_fixture[b607]::diff_root), Binder)";
        assert_eq!(stable(before), stable(after));
        assert_eq!(stable(before), "FnDef(DefId(diff_fixture::diff_root), Binder)");
    }

    #[test]
    fn a_different_item_is_still_different() {
        assert_ne!(
            stable("DefId(0:7 ~ c[a]::one)"),
            stable("DefId(0:7 ~ c[a]::two)")
        );
    }

    #[test]
    fn text_without_a_def_id_survives_unchanged() {
        assert_eq!(stable("conv=Rust args=[Size(8)/Direct]"), "conv=Rust args=[Size(8)/Direct]");
    }
}

pub fn classify(before: &Contract, after: &Contract) -> Verdict {
    let fallback = |reason| Verdict::Fallback { reason };

    if after.def_kind != "Fn" {
        if after.is_associated {
            return fallback(Reason::AssociatedFn);
        }
        return fallback(Reason::NotAnOrdinaryFn {
            def_kind: after.def_kind.clone(),
        });
    }
    if after.is_coroutine {
        return fallback(Reason::Coroutine);
    }
    if after.is_const_fn || after.always_const {
        return fallback(Reason::ConstEvaluable);
    }
    if after.requires_monomorphization {
        return fallback(Reason::RequiresMonomorphization);
    }
    if after.cross_crate_inlinable {
        return fallback(Reason::CrossCrateInlinable);
    }
    if after.has_opaque_in_signature {
        return fallback(Reason::OpaqueInSignature);
    }
    // The rule this classifier rests on. Reached only when none of the more
    // specific causes above fired, so it is a genuine catch-all rather than
    // the usual answer.
    if after.mir_observable || before.mir_observable {
        return fallback(Reason::CrossCrateMirObservable);
    }
    // `#[inline(never)]` is required rather than merely observed: without it
    // a future rustc is free to inline the body into a caller in another
    // codegen unit, and the replacement would leave that copy behind — a
    // patch that "succeeds" while the old behaviour survives (V2 §10.3).
    if after.inline_attr != "Never" {
        return fallback(Reason::InlineNeverNotEnforced {
            inline_attr: after.inline_attr.clone(),
        });
    }

    let compared: [(&str, &String, &String); 7] = [
        ("fn_sig", &before.fn_sig, &after.fn_sig),
        ("type_of", &before.type_of, &after.type_of),
        ("generics", &before.generics, &after.generics),
        ("predicates", &before.predicates, &after.predicates),
        ("codegen_attrs", &before.codegen_attrs, &after.codegen_attrs),
        ("fn_abi", &before.fn_abi, &after.fn_abi),
        ("symbol_name", &before.symbol_name, &after.symbol_name),
    ];
    for (field, old, new) in compared {
        if old != new {
            return fallback(Reason::ContractChanged {
                field: field.into(),
                before: old.clone(),
                after: new.clone(),
            });
        }
    }

    if !after.closure_is_local {
        return fallback(Reason::ClosureNotLocal);
    }
    // Checked after the contract comparison and before the verdict, because a
    // new static is the one downstream requirement that is invisible to every
    // field above: the signature, the ABI and the symbol name are all
    // unchanged, and the program still needs storage that does not exist.
    let fresh: Vec<String> = after
        .statics
        .iter()
        .filter(|name| !before.crate_statics.contains(*name))
        .cloned()
        .collect();
    if !fresh.is_empty() {
        return fallback(Reason::NewStaticRequired { statics: fresh });
    }

    // A live patch replaces the closure. Anything outside it that changed in
    // the same revision keeps its old compiled code in the base image, and the
    // program then behaves like neither revision.
    //
    // Every check above this one is about the hot root, and that is exactly
    // why none of them could see it: `edit_outside_closure` changes a function
    // the root never mentions, so the signature, ABI, symbol name, attributes
    // and MIR observability are all bit-identical across the two revisions.
    // The runtime differential caught it returning 7 where a clean rebuild
    // returned 8, which is the first thing that suite found that nothing else
    // could have.
    if after.bodies.is_empty() || after.bodies.values().any(|(hash, _)| hash.is_empty()) {
        // Fail closed. An unavailable fingerprint compares equal to another
        // unavailable fingerprint, so a missing one would make this check pass
        // by being unable to run.
        return fallback(Reason::BodiesUnavailable);
    }
    // §48, before the body check because it is the stricter rule and its
    // failure mode is the one nothing else can see.
    // Only consts that existed before and have a different definition now.
    //
    // Not additions: a `const` the previous revision did not have cannot have
    // been folded into anything in the base image, so a patch that introduces
    // one and reads it is exactly as safe as a patch that introduces a helper.
    // The first version refused those too and turned §46's `const_table` — a
    // mutation whose whole purpose is to add a constant and read it — into a
    // FALLBACK, which took the corrupted-constant control with it.
    //
    // Not removals either. A base-image function that folded the old value
    // keeps a value that was correct when it was compiled; if anything still
    // *refers* to the const, the crate does not compile and no verdict is
    // reached at all.
    let mut changed_consts: Vec<String> = Vec::new();
    for (id, (hash, name)) in &after.consts {
        if let Some((was, _)) = before.consts.get(id) {
            if was != hash {
                changed_consts.push(name.clone());
            }
        }
    }
    if !changed_consts.is_empty() {
        changed_consts.sort();
        changed_consts.dedup();
        return fallback(Reason::ConstChanged { consts: changed_consts });
    }

    let mut outside: Vec<String> = Vec::new();
    let trace = std::env::var_os("SPIKE_TRACE_BODIES").is_some();
    for (id, (hash, name)) in &after.bodies {
        let unchanged = before.bodies.get(id).map(|(hash, _)| hash) == Some(hash);
        if trace && !unchanged {
            // Kept, rather than deleted once §45 was understood. The question
            // "which body does the compiler think changed, and to what" has now
            // had to be asked twice, and both times the answer overturned an
            // assumption; an instrument that has done that is worth its four
            // lines.
            eprintln!(
                "  trace {name}\n    before {}\n    after  {hash}",
                before.bodies.get(id).map(|(h, _)| h.as_str()).unwrap_or("<absent>")
            );
        }
        if !unchanged && !after.closure_paths.contains(id) {
            outside.push(name.clone());
        }
    }
    // A function that existed and no longer does is also a change outside the
    // closure — the base image still contains it, and still calls it.
    for (id, (_, name)) in &before.bodies {
        if !after.bodies.contains_key(id) && !after.closure_paths.contains(id) {
            outside.push(name.clone());
        }
    }
    if !outside.is_empty() {
        outside.sort();
        outside.dedup();
        return fallback(Reason::ChangedOutsideClosure { functions: outside });
    }
    Verdict::Direct
}
