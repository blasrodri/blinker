//! The runtime differential (V2 §12.2).
//!
//! §22 checked one value on one edit. That is a start and it is not a safety
//! case. This runs the same source edit down two independent paths and compares
//! what the resulting programs *do*:
//!
//! ```text
//!                    same starting revision
//!                             │
//!                ┌────────────┴────────────┐
//!            LIVE PATH                 CLEAN PATH
//!                │                         │
//!     validate / classify           apply the same edit
//!       Path D closure                     │
//!        cg_clif patch            rustc, LLVM, real linker
//!          publish                         │
//!                │                         │
//!           run probes                run probes
//!                └────────────┬────────────┘
//!                             ↓
//!                     compare observations
//! ```
//!
//! The two sides share no code below the source text. The clean path is
//! ordinary `rustc` with the default backend producing a `cdylib` that the
//! dynamic loader loads; the live path is closure-only cg_clif into a `MAP_JIT`
//! arena. Nothing in the live path's lifting, relocation or publication logic
//! is on the clean side, so a bug there cannot cancel out.
//!
//! Why there is a base image
//! -------------------------
//!
//! Every error the classifier exists to prevent is an error about code that was
//! *not* patched: a caller holding an inlined copy of an old body, a constant
//! already folded into a neighbour, a layout some other function still assumes.
//! A differential that only ever calls the patched function cannot see any of
//! them, and would be green for exactly the reasons it should be red.
//!
//! So the live path loads the *previous* revision as a real base image, and
//! drives its probes through `diff_entry`, which lives in that image and
//! reaches the patch through a gate. Everything the entry point touches other
//! than the patch closure is old compiled code — which is the situation a live
//! patch actually creates.

use std::path::{Path, PathBuf};

use crate::arena::Arena;
use crate::generation::Runtime;

/// The probe inputs.
///
/// Chosen so that `branch_cold`'s `scale > 8` path is taken by some and not
/// others, `loop_edit`'s trip count varies across the whole range it clamps to,
/// and a zero and a wrapping case are both present. A single probe would make
/// most of the mutation suite untestable.
const PROBES: &[(u64, u32)] = &[
    (0, 0),
    (1, 1),
    (3, 5),
    (7, 2),
    (100, 3),
    (12_345, 0),
    (2, 17),
    (u64::MAX / 3, 9),
    (u64::MAX, 4),
];

/// What a revision of the program does, as far as this fixture can be observed.
///
/// Return values and the memory the callee writes are covered. Three of the
/// observable surfaces V2 §12.2 lists are *not*, and the reason is the same for
/// all three: `stdout`/`stderr`, panics, and callbacks all require the patch to
/// reference constant data (a format string, a `&Location`, a vtable), which
/// arrives as a section-relative relocation, which the current DIRECT class
/// refuses outright. They are fields here rather than absent so that widening
/// the class widens the comparison automatically instead of requiring somebody
/// to remember.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Observation {
    /// `(returned, written to the out-parameter)` for each probe, in order.
    pub probes: Vec<(u64, u64)>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Observation {
    /// Where two observations first disagree, in words a person can act on.
    fn difference(&self, other: &Observation) -> Option<String> {
        for (index, ((a, b), (c, d))) in self.probes.iter().zip(&other.probes).enumerate() {
            if a != c || b != d {
                let (value, scale) = PROBES[index];
                return Some(format!(
                    "probe {index} ({value}, {scale}): live returned {a} wrote {b}, \
                     clean returned {c} wrote {d}"
                ));
            }
        }
        if self.probes.len() != other.probes.len() {
            return Some(format!(
                "live ran {} probes, clean ran {}",
                self.probes.len(),
                other.probes.len()
            ));
        }
        if self.stdout != other.stdout || self.stderr != other.stderr {
            return Some("the two revisions printed different output".into());
        }
        None
    }
}

/// A loaded base image: the fixture crate, compiled and linked the ordinary way.
pub struct Image {
    handle: *mut libc::c_void,
    entry: extern "C" fn(u64, u32, *mut u64) -> u64,
    gate: *mut Option<extern "C" fn(u64, u32, *mut u64) -> u64>,
    /// The two patchable functions, called directly for §40's generation
    /// scenarios. Those compare a *generation's* implementations against a
    /// clean rebuild's, so they bypass the gate rather than installing into it.
    root: extern "C" fn(u64, u32, *mut u64) -> u64,
    second: extern "C" fn(u64) -> u64,
}

