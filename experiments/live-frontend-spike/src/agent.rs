//! The agent-facing semantic API (M1).
//!
//! What this replaces
//! ------------------
//!
//! An agent fixing a Rust bug with conventional tools spends almost all of its
//! wall clock and most of its tokens on three things: reading files to find out
//! what the program is, running `cargo` to find out whether a change compiles,
//! and running a test binary to find out what the program *does*. The first is
//! archaeology over a language whose structure the compiler already knows. The
//! second and third are the same 400–800 ms rebuild §43 measures, paid once per
//! hypothesis.
//!
//! So the surface here is deliberately not a shell. It is seven verbs:
//!
//! | verb | question it answers |
//! |---|---|
//! | `inspect` | what is this function — signature, body, where it lives |
//! | `callers` | who calls it |
//! | `replace_body` | make this the new body; is that publishable |
//! | `probe` | what does this function actually return for these arguments |
//! | `run_affected` | which tests reach the change, and do they pass |
//! | `commit` | make the candidate the program |
//! | `rollback` | put the previous generation back |
//!
//! Every one answers with a small flat object. That shape is a decision, not a
//! convenience: an agent that has to parse a compiler's prose to learn whether
//! something worked spends tokens on the parsing and gets it wrong under
//! ambiguity, and an observation with a `status` field does not have ambiguity
//! to get wrong.
//!
//! Candidate, then commit
//! ----------------------
//!
//! `replace_body` never publishes. It compiles, classifies and stages, and what
//! comes back is a candidate that `Runtime::current` has never heard of — §40's
//! invariant, exposed rather than hidden. `probe` and `run_affected` will call
//! *into the candidate* by address, so an agent can find out what a change does
//! before any of it becomes the program.
//!
//! That is the property that makes this an experimentation surface rather than
//! a fast deploy button, and it is the thing M5's speculative branching needs to
//! exist at all: several candidates can be built and measured, and at most one
//! of them ever has to become real.
//!
//! What it refuses
//! ---------------
//!
//! Everything §43 refuses, unchanged — this adds no new DIRECT class and is not
//! allowed to. It also refuses to splice a body it cannot locate exactly (a
//! macro-generated one, whose two ends live in different files), and to probe a
//! signature it cannot construct without guessing.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::arena::Arena;
use crate::generation::Runtime;
use crate::index::Index;
use crate::live::Staged;

/// One request. Exactly the seven verbs, plus the three that manage a session.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    /// Restore the pristine source, load a base image, build the index.
    Open,
    Inspect { symbol: String },
    Callers { symbol: String },
    ReplaceBody { symbol: String, body: String },
    Probe {
        symbol: String,
        #[serde(default)]
        args: Vec<i64>,
        /// Bytes for the function's buffer parameter, if it has one. The
        /// corresponding entry in `args` is ignored — an agent has no way to
        /// know an address and should not be inventing one.
        #[serde(default)]
        bytes: Option<String>,
    },
    RunAffected,
    Commit,
    Rollback,
    /// The index, rebuilt against the source as it is now.
    Reindex,
    Status,
    Quit,
}

/// One observation.
///
/// Flat, and almost entirely optional: a verb answers with the fields it has
/// something to say about and omits the rest, so `inspect` is four fields and
/// `run_affected` is five rather than both being thirty with nulls in them.
#[derive(Debug, Default, serde::Serialize)]
pub struct Observation {
    /// `ok` · `staged` · `active` · `refused` · `rolled_back` · `error`.
    pub status: &'static str,
    /// What this call cost, end to end, in this process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Why a refusal, in the classifier's own vocabulary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned: Option<i64>,
    /// `candidate` · `generation` · `image`. Which copy of the function
    /// answered.
    ///
    /// Never inferable and never omissible. A probe that hits the base image
    /// after a patch has been staged is reporting the *old* behaviour, and that
    /// is a true and useful answer — it is what every unpatched caller still
    /// sees — but an agent that mistook it for the new one would conclude its
    /// edit had done nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// How many instances the change reaches — Path D's answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closure: Option<usize>,
    /// How many functions the semantic graph holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<usize>,
    /// Of a `reindex`, the part that was building the graph rather than
    /// compiling the crate. Reported apart because they are different costs
    /// with different futures: the session is rustc's and the graph is this
    /// code's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_ms: Option<f64>,
    /// Set once a FALLBACK has happened, per §42.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub needs_rebase: bool,
    /// The cost of a baseline or index session this call had to run first,
    /// reported apart from `latency_ms` so an agent can tell a first edit to a
    /// function from a subsequent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct Failure {
    pub test: String,
    pub expected: i64,
    pub actual: i64,
}

