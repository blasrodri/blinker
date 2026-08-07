//! The semantic code graph the agent API answers from.
//!
//! Every question in the agent protocol that is not "run this" is a question
//! about the program's structure: what is this function, what does it call, who
//! calls it, where does its body start and stop. An agent that has to answer
//! those by reading files and grepping is doing textual archaeology on a
//! language with modules, traits, method resolution and macros; the compiler
//! that is already resident knows the answers exactly.
//!
//! So this is built from rustc's own data — `optimized_mir`'s call terminators
//! for the edges, HIR spans for the byte ranges — and it uses `DefPathHash` as
//! the identity throughout, for the same reason §37 does: it is the identity
//! rustc designed to survive across sessions, and this index has to survive
//! across exactly that boundary.
//!
//! Not on the edit → active path
//! -----------------------------
//!
//! Building it means forcing `optimized_mir` for every function in the crate,
//! which is real work the revision path does not otherwise do — Path D forces
//! it for four to twelve instances, not for nine hundred. If this were built in
//! `after_analysis` on every revision it would land in front of the sink and
//! inflate the number §34 spent three sections making honest.
//!
//! It is therefore built only when asked for. A `replace_body` does not ask:
//! the resident workspace knows the byte delta of the edit it just made and
//! shifts every span in the same file arithmetically, which is exact. What it
//! cannot know is whether the edit changed the *call graph*, so it marks the
//! edited function's edges stale, and the next question that needs them pays
//! for a refresh and says so.

use rustc_hir::def::DefKind;
use rustc_middle::mir::TerminatorKind;
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::def_id::LocalDefId;

/// One function, as the agent sees it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Function {
    /// `DefPathHash`. The identity, everywhere (§37).
    pub id: String,
    /// The last path segment, which is what an agent will type.
    pub name: String,
    /// The def path without the crate prefix, to disambiguate two `scan`s.
    pub path: String,
    pub signature: String,
    pub file: String,
    pub line: usize,
    /// Byte offsets of the body *block*, `{` through `}` inclusive. This is
    /// what `replace_body` splices, and taking the block rather than its
    /// interior means the API structurally cannot change a signature — which
    /// happens to be the largest single FALLBACK class.
    pub body_start: usize,
    pub body_end: usize,
    /// Callees defined in this crate, by `DefPathHash`.
    pub calls: Vec<String>,
    /// Whether the edges above were rebuilt for the current source. False
    /// after a `replace_body` until a refresh.
    pub edges_fresh: bool,
    pub inline_never: bool,
    /// The symbol the backend gives it, for finding it in a patch or an image.
    pub symbol: String,
    /// Whether a probe can call it, and with what.
    pub probe: Option<Probe>,
    /// `extern "C" fn() -> u64`, named `test_…`: the suite `run_affected` picks
    /// from. Declared by shape rather than by attribute because `#[test]`
    /// bodies only exist under a test harness rustc is not building here.
    pub is_test: bool,
}

/// A callable signature, in the one shape a probe can construct.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Probe {
    /// Width in bits and signedness of each parameter, in order.
    pub params: Vec<Scalar>,
    pub ret: Scalar,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Scalar {
    pub bits: u32,
    pub signed: bool,
    /// A raw pointer to bytes. The probe fills these from its `bytes` argument
    /// rather than from `args`.
    ///
    /// Without this a probe can only call functions whose arguments are all
    /// numbers, and the functions worth interrogating in a real crate — parsers,
    /// scanners, decoders — take a buffer. The first version refused `scan` on
    /// exactly this ground and the gate had nothing to ask a question of.
    pub buffer: bool,
}

impl Scalar {
    /// Narrow a 64-bit register to what the declared type actually holds.
    ///
    /// AAPCS leaves the high bits of a register holding a narrow value
    /// unspecified, so a `u32`-returning function may leave anything above bit
    /// 31 in `x0`. Reading the raw register would make a correct patch look
    /// like it returned a different number from the clean rebuild on some runs
    /// and not others, which is the worst kind of differential failure.
    pub fn narrow(self, raw: u64) -> i64 {
        if self.bits >= 64 {
            return raw as i64;
        }
        let masked = raw & ((1u64 << self.bits) - 1);
        if self.signed && masked & (1 << (self.bits - 1)) != 0 {
            (masked as i64) - (1i64 << self.bits)
        } else {
            masked as i64
        }
    }