impl Image {
    /// Compile `source` to a `cdylib` with ordinary rustc and load it.
    ///
    /// `-Cmetadata` is pinned so the crate disambiguator — and therefore every
    /// mangled symbol — matches the spike's own session for the same source.
    /// Without it the patch's references to base-image symbols would be
    /// spelled differently in the two compilations and would not resolve.
    fn build(source: &Path, out: &Path) -> Result<Image, String> {
        let status = std::process::Command::new("rustc")
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
                "--crate-name=diff_fixture",
                "-Copt-level=0",
                "-Cdebuginfo=0",
                "-Cdebug-assertions=off",
                "-Cmetadata=diff",
                "--cap-lints=allow",
            ])
            .arg(source)
            .arg("-o")
            .arg(out)
            .output()
            .map_err(|e| format!("cannot run rustc: {e}"))?;
        if !status.status.success() {
            return Err(format!(
                "the clean build failed: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }
        Image::load(out)
    }

    fn load(path: &Path) -> Result<Image, String> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())?;
        // RTLD_LOCAL: each revision's image is loaded beside the others and
        // they all define the same symbol names. Letting them into the global
        // namespace would mean the *first* one loaded answers every later
        // `dlsym`, and the comparison would be a revision against itself.
        // SAFETY: a path this process just wrote.
        let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            // SAFETY: `dlerror` returns a static C string or null.
            let error = unsafe { libc::dlerror() };
            return Err(format!(
                "cannot load {}: {}",
                path.display(),
                if error.is_null() {
                    "unknown".into()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(error) }.to_string_lossy()
                }
            ));
        }
        let entry = symbol(handle, "diff_entry")
            .ok_or_else(|| format!("{} has no diff_entry", path.display()))?;
        let gate = symbol(handle, "DIFF_GATE")
            .ok_or_else(|| format!("{} has no DIFF_GATE", path.display()))?;
        let root = symbol(handle, "diff_root")
            .ok_or_else(|| format!("{} has no diff_root", path.display()))?;
        let second = symbol(handle, "diff_second")
            .ok_or_else(|| format!("{} has no diff_second", path.display()))?;
        Ok(Image {
            handle,
            // SAFETY: both are declared `#[no_mangle] extern "C"` in the
            // fixture with these signatures.
            root: unsafe { std::mem::transmute(root) },
            second: unsafe { std::mem::transmute(second) },
            // SAFETY: the fixture declares `diff_entry` with this signature and
            // `#[no_mangle] extern "C"`, so the type is read off the source
            // rather than assumed — §25.
            entry: unsafe { std::mem::transmute(entry) },
            gate: gate.cast(),
        })
    }

    /// Run every probe through the base image's entry point.
    fn observe(&self) -> Observation {
        let mut probes = Vec::with_capacity(PROBES.len());
        for (value, scale) in PROBES {
            let mut out = 0u64;
            let returned = (self.entry)(*value, *scale, &mut out);
            probes.push((returned, out));
        }
        Observation {
            probes,
            ..Default::default()
        }
    }

    /// Point the gate at a published patch, or back at the base image.
    ///
    /// # Safety
    /// `patch` must be a live, fully relocated function of the gate's type.
    unsafe fn install(&self, patch: Option<*const u8>) {
        let value = patch.map(|p| unsafe { std::mem::transmute::<*const u8, _>(p) });
        unsafe { std::ptr::write(self.gate, value) };
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: a handle this process opened and has not closed.
        unsafe { libc::dlclose(self.handle) };
    }
}

fn symbol(handle: *mut libc::c_void, name: &str) -> Option<*mut libc::c_void> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: an open handle and a null-terminated name.
    let address = unsafe { libc::dlsym(handle, c.as_ptr()) };
    (!address.is_null()).then_some(address)
}

/// One mutation's verdict.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Trial {
    pub mutation: String,
    pub verdict: String,
    /// `Some` when the patch was published and both sides ran.
    pub agreed: Option<bool>,
    /// The first disagreement, when there was one.
    pub difference: Option<String>,
    /// Whether the base image still answers as it did before the attempt.
    /// Checked on every trial, not only the failing ones: an accepted patch
    /// that corrupted the old generation would otherwise go unnoticed.
    pub base_intact: bool,
    pub closure_size: usize,
    pub error: Option<String>,
}