impl Observation {
    fn ok() -> Observation {
        Observation { status: "ok", ..Default::default() }
    }

    fn error(message: impl Into<String>) -> Observation {
        Observation {
            status: "error",
            error: Some(message.into()),
            ..Default::default()
        }
    }
}

/// A base image: the fixture compiled and linked the ordinary way.
///
/// The program the patches are patches *of*. Without one there is nothing for a
/// probe to answer with before the first edit, and — more importantly — nothing
/// for the closure to be a *subset* of. A live patch whose base image is itself
/// is not a live patch.
struct BaseImage {
    handle: *mut libc::c_void,
}

impl BaseImage {
    fn build(fixture: &crate::Fixture, source: &Path, out: &Path) -> Result<BaseImage, String> {
        let output = std::process::Command::new("rustc")
            .args([
                "--edition=2021",
                                // A Rust `dylib`, not a `cdylib`.
                //
                // A `cdylib` exports only the C surface — everything else,
                // including the parts of `core` linked into it, gets local
                // visibility. So a patch carrying a panic path could not
                // resolve `core::panicking::panic_const_div_by_zero`: the code
                // was right there in the image, at a `t` symbol `dlsym` cannot
                // see. `-Wl,-export_dynamic` does not help, because the hiding
                // happens in codegen rather than at link time.
                //
                // A stand-in either way, and worth naming as one: in the
                // product the base image is the developer's own binary and
                // Blinker *is* its linker, so it holds the address of every
                // symbol whether or not the dynamic table does. Choosing a
                // crate type that keeps them visible is how a harness without
                // a linker gets the same answer.
                "--crate-type=dylib",
                "-Copt-level=0",
                "-Cdebuginfo=0",
                "-Cdebug-assertions=off",
                // Pinned, and pinned to the same value the spike's own session
                // uses for this fixture. The crate disambiguator feeds every
                // mangled symbol, so two compilations of one source that
                // disagree about it produce patches whose references to base
                // image symbols are spelled differently and do not resolve.
                "-Cmetadata=agent",
                "--cap-lints=allow",
            ])
            .arg(format!("--crate-name={}", fixture.crate_name))
            .arg(source)
            .arg("-o")
            .arg(out)
            .output()
            .map_err(|e| format!("cannot run rustc: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "the base image did not build: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let c = std::ffi::CString::new(out.as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())?;
        // SAFETY: a path this process just wrote.
        let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("cannot load {}", out.display()));
        }
        Ok(BaseImage { handle })
    }

    fn find(&self, symbol: &str) -> Option<*const u8> {
        let bare = symbol.strip_prefix('_').unwrap_or(symbol);
        let c = std::ffi::CString::new(bare).ok()?;
        // SAFETY: an open handle and a null-terminated name.
        let address = unsafe { libc::dlsym(self.handle, c.as_ptr()) };
        (!address.is_null()).then(|| address as *const u8)
    }
}

impl Drop for BaseImage {
    fn drop(&mut self) {
        // SAFETY: a handle this process opened and has not closed.
        unsafe { libc::dlclose(self.handle) };
    }
}

