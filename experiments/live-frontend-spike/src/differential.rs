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
                "--crate-type=cdylib",
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
        Ok(Image {
            handle,
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
}

impl Sabotage {
    pub fn parse(text: &str) -> Option<Sabotage> {
        Some(match text {
            "none" => Sabotage::None,
            "omit" => Sabotage::OmitClosureMember,
            "relocation" => Sabotage::CorruptRelocation,
            "classifier" => Sabotage::IgnoreClassifier,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Sabotage::None => "none",
            Sabotage::OmitClosureMember => "omit a closure member",
            Sabotage::CorruptRelocation => "corrupt a relocation",
            Sabotage::IgnoreClassifier => "ignore the classifier",
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
                    base_intact: true,
                    closure_size: 0,
                    error: Some(format!("no variant named {mutation}")),
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
        },
    );
    trial.verdict = live.verdict.clone();
    trial.closure_size = live.closure_size;

    let published = match live.entry {
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
            Sabotage::OmitClosureMember | Sabotage::CorruptRelocation => {
                trial.agreed.is_none() && trial.error.is_some() && trial.base_intact
            }
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