/// How the harness is allowed to sabotage itself.
///
/// A differential suite that has never failed is a suite nobody has shown can
/// fail. Each of these injects a specific, realistic defect and asserts the
/// *specific* way the system is supposed to survive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sabotage {
    None,
    /// Drop one member of the patch closure, as an incomplete Path D would.
    /// Must be caught while the artifact is being built — never as a wrong
    /// answer at run time.
    OmitClosureMember,
    /// Point one relocation at a symbol that does not exist, as a corrupted
    /// artifact would. Must reject the candidate and leave the current
    /// generation exactly as it was.
    CorruptRelocation,
    /// Publish an edit the classifier refused. The suite must then *find the
    /// semantic error itself*, because otherwise a green run proves only that
    /// the classifier never says yes to anything dangerous — not that the
    /// differential could tell if it did.
    IgnoreClassifier,
    /// Flip a byte in a constant the patch carries (§46). Nothing about the
    /// artifact becomes unresolvable, so nothing refuses it — the program
    /// simply computes a different answer, and the differential has to be the
    /// thing that notices. Without this control, "the patch carries its
    /// constants" is satisfied equally well by carrying bytes nobody reads.
    CorruptConstant,
    /// Drop a carried constant, as an incomplete transitive walk would. Must be
    /// caught while the artifact is built, never as a wrong answer.
    OmitConstant,
}