/// The resident workspace: one process, one crate, many revisions.
///
/// It holds what is genuinely session-spanning — the arena, the generation
/// table, the base image, the index, and the contracts already derived — and
/// rebuilds a `TyCtxt` per revision, for the reason §38 gives: rustc's
/// `Compiler` is one session by construction and pretending otherwise is a much
/// larger correctness surface than the milliseconds it might save.
pub struct Workspace {
    arena: Arena,
    runtime: Runtime,
    fixture: crate::Fixture,
    target_file: PathBuf,
    incremental: PathBuf,
    out_dir: PathBuf,
    backend: Vec<String>,
    pristine: PathBuf,

    image: Option<BaseImage>,
    index: Index,
    /// The last contract derived for each function, by def path. The *compiler*
    /// predecessor, per §42: it is updated after every session, accepted or
    /// refused.
    contracts: std::collections::BTreeMap<String, crate::classify::Contract>,
    /// A candidate nothing can reach.
    staged: Option<Staged>,
    staged_entries: Vec<(String, usize)>,
    /// The entry table of each committed generation, innermost last, so a
    /// rollback restores addresses as well as the pointer.
    committed: Vec<(u64, Vec<(String, usize)>)>,
    /// The functions edited since the last commit or rollback.
    ///
    /// A set, not the most recent one. It was `Option<String>`, so a revision
    /// that changed two functions selected only the tests reaching the
    /// *second* — and Agent Bench found the consequence, which is worse than
    /// under-testing: `run_affected` reported zero failures while tests
    /// reaching the first function were still failing, so the agent committed
    /// and stopped. The suite on disk disagreed.
    editing: std::collections::BTreeSet<String>,
    needs_rebase: bool,
    /// The most recent probe's byte buffer, kept alive by the workspace rather
    /// than by the call that made it.
    buffer: Vec<u8>,
}

impl Workspace {
    pub fn new(options: &crate::Options) -> Workspace {
        let backend = options
            .backend
            .as_ref()
            .map(|path| format!("-Zcodegen-backend={}", path.display()))
            .unwrap_or_else(|| "-Zcodegen-backend=cranelift".into());
        Workspace {
            arena: Arena::reserve(256 * 1024 * 1024).expect("arena"),
            runtime: Runtime::new(64),
            fixture: options.fixture.clone(),
            target_file: options.target_file.clone(),
            incremental: options.incremental.clone(),
            out_dir: options.incremental.clone(),
            backend: vec![backend],
            pristine: options.variants.join("pristine.rs"),
            image: None,
            index: Index::default(),
            contracts: std::collections::BTreeMap::new(),
            staged: None,
            staged_entries: Vec::new(),
            committed: Vec::new(),
            editing: std::collections::BTreeSet::new(),
            needs_rebase: false,
            buffer: Vec::new(),
        }
    }

    fn handle(&mut self, request: Request) -> Observation {
        match request {
            Request::Open => self.open(),
            Request::Reindex => self.reindex(),
            Request::Inspect { symbol } => self.inspect(&symbol),
            Request::Callers { symbol } => self.callers(&symbol),
            Request::ReplaceBody { symbol, body } => self.replace_body(&symbol, &body),
            Request::Probe { symbol, args, bytes } => self.probe(&symbol, &args, bytes.as_deref()),
            Request::RunAffected => self.run_affected(),
            Request::Commit => self.commit(),
            Request::Rollback => self.rollback(),
            Request::Status => self.status(),
            Request::Quit => Observation::ok(),
        }
    }