    /// The same in reverse, for an argument.
    pub fn widen(self, value: i64) -> u64 {
        if self.bits >= 64 {
            return value as u64;
        }
        (value as u64) & ((1u64 << self.bits) - 1)
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Index {
    pub functions: Vec<Function>,
    /// What it cost to build, reported rather than hidden: this is the price of
    /// the semantic answers and an agent protocol that pretends its index is
    /// free is one nobody can budget for.
    pub build_ms: f64,
}

impl Index {
    pub fn find(&self, name: &str) -> Option<&Function> {
        // An exact path first, then an unambiguous last segment. Ambiguity is
        // reported by the caller rather than resolved by picking one.
        self.functions
            .iter()
            .find(|f| f.path == name || f.id == name)
            .or_else(|| {
                let mut matches = self.functions.iter().filter(|f| f.name == name);
                let first = matches.next()?;
                matches.next().is_none().then_some(first)
            })
    }

    pub fn ambiguous(&self, name: &str) -> Vec<String> {
        if self.functions.iter().any(|f| f.path == name || f.id == name) {
            return Vec::new();
        }
        let paths: Vec<String> = self
            .functions
            .iter()
            .filter(|f| f.name == name)
            .map(|f| f.path.clone())
            .collect();
        if paths.len() > 1 { paths } else { Vec::new() }
    }

    pub fn by_id(&self, id: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.id == id)
    }

    /// Everything that calls `id`, directly.
    pub fn callers(&self, id: &str) -> Vec<&Function> {
        self.functions.iter().filter(|f| f.calls.iter().any(|c| c == id)).collect()
    }

    /// Everything `root` reaches, transitively, including itself.
    ///
    /// Over the `DefId` graph rather than over monomorphized instances, so it
    /// over-approximates Path D: one generic function stands for all of its
    /// instantiations. For choosing which tests to run that is the safe
    /// direction — a superset runs tests that did not need to run, where a
    /// subset silently skips the one that would have failed.
    pub fn reaches(&self, root: &str) -> std::collections::BTreeSet<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut work = vec![root.to_string()];
        while let Some(id) = work.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(function) = self.by_id(&id) {
                work.extend(function.calls.iter().cloned());
            }
        }
        seen
    }

    /// Shift every span in `file` that starts at or after `at` by `delta`.
    ///
    /// Called after a body splice, and it is exact: the workspace wrote the
    /// bytes itself, so it knows precisely where the file changed and by how
    /// much. Nothing before the splice point moves; everything after it moves
    /// by the same amount. Recompiling to rediscover that would cost a session
    /// to learn arithmetic.
    pub fn shift(&mut self, file: &str, at: usize, delta: isize) {
        for function in &mut self.functions {
            if function.file != file {
                continue;
            }
            let move_by = |offset: &mut usize| {
                if *offset >= at {
                    *offset = offset.saturating_add_signed(delta);
                }
            };
            move_by(&mut function.body_start);
            move_by(&mut function.body_end);
        }
    }
}

/// Build the index for the crate under `tcx`.
pub fn build(tcx: TyCtxt<'_>) -> Index {
    let at = std::time::Instant::now();
    let source_map = tcx.sess.source_map();
    let typing_env = ty::TypingEnv::fully_monomorphized();

    let mut functions = Vec::new();
    for def_id in tcx.hir_crate_items(()).definitions() {
        if !matches!(tcx.def_kind(def_id), DefKind::Fn | DefKind::AssocFn) {
            continue;
        }
        if !tcx.is_mir_available(def_id.to_def_id()) {
            continue;
        }
        let global = def_id.to_def_id();

        // The body block's byte range within its own file. `lookup_byte_offset`
        // gives the containing `SourceFile` and a position relative to it,
        // which is what an editor — and a splice — needs; a raw `BytePos` is an
        // offset into rustc's global source map and means nothing on disk.
        let body = tcx.hir_body_owned_by(def_id);
        let span = body.value.span;
        let lo = source_map.lookup_byte_offset(span.lo());
        let hi = source_map.lookup_byte_offset(span.hi());
        let file = lo.sf.name.prefer_local_unconditionally().to_string();
        // A body whose two ends landed in different source files came from a
        // macro. Splicing it would write into whichever file the start happened
        // to be in, so it is recorded without a range rather than with a wrong
        // one, and `replace_body` refuses it by name.
        let (body_start, body_end) = if lo.sf.start_pos == hi.sf.start_pos {
            (lo.pos.0 as usize, hi.pos.0 as usize)
        } else {
            (0, 0)
        };

        let signature = tcx.fn_sig(global).instantiate_identity().skip_normalization();
        let signature = tcx.instantiate_bound_regions_with_erased(signature);
        let rendered = format!(
            "fn({}) -> {}",
            signature.inputs().iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", "),
            signature.output()
        );

        let instance = ty::Instance::try_resolve(
            tcx,
            typing_env,
            global,
            ty::GenericArgs::identity_for_item(tcx, global),
        );
        let (symbol, calls) = match instance {
            Ok(Some(instance)) => (
                tcx.symbol_name(instance).name.to_string(),
                callees(tcx, def_id),
            ),
            // Generic, so there is no one instance and no one symbol. Its edges
            // still matter for reachability, and `instance_mir` is not needed to
            // read them — `optimized_mir` on the definition has the same calls.
            _ => (String::new(), callees(tcx, def_id)),
        };

        let attrs = tcx.codegen_fn_attrs(def_id);
        let probe = probe_signature(&signature);
        let name = tcx.item_name(global).to_string();
        functions.push(Function {
            id: format!("{:?}", tcx.def_path_hash(global)),
            is_test: name.starts_with("test_") && is_test_signature(&signature),
            name,
            path: tcx.def_path(global).to_string_no_crate_verbose(),
            signature: rendered,
            file,
            line: source_map.lookup_char_pos(tcx.def_span(global).lo()).line,
            body_start,
            body_end,
            calls,
            edges_fresh: true,
            inline_never: attrs.inline == rustc_hir::attrs::InlineAttr::Never,
            symbol,
            probe,
        });
    }
    functions.sort_by(|a, b| a.path.cmp(&b.path));

    Index { functions, build_ms: at.elapsed().as_secs_f64() * 1e3 }
}