impl Sabotage {
    pub fn parse(text: &str) -> Option<Sabotage> {
        Some(match text {
            "none" => Sabotage::None,
            "omit" => Sabotage::OmitClosureMember,
            "relocation" => Sabotage::CorruptRelocation,
            "classifier" => Sabotage::IgnoreClassifier,
            "constant" => Sabotage::CorruptConstant,
            "omit-constant" => Sabotage::OmitConstant,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Sabotage::None => "none",
            Sabotage::OmitClosureMember => "omit a closure member",
            Sabotage::CorruptRelocation => "corrupt a relocation",
            Sabotage::IgnoreClassifier => "ignore the classifier",
            Sabotage::CorruptConstant => "corrupt a carried constant",
            Sabotage::OmitConstant => "omit a carried constant",
        }
    }
}

/// Run the whole suite.
pub fn run(options: &crate::Options, sabotage: Sabotage) -> Vec<Trial> {
    let work = options.incremental.clone();
    // Start cold, every run.
    //
    // Not a tidiness measure. rustc reuses a cached codegen unit when a
    // crate's code has not changed, and the suite runs the same ten mutations
    // once per sabotage mode — so from the second mode onward every revision
    // was a cache hit and nothing was compiled at all. The sink noticed,
    // because it only ever sees code that was actually generated. The object
    // path did not: it read a file off disk that a *previous* compilation had
    // left there and published it. That happened to be the right code here,
    // and it is right by luck rather than by construction.
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::create_dir_all(&work);
    let pristine = std::fs::read_to_string(options.variants.join("pristine.rs"))
        .expect("the pristine variant exists");

    println!("\n  runtime differential — sabotage: {}", sabotage.label());
    let mut trials = Vec::new();
    for (index, mutation) in options.edits.iter().enumerate() {
        let edited = match std::fs::read_to_string(options.variants.join(format!("{mutation}.rs")))
        {
            Ok(text) => text,
            Err(_) => {
                trials.push(Trial {
                    mutation: mutation.clone(),
                    verdict: String::new(),
                    agreed: None,
                    difference: None,
                    base_intact: false,
                    closure_size: 0,
                    // Marked as *not* base-intact so the report fails.
                    //
                    // It used to read as an ordinary refusal, so a mutation
                    // whose file the generator had never written counted as a
                    // pass — and §48's mutation did exactly that for one run,
                    // reporting "as expected" for an experiment that had not
                    // happened. A suite that cannot tell "refused" from "never
                    // ran" is a suite that can be silently emptied.
                    error: Some(format!(
                        "no variant named {mutation} — run differential_fixtures.py"
                    )),
                });
                continue;
            }
        };
        trials.push(trial(options, mutation, &pristine, &edited, index, sabotage, &work));
    }
    trials
}

#[allow(clippy::too_many_arguments)]
fn trial(
    options: &crate::Options,
    mutation: &str,
    pristine: &str,
    edited: &str,
    index: usize,
    sabotage: Sabotage,
    work: &Path,
) -> Trial {
    let mut trial = Trial {
        mutation: mutation.to_string(),
        verdict: String::new(),
        agreed: None,
        difference: None,
        base_intact: false,
        closure_size: 0,
        error: None,
    };

    // The base image: the *previous* revision, compiled and linked the ordinary
    // way. This is what a running program would be.
    std::fs::write(&options.target_file, pristine).expect("write pristine");
    let base = match Image::build(
        &options.target_file,
        &work.join(format!("base-{index}.dylib")),
    ) {
        Ok(image) => image,
        Err(error) => {
            trial.error = Some(error);
            return trial;
        }
    };
    let before = base.observe();

    // The previous revision's contract, from a real compiler session, so the
    // verdict is a relation between two revisions rather than a guess.
    let warm = crate::run_session_with(
        &options.fixture,
        &options.target_file,
        &options.incremental,
        &options.hot,
        crate::Mode::HotClosure,
        // The backend flag belongs here too, and its absence was not harmless.
        // rustc loads the codegen backend once per *process* and caches it, so
        // the first session decides for all of them. This one ran first with no
        // flag, which meant every later session used LLVM no matter what it
        // asked for — the sink was never reached, and before that the suite was
        // quietly validating LLVM's objects while believing they were cg_clif's.
        &{
            let mut arguments = vec![
                "-Cmetadata=diff".to_string(),
                "-Cdebug-assertions=off".to_string(),
            ];
            // Always, and matching whatever `patch` will ask for. Omitting it
            // when no `--backend` was given still left this session choosing
            // LLVM and every session after it stuck with that choice — the
            // same defect in a smaller form, which is why the invariant checks
            // the *value* rather than merely that a flag was passed.
            arguments.push(match &options.backend {
                Some(backend) => format!("-Zcodegen-backend={}", backend.display()),
                None => "-Zcodegen-backend=cranelift".into(),
            });
            arguments
        },
        false,
    );

    // The live path.
    std::fs::write(&options.target_file, edited).expect("write edited");
    let arena = Arena::reserve(64 * 1024 * 1024).expect("arena");
    let runtime = Runtime::new(4);
    if options.backend.is_some() {
        crate::sink::set_sabotage(crate::live::PatchOptions {
            omit_closure_member: sabotage == Sabotage::OmitClosureMember,
            corrupt_relocation: sabotage == Sabotage::CorruptRelocation,
            ignore_classifier: sabotage == Sabotage::IgnoreClassifier,
            corrupt_constant: sabotage == Sabotage::CorruptConstant,
            omit_constant: sabotage == Sabotage::OmitConstant,
        });
    }
    let live = crate::live::patch(
        &arena,
        &runtime,
        &options.fixture,
        &options.target_file,
        &options.incremental,
        &options.hot,
        warm.contract.as_ref(),
        work,
        // The base image answers first for anything the patch references but
        // does not contain — a static it reads, a function outside the
        // closure. That is what a base image is *for*, and passing `None` here
        // made `read_static` fail to resolve a static the program plainly had.
        Some(base.handle),
        options.backend.as_deref(),
        crate::live::PatchOptions {
            omit_closure_member: sabotage == Sabotage::OmitClosureMember,
            corrupt_relocation: sabotage == Sabotage::CorruptRelocation,
            ignore_classifier: sabotage == Sabotage::IgnoreClassifier,
            corrupt_constant: sabotage == Sabotage::CorruptConstant,
            omit_constant: sabotage == Sabotage::OmitConstant,
        },
    );
    trial.verdict = live.verdict.clone();
    trial.closure_size = live.closure_size;

    let published = match live.staged.map(|staged| {
        let entry = staged.entry();
        // The commit. A trial that got this far has a DIRECT verdict and an
        // artifact that passed every rule, which is exactly the precondition.
        staged.commit(&runtime);
        entry as *const u8
    }) {
        Some(pointer) => pointer,
        None => {
            trial.error = live.error.clone();
            // The transactional claim, and the reason this is checked even when
            // nothing was published: a refused candidate must leave the running
            // program exactly as it was, not merely fail to improve it.
            trial.base_intact = base.observe() == before;
            return trial;
        }
    };

    // SAFETY: `entry` is a relocated function in the arena, of the gate's type.
    unsafe { base.install(Some(published)) };
    let observed = base.observe();
    // SAFETY: restoring the base image's own function.
    unsafe { base.install(None) };
    trial.base_intact = base.observe() == before;

    // The clean path: the same edit, rebuilt from scratch by ordinary rustc.
    let clean = match Image::build(
        &options.target_file,
        &work.join(format!("clean-{index}.dylib")),
    ) {
        Ok(image) => image,
        Err(error) => {
            trial.error = Some(error);
            return trial;
        }
    };
    let expected = clean.observe();

    trial.difference = observed.difference(&expected);
    trial.agreed = Some(trial.difference.is_none());
    trial
}

/// What a probe observes when it runs the patched code through a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
struct Reading {
    returned: u64,
    written: u64,
    second: u64,
}

/// Call a generation's closure. `root` and `second` are gate indices.
///
/// # Safety
/// The generation's slots must hold relocated functions of these signatures.
unsafe fn read(generation: &crate::generation::Generation, root: usize, second: usize) -> Reading {
    let mut written = 0u64;
    let f: extern "C" fn(u64, u32, *mut u64) -> u64 =
        unsafe { std::mem::transmute(generation.implementation(root).expect("root")) };
    let g: extern "C" fn(u64) -> u64 =
        unsafe { std::mem::transmute(generation.implementation(second).expect("second")) };
    let returned = f(3, 5, &mut written);
    Reading { returned, written, second: g(11) }
}

/// The same reading from a cleanly rebuilt image, as the reference.
fn read_clean(image: &Image) -> Reading {
    let mut written = 0u64;
    let returned = (image.root)(3, 5, &mut written);
    Reading { returned, written, second: (image.second)(11) }
}

/// Where the closure's two `extern "C"` members sit in a candidate's gates.
fn gates(staged: &crate::live::Staged) -> Option<(usize, usize)> {
    let find = |wanted: &str| {
        staged
            .entries
            .iter()
            .position(|(name, _)| crate::live::symbol_matches(name, wanted))
    };
    Some((find("diff_root")?, find("diff_second")?))
}

/// Build one revision's candidate without committing it.
fn candidate(
    options: &crate::Options,
    arena: &Arena,
    runtime: &Runtime,
    source: &str,
    before: Option<&crate::classify::Contract>,
    image: &Image,
    work: &Path,
) -> Result<(crate::live::Staged, crate::classify::Contract), String> {
    std::fs::write(&options.target_file, source).expect("write");
    let mut arguments = vec![
        "-Cmetadata=diff".to_string(),
        "-Cdebug-assertions=off".to_string(),
    ];
    arguments.push(match &options.backend {
        Some(backend) => format!("-Zcodegen-backend={}", backend.display()),
        None => "-Zcodegen-backend=cranelift".into(),
    });
    let warm = crate::run_session_with(
        &options.fixture,
        &options.target_file,
        &options.incremental,
        &options.hot,
        crate::Mode::HotClosure,
        &arguments,
        false,
    );
    let _ = warm;
    let patch = crate::live::patch(
        arena,
        runtime,
        &options.fixture,
        &options.target_file,
        &options.incremental,
        &options.hot,
        before,
        work,
        Some(image.handle),
        options.backend.as_deref(),
        crate::live::PatchOptions::default(),
    );
    match (patch.staged, patch.error) {
        (Some(staged), _) => {
            let contract = patch.contract.ok_or("no contract")?;
            Ok((staged, contract))
        }
        (None, Some(error)) => Err(error),
        (None, None) => Err("no candidate and no reason".into()),
    }
}

/// Generation consistency under concurrency, and rollback (§40).
///
/// Barriers rather than sleeps. A sleep makes a race *likely*; a barrier makes
/// the interleaving the test claims to exercise the only one that can happen.
pub fn generations(options: &crate::Options) -> bool {
    use std::sync::Barrier;

    let work = options.incremental.clone();
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::create_dir_all(&work);
    let arena = Arena::reserve(64 * 1024 * 1024).expect("arena");
    let runtime = Runtime::new(8);

    let read_source = |name: &str| {
        std::fs::read_to_string(options.variants.join(format!("{name}.rs")))
            .unwrap_or_else(|_| panic!("no variant named {name}"))
    };
    let g0 = read_source("pristine");
    let g1 = read_source("body_arith");
    let g2 = read_source("two_gates");

    // Three clean rebuilds, by ordinary rustc and LLVM in a subprocess, loaded
    // by the dynamic loader. Every assertion below is against one of these.
    std::fs::write(&options.target_file, &g0).expect("write");
    let base = match Image::build(&options.target_file, &work.join("g0.dylib")) {
        Ok(image) => image,
        Err(error) => {
            println!("  cannot build the base image: {error}");
            return false;
        }
    };
    let mut clean = Vec::new();
    for (index, source) in [&g1, &g2].iter().enumerate() {
        std::fs::write(&options.target_file, source).expect("write");
        match Image::build(&options.target_file, &work.join(format!("clean{index}.dylib"))) {
            Ok(image) => clean.push(image),
            Err(error) => {
                println!("  cannot build a clean reference: {error}");
                return false;
            }
        }
    }
    let (clean_g1, clean_g2) = (read_clean(&clean[0]), read_clean(&clean[1]));
    // Every field has to discriminate, not just one. A probe that reads two
    // gates from a captured generation proves nothing about the second gate if
    // the two revisions happen to agree on it.
    if clean_g1.returned == clean_g2.returned
        || clean_g1.written == clean_g2.written
        || clean_g1.second == clean_g2.second
    {
        println!(
            "  the two revisions agree on something the probe reads \
             ({clean_g1:?} vs {clean_g2:?}); part of this test could not fail"
        );
        return false;
    }

    // G0 → G1, committed.
    std::fs::write(&options.target_file, &g0).expect("write");
    let zero = crate::run_session_with(
        &options.fixture,
        &options.target_file,
        &options.incremental,
        &options.hot,
        crate::Mode::HotClosure,
        &[
            "-Cmetadata=diff".to_string(),
            "-Cdebug-assertions=off".to_string(),
            match &options.backend {
                Some(backend) => format!("-Zcodegen-backend={}", backend.display()),
                None => "-Zcodegen-backend=cranelift".into(),
            },
        ],
        false,
    );
    let (first, contract_g1) =
        match candidate(options, &arena, &runtime, &g1, zero.contract.as_ref(), &base, &work) {
            Ok(pair) => pair,
            Err(error) => {
                println!("  G1 produced no candidate: {error}");
                return false;
            }
        };
    let Some((root, second)) = gates(&first) else {
        println!("  the candidate does not contain both probe functions");
        return false;
    };
    first.commit(&runtime);

    let mut failures: Vec<String> = Vec::new();
    let mut checks = 0usize;

    // Scenario 1: a scope holds the generation it entered, across a commit.
    {
        let (start, resume) = (Barrier::new(2), Barrier::new(2));
        let (second_candidate, _produced) =
            match candidate(options, &arena, &runtime, &g2, Some(&contract_g1), &base, &work) {
                Ok(pair) => pair,
                Err(error) => {
                    println!("  G2 produced no candidate: {error}");
                    return false;
                }
            };
        std::thread::scope(|threads| {
            let holder = threads.spawn(|| {
                crate::generation::scope(&runtime, |generation| {
                    // SAFETY: a committed generation of this fixture.
                    let before = unsafe { read(generation, root, second) };
                    start.wait();
                    resume.wait();
                    // The same generation, read again *after* another thread
                    // has committed a newer one. Two functions, not one: a
                    // single pointer read on entry could not have changed.
                    let after = unsafe { read(generation, root, second) };
                    (before, after)
                })
            });
            start.wait();
            second_candidate.commit(&runtime);
            let fresh = crate::generation::scope(&runtime, |generation| {
                // SAFETY: the generation just committed.
                unsafe { read(generation, root, second) }
            });
            resume.wait();
            let (before, after) = holder.join().expect("thread");
            checks += 3;
            if before != clean_g1 {
                failures.push(format!("a scope on G1 read {before:?}, clean G1 is {clean_g1:?}"));
            }
            if after != clean_g1 {
                failures.push(format!(
                    "after a concurrent commit the same scope read {after:?}, clean G1 is \
                     {clean_g1:?} — the scope did not hold its generation"
                ));
            }
            if fresh != clean_g2 {
                failures.push(format!("a scope entered on G2 read {fresh:?}, clean G2 is {clean_g2:?}"));
            }
        });
    }

    // Scenario 2: a refused candidate is never observable — not eventually,
    // never. It is built while a scope is open and discarded without ever
    // being committed, so there is no interleaving in which it could run.
    {
        let refused = read_source("edit_outside_closure");
        let (start, resume) = (Barrier::new(2), Barrier::new(2));
        std::thread::scope(|threads| {
            let holder = threads.spawn(|| {
                crate::generation::scope(&runtime, |generation| {
                    start.wait();
                    resume.wait();
                    // SAFETY: a committed generation of this fixture.
                    unsafe { read(generation, root, second) }
                })
            });
            start.wait();
            let outcome =
                candidate(options, &arena, &runtime, &refused, Some(&contract_g1), &base, &work);
            let refused_correctly = match outcome {
                // The classifier refuses before a candidate is returned.
                Err(_) => true,
                // A candidate that was built is simply dropped. Nothing ever
                // pointed at it.
                Ok((staged, _)) => {
                    drop(staged);
                    true
                }
            };
            let during = crate::generation::scope(&runtime, |generation| {
                // SAFETY: a committed generation of this fixture.
                unsafe { read(generation, root, second) }
            });
            resume.wait();
            let held = holder.join().expect("thread");
            checks += 2;
            let _ = refused_correctly;
            if held != clean_g2 || during != clean_g2 {
                failures.push(format!(
                    "a refused revision was observable: held {held:?}, concurrent {during:?}, \
                     clean G2 is {clean_g2:?}"
                ));
            }
        });
    }

    // Scenario 3: rollback obeys the same rules as publication.
    {
        let (start, resume) = (Barrier::new(2), Barrier::new(2));
        std::thread::scope(|threads| {
            let holder = threads.spawn(|| {
                crate::generation::scope(&runtime, |generation| {
                    let before = unsafe { read(generation, root, second) };
                    start.wait();
                    resume.wait();
                    let after = unsafe { read(generation, root, second) };
                    (before, after)
                })
            });
            start.wait();
            // Back to G1. Code only: whatever G2 wrote to globals, files or
            // sockets is still written, which is why it is called a code
            // rollback and not a transaction.
            let rolled = runtime.rollback_code(1);
            let after_rollback = crate::generation::scope(&runtime, |generation| unsafe {
                read(generation, root, second)
            });
            resume.wait();
            let (before, after) = holder.join().expect("thread");
            checks += 3;
            if !rolled {
                failures.push("the rollback found no such generation".into());
            }
            if before != clean_g2 || after != clean_g2 {
                failures.push(format!(
                    "a scope on G2 read {before:?} then {after:?} across a rollback, clean G2 \
                     is {clean_g2:?}"
                ));
            }
            if after_rollback != clean_g1 {
                failures.push(format!(
                    "after rolling back to G1 a new scope read {after_rollback:?}, clean G1 is \
                     {clean_g1:?}"
                ));
            }
        });
    }

    // The control, last, because it commits. Scenario 1 asserts that a *captured* generation still reads
    // G1 after a concurrent commit — but that assertion is only worth
    // something if reading the wrong thing would have been noticed. So the
    // same interleaving is run again with the probe deliberately re-entering
    // the runtime after the barrier instead of using the generation it holds.
    // It runs after the rollback, so the current generation is G1 and the
    // commit is a G2 candidate: the held scope must still read G1 and the
    // re-entering one must read G2. If the second does not, the commit was
    // never visible and scenario 1 passed because nothing was happening.
    {
        let (start, resume) = (Barrier::new(2), Barrier::new(2));
        let (third, _) =
            match candidate(options, &arena, &runtime, &g2, Some(&contract_g1), &base, &work) {
                Ok(pair) => pair,
                Err(error) => {
                    println!("  the control produced no candidate: {error}");
                    return false;
                }
            };
        std::thread::scope(|threads| {
            let holder = threads.spawn(|| {
                let held = crate::generation::scope(&runtime, |generation| {
                    start.wait();
                    resume.wait();
                    // SAFETY: a committed generation of this fixture.
                    unsafe { read(generation, root, second) }
                });
                // Deliberately wrong: a fresh scope, so the newest generation.
                let reloaded = crate::generation::scope(&runtime, |generation| unsafe {
                    read(generation, root, second)
                });
                (held, reloaded)
            });
            start.wait();
            third.commit(&runtime);
            resume.wait();
            let (held, reloaded) = holder.join().expect("thread");
            checks += 2;
            if held != clean_g1 {
                failures.push(format!("the control's held scope read {held:?}, expected G1"));
            }
            if reloaded != clean_g2 {
                failures.push(format!(
                    "re-entering the runtime after the commit read {reloaded:?} rather than the \
                     newly committed revision — the commit was not visible, so scenario 1 \
                     proved nothing"
                ));
            }
        });
    }


    println!("\n  generation semantics, against independently rebuilt references");
    println!("    clean G1 {clean_g1:?}");
    println!("    clean G2 {clean_g2:?}");
    println!(
        "    {checks} observations across 3 scenarios and a control: {}",
        if failures.is_empty() { "all exact" } else { "FAILED" }
    );
    for failure in &failures {
        println!("      {failure}");
    }
    failures.is_empty()
}

/// Print the suite's outcome, and say what it proves.
pub fn report(trials: &[Trial], sabotage: Sabotage, out: Option<&PathBuf>) -> bool {
    println!(
        "\n  {:<18} {:<10} {:>8} {:>8} {:>7}  {}",
        "mutation", "verdict", "closure", "agreed", "base", "note"
    );
    let mut ok = true;
    let mut refused = 0usize;
    let mut agreed = 0usize;
    for trial in trials {
        if trial.agreed == Some(true) {
            agreed += 1;
        }
        // What "correct" means depends on which control is running, and the
        // whole value of the controls is that they expect *different* things.
        let expected_to_publish = trial.verdict == "DIRECT" && sabotage == Sabotage::None;
        let good = match sabotage {
            Sabotage::None => {
                if expected_to_publish {
                    // A DIRECT edit that could not be built into an artifact is
                    // a *coverage* gap, not a safety failure: the program is
                    // untouched and correct. Counted separately below, because
                    // conflating "refused" with "wrong" in either direction
                    // makes the suite useless — one hides real breakage, the
                    // other makes an honest refusal look like one.
                    match trial.agreed {
                        Some(agreed) => agreed && trial.base_intact,
                        None => {
                            refused += 1;
                            trial.base_intact
                        }
                    }
                } else {
                    // A refusal is a correct outcome; the program must survive it.
                    trial.agreed.is_none() && trial.base_intact
                }
            }
            // Caught while building the artifact, never as a wrong answer, and
            // the running program untouched.
            Sabotage::OmitClosureMember
            | Sabotage::CorruptRelocation
            | Sabotage::OmitConstant => {
                trial.agreed.is_none() && trial.error.is_some() && trial.base_intact
            }
            // The one control that must *not* be caught while building. A
            // constant with its bytes flipped is a perfectly well-formed
            // artifact: every relocation resolves, nothing is missing, the
            // patch publishes. Only the behaviour comparison can see it.
            //
            // The requirement is therefore at the *suite* level rather than per
            // mutation, and that is not a weakening — it is what this control
            // is actually about. Most carried constants are panic locations on
            // a path the probes never take, so corrupting them is genuinely
            // unobservable and demanding that every mutation notice would be
            // demanding the impossible. What has to be true is that the
            // differential *can* detect a corrupted constant, and one mutation
            // whose answer depends on carried bytes is enough to establish it.
            // The check is below, after the loop.
            Sabotage::CorruptConstant => trial.base_intact,
            // Only the mutations the classifier *would* have refused are under
            // test here. Forcing an edit that was DIRECT anyway changes
            // nothing, and demanding that those also break made the control
            // fail on eight mutations that were behaving perfectly — the
            // control was miswritten, not the system.
            Sabotage::IgnoreClassifier => {
                if trial.verdict == "DIRECT" {
                    // Same allowance as the main mode: an artifact the class
                    // cannot build is a refusal, not a disagreement.
                    match trial.agreed {
                        Some(agreed) => agreed && trial.base_intact,
                        None => {
                            refused += 1;
                            trial.base_intact
                        }
                    }
                } else {
                    // Something must catch it: either the artifact rules
                    // refuse to build it, or the behaviour comparison sees it.
                    trial.agreed != Some(true) && trial.base_intact
                }
            }
        };
        ok &= good;
        println!(
            "  {:<18} {:<10} {:>8} {:>8} {:>7}  {} {}",
            trial.mutation,
            if trial.verdict.is_empty() { "-" } else { &trial.verdict },
            trial.closure_size,
            match (trial.agreed, expected_to_publish) {
                (Some(true), _) => "yes",
                (Some(false), _) => "NO",
                (None, true) => "refused",
                (None, false) => "-",
            },
            if trial.base_intact { "intact" } else { "MOVED" },
            if good { "ok  " } else { "FAIL" },
            trial
                .difference
                .clone()
                .or_else(|| trial.error.clone())
                .unwrap_or_default(),
        );
    }
    // A suite in which nothing ever agrees would satisfy every rule above by
    // refusing everything, so the count that would make it vacuous is stated.
    if sabotage == Sabotage::None && agreed == 0 {
        ok = false;
        println!("\n  nothing agreed: the suite proved nothing");
    }
    // The suite-level half of the corrupted-constant control (§46).
    //
    // Per-trial it only requires that the program survive, because most carried
    // constants are panic locations on a path the probes never take and
    // corrupting those is genuinely unobservable. What the control is *for* is
    // the claim that the differential can detect a wrong constant at all, and
    // that is a property of the suite: at least one mutation whose answer
    // depends on carried bytes has to notice.
    if sabotage == Sabotage::CorruptConstant {
        let detected = trials.iter().filter(|trial| trial.agreed == Some(false)).count();
        if detected == 0 {
            ok = false;
            println!(
                "\n  no mutation disagreed: corrupting every carried constant changed nothing \
                 the probes can see, so this run does not show that a wrong constant is \
                 detectable"
            );
        } else {
            println!("\n  {detected} mutation(s) disagreed, which is what this control requires");
        }
    }
    println!(
        "\n  {}: {}{}",
        sabotage.label(),
        if ok { "as expected" } else { "NOT as expected" },
        if refused > 0 {
            format!("  ({agreed} agreed, {refused} refused as out of artifact class)")
        } else {
            String::new()
        }
    );
    if let Some(path) = out {
        let json = serde_json::to_string_pretty(trials).expect("serialize");
        std::fs::write(path, json).expect("write");
    }
    ok
}