    fn open(&mut self) -> Observation {
        let at = Instant::now();
        let source = match std::fs::read_to_string(&self.pristine) {
            Ok(source) => source,
            Err(error) => return Observation::error(format!("no pristine variant: {error}")),
        };
        if let Err(error) = std::fs::write(&self.target_file, &source) {
            return Observation::error(format!("cannot write the target file: {error}"));
        }
        let image_path = self.out_dir.join("agent_base.dylib");
        match BaseImage::build(&self.fixture, &self.target_file, &image_path) {
            Ok(image) => self.image = Some(image),
            Err(error) => return Observation::error(error),
        }
        self.staged = None;
        self.staged_entries.clear();
        self.committed.clear();
        self.contracts.clear();
        self.editing.clear();
        self.needs_rebase = false;

        let mut observation = self.reindex();
        observation.latency_ms = Some(at.elapsed().as_secs_f64() * 1e3);
        observation
    }

    fn reindex(&mut self) -> Observation {
        let at = Instant::now();
        let (index, _, _, error) = crate::live::index_session(
            &self.fixture,
            &self.target_file,
            &self.incremental,
            &self.fixture.hot,
            &self.backend,
        );
        let Some(index) = index else {
            return Observation::error(
                error.unwrap_or_else(|| "the session produced no index".into()),
            );
        };
        self.index = index;
        Observation {
            latency_ms: Some(at.elapsed().as_secs_f64() * 1e3),
            index_ms: Some(self.index.build_ms),
            tests: Some(self.index.functions.iter().filter(|f| f.is_test).count()),
            functions: Some(self.index.functions.len()),
            ..Observation::ok()
        }
    }

    fn inspect(&mut self, symbol: &str) -> Observation {
        let at = Instant::now();
        let source = match std::fs::read_to_string(&self.target_file) {
            Ok(source) => source,
            Err(error) => return Observation::error(error.to_string()),
        };
        let ambiguous = self.index.ambiguous(symbol);
        let Some(function) = self.index.find(symbol) else {
            return if ambiguous.is_empty() {
                Observation::error(format!("no function named {symbol}"))
            } else {
                Observation {
                    callers: ambiguous,
                    ..Observation::error(format!("{symbol} names more than one function"))
                }
            };
        };
        let body = source.get(function.body_start..function.body_end).map(str::to_string);
        let calls = function
            .calls
            .iter()
            .filter_map(|id| self.index.by_id(id))
            .map(|f| f.path.clone())
            .collect();
        Observation {
            latency_ms: Some(at.elapsed().as_secs_f64() * 1e3),
            symbol: Some(function.path.clone()),
            signature: Some(function.signature.clone()),
            file: Some(function.file.clone()),
            line: Some(function.line),
            body,
            calls,
            ..Observation::ok()
        }
    }

    fn callers(&mut self, symbol: &str) -> Observation {
        let at = Instant::now();
        let Some(function) = self.index.find(symbol) else {
            return Observation::error(format!("no function named {symbol}"));
        };
        let (id, path) = (function.id.clone(), function.path.clone());
        let callers = self.index.callers(&id).iter().map(|f| f.path.clone()).collect();
        Observation {
            latency_ms: Some(at.elapsed().as_secs_f64() * 1e3),
            symbol: Some(path),
            callers,
            ..Observation::ok()
        }
    }