/// The functions this one calls, by `DefPathHash`, local definitions only.
fn callees(tcx: TyCtxt<'_>, def_id: LocalDefId) -> Vec<String> {
    let body = tcx.optimized_mir(def_id.to_def_id());
    let mut out = Vec::new();
    for block in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else {
            continue;
        };
        let TerminatorKind::Call { func, .. } = &terminator.kind else {
            continue;
        };
        let ty::FnDef(callee, _) = *func.ty(&body.local_decls, tcx).kind() else {
            continue;
        };
        // Local only. An edge to another crate is real but nothing in this
        // index can answer questions about the other end of it, and a graph
        // whose nodes are half absent invites a caller to conclude that a
        // function has no callers when it has one it cannot see.
        if callee.is_local() {
            out.push(format!("{:?}", tcx.def_path_hash(callee)));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Whether a probe can call this function, and how.
///
/// Deliberately narrow. A probe transmutes an address to a function pointer and
/// calls it, so the set of signatures it may accept is the set whose calling
/// convention this code can construct without guessing: `extern "C"`, integer
/// scalars only, at most six arguments — which on AAPCS is the register-passed
/// prefix, so nothing lands on the stack and nothing is passed indirectly.
///
/// Everything else is refused by name rather than attempted. A probe that got
/// this wrong would not fail loudly; it would return a plausible number, which
/// is the failure mode §36 caught when slot 0 turned out to be the wrong
/// function.
fn probe_signature(signature: &ty::FnSig<'_>) -> Option<Probe> {
    if !matches!(signature.abi(), rustc_abi::ExternAbi::C { .. }) {
        return None;
    }
    if signature.c_variadic() || signature.inputs().len() > 6 {
        return None;
    }
    let params: Option<Vec<Scalar>> = signature.inputs().iter().map(|t| scalar(*t)).collect();
    // A pointer *out* would hand an agent an address to do arithmetic on, and
    // the only thing it could do with one is get it wrong.
    if signature.output().is_raw_ptr() {
        return None;
    }
    Some(Probe {
        params: params?,
        // A `()` return is reported as a zero-bit scalar rather than refused: a
        // test that returns nothing is still worth calling, and narrowing to
        // zero bits yields 0 rather than whatever was left in the register.
        ret: if signature.output().is_unit() {
            Scalar::default()
        } else {
            scalar(signature.output())?
        },
    })
}

/// Whether this is a function `run_affected` knows how to call and score.
///
/// `extern "C" fn(*mut u64) -> u64`: the test writes what it expected through
/// the out-parameter and returns what it got, and it passes when the two agree.
///
/// Both numbers, rather than a pass/fail code, because the observation an agent
/// acts on is `{"expected": 167, "actual": 162}` and a boolean forces it to go
/// find the difference some other way — which in a conventional harness means
/// reading test output, the exact thing this API exists to avoid.
///
/// Declared by shape rather than by `#[test]`, because `#[test]` bodies only
/// exist under a harness rustc is not building here, and a test that is only
/// real in a different compilation is not a test this can run.
fn is_test_signature(signature: &ty::FnSig<'_>) -> bool {
    use rustc_middle::ty::{Mutability, UintTy};
    if !matches!(signature.abi(), rustc_abi::ExternAbi::C { .. }) || signature.inputs().len() != 1 {
        return false;
    }
    let out = matches!(
        signature.inputs()[0].kind(),
        ty::RawPtr(pointee, Mutability::Mut) if pointee.is_integral()
    );
    out && matches!(signature.output().kind(), ty::Uint(UintTy::U64))
}

fn scalar(ty: ty::Ty<'_>) -> Option<Scalar> {
    use rustc_middle::ty::{IntTy, UintTy};
    let of = |bits, signed| Scalar { bits, signed, buffer: false };
    Some(match ty.kind() {
        ty::Bool => of(8, false),
        ty::Uint(UintTy::U8) => of(8, false),
        ty::Uint(UintTy::U16) => of(16, false),
        ty::Uint(UintTy::U32) => of(32, false),
        ty::Uint(UintTy::U64 | UintTy::Usize) => of(64, false),
        ty::Int(IntTy::I8) => of(8, true),
        ty::Int(IntTy::I16) => of(16, true),
        ty::Int(IntTy::I32) => of(32, true),
        ty::Int(IntTy::I64 | IntTy::Isize) => of(64, true),
        // A pointer to bytes, and only to bytes. Widening this to any pointee
        // would mean a probe constructing a `&Config` out of an integer, which
        // is a way to crash the resident process on a typo.
        ty::RawPtr(pointee, _) if matches!(pointee.kind(), ty::Uint(UintTy::U8)) => {
            Scalar { bits: 64, signed: false, buffer: true }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_reads_only_the_declared_width() {
        let u32 = Scalar { bits: 32, signed: false, buffer: false };
        // The high half is what AAPCS leaves unspecified.
        assert_eq!(u32.narrow(0xDEAD_BEEF_0000_002A), 42);
        let i32 = Scalar { bits: 32, signed: true, buffer: false };
        assert_eq!(i32.narrow(0xFFFF_FFFF_FFFF_FFFF), -1);
        assert_eq!(Scalar { bits: 64, signed: true, buffer: false }.narrow(u64::MAX), -1);
        assert_eq!(Scalar::default().narrow(u64::MAX), 0);
    }

    #[test]
    fn the_spliced_function_moves_exactly_once() {
        // The regression Agent Bench found. A body at [10, 20) grown by 5 ends
        // at 25, not 30 — and it is `shift` that moves it, because 20 is at the
        // splice point rather than before it. Adjusting it again afterwards put
        // the next edit five bytes into the following item.
        let mut index = Index {
            functions: vec![Function {
                file: "a".into(),
                body_start: 10,
                body_end: 20,
                ..Default::default()
            }],
            build_ms: 0.0,
        };
        index.shift("a", 20, 5);
        assert_eq!((index.functions[0].body_start, index.functions[0].body_end), (10, 25));
    }

    #[test]
    fn shift_moves_only_what_follows_the_splice() {
        let mut index = Index {
            functions: vec![
                Function { file: "a".into(), body_start: 10, body_end: 20, ..Default::default() },
                Function { file: "a".into(), body_start: 30, body_end: 40, ..Default::default() },
                Function { file: "b".into(), body_start: 30, body_end: 40, ..Default::default() },
            ],
            build_ms: 0.0,
        };
        index.shift("a", 25, 5);
        assert_eq!((index.functions[0].body_start, index.functions[0].body_end), (10, 20));
        assert_eq!((index.functions[1].body_start, index.functions[1].body_end), (35, 45));
        assert_eq!((index.functions[2].body_start, index.functions[2].body_end), (30, 40));
    }

    #[test]
    fn reaches_is_transitive_and_terminates_on_cycles() {
        let f = |id: &str, calls: &[&str]| Function {
            id: id.into(),
            calls: calls.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let index = Index {
            functions: vec![f("a", &["b"]), f("b", &["c", "a"]), f("c", &[]), f("d", &[])],
            build_ms: 0.0,
        };
        assert_eq!(index.reaches("a"), ["a", "b", "c"].map(String::from).into_iter().collect());
        assert!(!index.reaches("c").contains("a"));
    }

    #[test]
    fn a_bare_name_that_matches_two_paths_is_ambiguous_rather_than_first() {
        let f = |path: &str| Function {
            name: "scan".into(),
            path: path.into(),
            ..Default::default()
        };
        let index = Index { functions: vec![f("::a::scan"), f("::b::scan")], build_ms: 0.0 };
        assert!(index.find("scan").is_none());
        assert_eq!(index.ambiguous("scan").len(), 2);
        assert!(index.find("::a::scan").is_some());
        assert!(index.ambiguous("::a::scan").is_empty());
    }
}