    /// Splice a new body in, compile it, classify it, stage it. Publish nothing.
    fn replace_body(&mut self, symbol: &str, body: &str) -> Observation {
        let Some(function) = self.index.find(symbol) else {
            return Observation::error(format!("no function named {symbol}"));
        };
        let function = function.clone();
        if function.body_start == 0 && function.body_end == 0 {
            return Observation::error(format!(
                "{}'s body does not occupy a contiguous range of one file — it came from a \
                 macro, and splicing it would write into the wrong place",
                function.path
            ));
        }
        let source = match std::fs::read_to_string(&self.target_file) {
            Ok(source) => source,
            Err(error) => return Observation::error(error.to_string()),
        };
        if source.len() < function.body_end {
            return Observation::error("the index is older than the file it describes");
        }

        // A baseline for this function, if this is the first edit to it. One
        // session, paid once per function, reported apart from the edit's own
        // latency so that the two are never confused for each other.
        let mut setup_ms = 0.0;
        if !self.contracts.contains_key(&function.path) {
            let (_, contract, ms, error) = crate::live::index_session(
                &self.fixture,
                &self.target_file,
                &self.incremental,
                &function.name,
                &self.backend,
            );
            setup_ms = ms;
            match contract {
                Some(contract) => {
                    self.contracts.insert(function.path.clone(), contract);
                }
                None => {
                    return Observation {
                        setup_ms: Some(setup_ms),
                        ..Observation::error(
                            error.unwrap_or_else(|| "no baseline contract".into()),
                        )
                    }
                }
            }
        }

        // The splice. The block including its braces, so the signature is not
        // reachable from here — an API that cannot express a signature change
        // cannot accidentally make one, and a signature change is the largest
        // single FALLBACK class.
        let mut edited = String::with_capacity(source.len() + body.len());
        edited.push_str(&source[..function.body_start]);
        edited.push_str(body);
        edited.push_str(&source[function.body_end..]);
        let delta = body.len() as isize - (function.body_end - function.body_start) as isize;

        // Every test as an extra codegen root, not only the ones the index
        // calls affected.
        //
        // Selection happens later, in `run_affected`, against the graph. Doing
        // it *here* as well would be selecting with a graph that describes the
        // source before this edit — and an edit whose whole purpose is to
        // change what a function calls is exactly the case where the two
        // disagree. Generating them all costs codegen proportional to the
        // suite's reachable set, which is the honest limit to name for M2, and
        // it removes the staleness window entirely.
        let roots: Vec<String> = self
            .index
            .functions
            .iter()
            .filter(|f| f.is_test)
            .map(|f| f.name.clone())
            .collect();

        let before = self.contracts.get(&function.path).cloned();
        let (record, staged) = crate::live::sink_candidate(
            &self.arena,
            &self.runtime,
            &self.fixture,
            &self.target_file,
            &self.incremental,
            &function.name,
            &edited,
            before.as_ref(),
            &self.out_dir.clone(),
            self.image.as_ref().map(|i| i.handle),
            &self.backend.clone(),
            &crate::Extras { roots, index: false },
        );

        // The compiler predecessor, updated whether or not the revision was
        // accepted — §42's rule, and the same one the soak follows.
        if let Some(contract) = record.contract.clone() {
            // Every cached contract's view of the crate's bodies is refreshed
            // together with it. They share one crate: leaving an older
            // function's `bodies` map behind would make the *next* edit to it
            // report this edit as a change outside its closure, which is a
            // spurious FALLBACK produced entirely by the cache.
            for cached in self.contracts.values_mut() {
                cached.bodies = contract.bodies.clone();
                cached.crate_statics = contract.crate_statics.clone();
            }
            self.contracts.insert(function.path.clone(), contract);
        }

        let direct = record.verdict == "DIRECT";
        if !direct {
            self.needs_rebase = true;
        }
        let Some(staged) = staged.filter(|_| direct) else {
            return Observation {
                status: "refused",
                latency_ms: Some(record.active_ms.max(record.total_ms)),
                setup_ms: (setup_ms > 0.0).then_some(setup_ms),
                verdict: Some(if record.verdict.is_empty() {
                    "ERROR".into()
                } else {
                    record.verdict.clone()
                }),
                reason: record
                    .reason
                    .as_ref()
                    .and_then(|verdict| serde_json::to_string(verdict).ok()),
                symbol: Some(function.path.clone()),
                error: record.error.clone(),
                needs_rebase: self.needs_rebase,
                closure: Some(record.closure_size),
                ..Default::default()
            };
        };

        // Accepted, so the file on disk is what was compiled and the index's
        // spans move by exactly the length the splice changed.
        if let Err(error) = std::fs::write(&self.target_file, &edited) {
            return Observation::error(format!("cannot write the edit: {error}"));
        }
        // `shift` moves every offset at or after the old body's end, and the
        // edited function's own `body_end` is exactly that, so it moves with
        // the rest. It used to be adjusted a second time here, which put it
        // `delta` bytes too far and made the *next* edit to the same function
        // splice over whatever followed it.
        //
        // Invisible to the M1 gate, which edits each function once. Agent Bench
        // found it on its second candidate repair, as "the compiler session
        // failed" — the API had written syntactically broken Rust and the only
        // thing that noticed was rustc.
        self.index.shift(&function.file, function.body_end, delta);
        if let Some(entry) = self.index.functions.iter_mut().find(|f| f.id == function.id) {
            // The splice may have changed what this function calls, and nothing
            // here can know without asking the compiler. Marked rather than
            // guessed; `reindex` is the answer and it says what it cost.
            entry.edges_fresh = false;
        }

        self.staged_entries = staged.entries.clone();
        self.staged = Some(staged);
        self.editing.insert(function.id.clone());
        Observation {
            status: "staged",
            latency_ms: Some(record.active_ms),
            setup_ms: (setup_ms > 0.0).then_some(setup_ms),
            verdict: Some(record.verdict.clone()),
            symbol: Some(function.path),
            closure: Some(record.closure_size),
            generation: Some(self.runtime.enter().id),
            ..Default::default()
        }
    }

    /// Where a symbol's code currently lives, newest first.
    fn locate(&self, symbol: &str) -> Option<(*const u8, &'static str)> {
        let search = |entries: &[(String, usize)]| {
            entries
                .iter()
                .find(|(name, _)| crate::live::symbol_matches(name, symbol))
                .map(|(_, address)| *address as *const u8)
        };
        if let Some(address) = search(&self.staged_entries) {
            return Some((address, "candidate"));
        }
        if let Some(address) = self.committed.last().and_then(|(_, e)| search(e)) {
            return Some((address, "generation"));
        }
        self.image.as_ref()?.find(symbol).map(|address| (address, "image"))
    }

    fn probe(&mut self, symbol: &str, args: &[i64], bytes: Option<&str>) -> Observation {
        let at = Instant::now();
        let Some(function) = self.index.find(symbol) else {
            return Observation::error(format!("no function named {symbol}"));
        };
        let Some(signature) = function.probe.clone() else {
            return Observation {
                signature: Some(function.signature.clone()),
                ..Observation::error(format!(
                    "{} cannot be probed: a probe calls through a fixed calling convention \
                     and only builds `extern \"C\"` signatures of at most six integer \
                     scalars",
                    function.path
                ))
            };
        };
        if args.len() != signature.params.len() {
            return Observation::error(format!(
                "{} takes {} arguments, {} given",
                function.path,
                signature.params.len(),
                args.len()
            ));
        }
        let (path, wanted) = (function.path.clone(), function.symbol.clone());
        let name = if wanted.is_empty() { function.name.clone() } else { wanted };
        let Some((address, source)) = self.locate(&name) else {
            return Observation::error(format!(
                "{path} has no implementation in the candidate, the current generation or the \
                 base image — it is not exported and no patch has generated it"
            ));
        };

        // The buffer outlives the call because it outlives this scope: the
        // workspace keeps it. A `Vec` allocated here would be freed on return,
        // and a probe whose argument dangles by the time anything reads the
        // result is a probe that reports whatever the allocator left behind.
        let buffer = bytes.map(|text| text.as_bytes().to_vec());
        if let Some(buffer) = buffer {
            self.buffer = buffer;
        }
        if signature.params.iter().any(|p| p.buffer) && bytes.is_none() {
            return Observation::error(format!(
                "{path} takes a byte buffer; pass one as `bytes`"
            ));
        }
        let mut widened = [0u64; 6];
        for (slot, (value, scalar)) in
            widened.iter_mut().zip(args.iter().zip(signature.params.iter()))
        {
            *slot = if scalar.buffer {
                self.buffer.as_ptr() as u64
            } else {
                scalar.widen(*value)
            };
        }
        // SAFETY: `address` is a live, fully relocated function — either an
        // arena slab whose relocation completed before it was staged, or a
        // symbol in a loaded image. `probe_signature` has established that it
        // is `extern "C"` with at most six integer-scalar parameters, all of
        // which AAPCS passes in x0–x5; calling it through a six-register
        // prototype therefore passes its arguments in the registers it reads
        // and leaves the rest holding values it never looks at.
        let call: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(address) };
        let raw = call(widened[0], widened[1], widened[2], widened[3], widened[4], widened[5]);

        Observation {
            latency_ms: Some(at.elapsed().as_secs_f64() * 1e3),
            symbol: Some(path),
            returned: Some(signature.ret.narrow(raw)),
            source: Some(source),
            ..Observation::ok()
        }
    }

    /// Run the tests that reach the change, and only those.
    fn run_affected(&mut self) -> Observation {
        let at = Instant::now();
        let changed = self.editing.clone();
        let selected: Vec<crate::index::Function> = self
            .index
            .functions
            .iter()
            .filter(|f| f.is_test)
            .filter(|f| {
                // Selection, and the whole point of having a call graph: a test
                // that cannot reach *any* edited function cannot observe the
                // change. With nothing edited yet every test is selected,
                // because there is nothing to be unaffected by.
                if changed.is_empty() {
                    return true;
                }
                let reaches = self.index.reaches(&f.id);
                changed.iter().any(|id| reaches.contains(id))
            })
            .cloned()
            .collect();

        let mut failures = Vec::new();
        let mut ran = 0usize;
        for test in &selected {
            let name = if test.symbol.is_empty() { test.name.clone() } else { test.symbol.clone() };
            let Some((address, _)) = self.locate(&name) else {
                failures.push(Failure { test: test.path.clone(), expected: 0, actual: -1 });
                continue;
            };
            let mut expected = 0u64;
            // SAFETY: `is_test` established the signature — `extern "C"`, one
            // `*mut u64` out-parameter, `u64` return — off the declared type
            // rather than by assumption, which is §25's rule.
            let call: extern "C" fn(*mut u64) -> u64 = unsafe { std::mem::transmute(address) };
            let actual = call(&mut expected);
            ran += 1;
            if actual != expected {
                failures.push(Failure {
                    test: test.path.clone(),
                    expected: expected as i64,
                    actual: actual as i64,
                });
            }
        }

        Observation {
            latency_ms: Some(at.elapsed().as_secs_f64() * 1e3),
            tests: Some(ran),
            failed: Some(failures.len()),
            failures,
            source: Some(if self.staged.is_some() { "candidate" } else { "generation" }),
            ..Observation::ok()
        }
    }

    fn commit(&mut self) -> Observation {
        let at = Instant::now();
        let Some(staged) = self.staged.take() else {
            return Observation::error("nothing is staged");
        };
        let entries = std::mem::take(&mut self.staged_entries);
        let id = staged.commit(&self.runtime);
        self.committed.push((id, entries));
        self.editing.clear();
        Observation {
            status: "active",
            latency_ms: Some(at.elapsed().as_secs_f64() * 1e3),
            generation: Some(id),
            needs_rebase: self.needs_rebase,
            ..Default::default()
        }
    }

    /// Put the previous generation back. Code, and nothing else.
    ///
    /// Named for what it does. Whatever the retired generation wrote to
    /// globals, files or sockets is still written — §41's wording, kept,
    /// because an agent that reads `rollback` as "undo" will trust it with
    /// things it cannot undo.
    fn rollback(&mut self) -> Observation {
        let at = Instant::now();
        // A staged candidate is discarded first: rolling back while one is held
        // would leave a probe answering from a candidate that belongs to a
        // revision the caller has just abandoned.
        self.staged = None;
        self.staged_entries.clear();
        self.editing.clear();
        let parent = self.runtime.enter().parent;
        if !self.runtime.rollback_code(parent) {
            return Observation::error("no previous generation");
        }
        self.committed.pop();
        Observation {
            status: "rolled_back",
            latency_ms: Some(at.elapsed().as_secs_f64() * 1e3),
            generation: Some(parent),
            needs_rebase: self.needs_rebase,
            ..Default::default()
        }
    }

    fn status(&mut self) -> Observation {
        Observation {
            generation: Some(self.runtime.enter().id),
            tests: Some(self.index.functions.iter().filter(|f| f.is_test).count()),
            functions: Some(self.index.functions.len()),
            needs_rebase: self.needs_rebase,
            symbol: self
                .editing
                .iter()
                .filter_map(|id| self.index.by_id(id))
                .map(|f| f.path.clone())
                .collect::<Vec<_>>()
                .first()
                .cloned(),
            source: Some(if self.staged.is_some() { "candidate" } else { "generation" }),
            ..Observation::ok()
        }
    }
}

/// One request per line in, one observation per line out.
///
/// A line protocol rather than a socket or an MCP server because M1 is about
/// whether the *verbs* are the right ones. Everything above this function is
/// indifferent to the transport, and swapping it is an afternoon; discovering
/// that `probe` needed to take a generation identifier would not be.
pub fn serve(options: &crate::Options) -> bool {
    let mut workspace = Workspace::new(options);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut failed = false;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                let _ = writeln!(stdout, "{}", render(&Observation::error(error.to_string())));
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let quit = line.contains("\"quit\"");
        let observation = match serde_json::from_str::<Request>(line) {
            Ok(request) => workspace.handle(request),
            // The parse error itself, not a summary of it: an agent that sent
            // a malformed request needs to know which field, and this is the
            // one place where the protocol can say so.
            Err(error) => Observation::error(format!("cannot parse the request: {error}")),
        };
        if observation.status == "error" {
            failed = true;
        }
        let _ = writeln!(stdout, "{}", render(&observation));
        let _ = stdout.flush();
        if quit {
            break;
        }
    }
    !failed
}

fn render(observation: &Observation) -> String {
    serde_json::to_string(observation).unwrap_or_else(|error| {
        format!("{{\"status\":\"error\",\"error\":\"cannot serialize: {error}\"}}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_observation_omits_what_it_has_nothing_to_say_about() {
        // The shape is the product: an agent pays tokens per field, on every
        // observation, for the whole of a run.
        let rendered = render(&Observation {
            status: "active",
            latency_ms: Some(21.4),
            failed: Some(1),
            ..Default::default()
        });
        assert_eq!(rendered, r#"{"status":"active","latency_ms":21.4,"failed":1}"#);
    }

    #[test]
    fn a_failure_carries_both_numbers() {
        let rendered = render(&Observation {
            tests: Some(3),
            failed: Some(1),
            failures: vec![Failure { test: "::test_two".into(), expected: 167, actual: 162 }],
            ..Observation::ok()
        });
        assert!(rendered.contains(r#""expected":167,"actual":162"#));
    }

    #[test]
    fn the_verbs_parse_from_what_an_agent_would_write() {
        let parse = |text: &str| serde_json::from_str::<Request>(text).is_ok();
        assert!(parse(r#"{"op":"inspect","symbol":"scan"}"#));
        assert!(parse(r#"{"op":"replace_body","symbol":"scan","body":"{ 0 }"}"#));
        assert!(parse(r#"{"op":"probe","symbol":"scan","args":[3,5]}"#));
        // Arguments default to none rather than being required.
        assert!(parse(r#"{"op":"probe","symbol":"scan"}"#));
        assert!(parse(r#"{"op":"run_affected"}"#));
        assert!(parse(r#"{"op":"commit"}"#));
        assert!(parse(r#"{"op":"rollback"}"#));
        assert!(!parse(r#"{"op":"deploy"}"#));
    }
}