//! The link itself: object files in, an executable out.
//!
//! Every other crate here does one stage well and is tested in isolation.
//! This one is the seam between them, and seams are where the errors that
//! isolated tests cannot see live — a symbol whose address is computed in one
//! coordinate system and consumed in another, a section copied to the right
//! offset in the wrong buffer. So the test for this crate is not "did the
//! pieces get called" but "does the program run and print what it should".
//!
//! ```text
//! parse each object      blinker-macho
//!   → resolve symbols    blinker-symbols
//!   → place sections     blinker-layout
//!   → copy content       here
//!   → patch relocations  blinker-relocations
//!   → emit and sign      blinker-output
//! ```
//!
//! # Why the layout is computed twice
//!
//! Relocations need the addresses that layout assigns, and layout runs inside
//! `ImageBuilder::build`. Rather than duplicate the layout computation — where
//! a divergence would put relocations at addresses the emitted image does not
//! use — the image is built once to *learn* the layout, the relocations are
//! applied against it, and it is built again with the patched bytes. Layout
//! depends only on section sizes and alignments, which the patching does not
//! change, so the two passes agree by construction.

// This crate is a library: everything it has to say travels through
// `LinkError`, `LinkTimings` or a return value. Printing is the CLI's job.
//
// Denied because it did not stay hypothetical — two `eprintln!` calls added to
// measure the loader were committed and shipped, printing to stderr on every
// link. The gate had nothing to say about it: the tests pass, the output is
// correct, and stray diagnostics are invisible to both.
#![deny(clippy::print_stderr, clippy::print_stdout)]

/// Every map in the link uses the fast hasher; see [`hashing`].
use hashing::{FastMap as HashMap, FastSet as HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blinker_layout::InputPlacement;
use blinker_macho::{
    parse_object, Arm64RelocationKind, InputRelocation, InputSection, ObjectId, ParsedObject,
    RelocationLength, RelocationTarget, SectionId, SectionKind, SymbolStrength, SymbolVisibility,
};
use blinker_output::image::Dylib;
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Bind, Image, ImageBuilder, Rebase, UnwindEntry};
use blinker_relocations::{apply, Context};
use blinker_symbols::{SymbolNameId, SymbolNames, SymbolProvider, SymbolTable};
use reachability::Strip;

mod hashing;
mod identity;
pub mod libraries;
mod mapping;
mod parallel;
mod session;
use hashing::FastMap;
pub use identity::ContributionKeys;
pub use session::Session;

pub mod error;
pub use error::LinkError;

/// How long each stage of a link took.
///
/// Recorded because M4's cache has to know *what* to cache. Caching the wrong
/// stage buys nothing: if emitting dominates, storing parse results saves
/// almost none of the 44.6 ms a cold link costs. This is measured rather than
/// assumed for the same reason every other number in this project is.
#[derive(Debug, Clone, Default)]
pub struct LinkTimings {
    pub read_and_parse_ms: f64,
    pub resolve_ms: f64,
    pub layout_probe_ms: f64,
    pub relocate_ms: f64,
    pub emit_ms: f64,
    pub total_ms: f64,
    /// Objects whose patched bytes came from the cache rather than from
    /// relocating them again. Zero on a cold link, and on every link that did
    /// not ask for a cache.
    pub reused_objects: u64,
    /// Objects the link resolved, after archive members were extracted.
    ///
    /// Reported alongside `reused_objects` because the two must be compared
    /// against each other and not against the number of input *files*: 19
    /// rlibs on the command line can become 200 objects in the link, and a
    /// hit rate computed against the file count is meaningless.
    pub total_objects: u64,
    /// Relocations that did not have to be applied, and the total.
    ///
    /// The honest unit. Object sizes in a Rust link span three orders of
    /// magnitude — one libstd codegen unit holds more relocations than every
    /// other object together — so "46 of 47 objects reused" describes a link
    /// that saved almost nothing (finding 66). Counting the work rather than
    /// the files is what makes the number mean what it appears to mean.
    pub reused_relocations: u64,
    pub total_relocations: u64,
    /// Whether the finished binary was taken from the cache outright.
    ///
    /// Distinct from reusing every object: that still resolves, lays out and
    /// assembles, and only skips relocating. This skips all of it. The two
    /// report the same hit rate, so without this flag no test could tell them
    /// apart — and a fast path that silently stops firing is finding 64 again.
    pub reused_finished_image: bool,
    /// Time spent deciding what the program can reach. Zero without
    /// `-dead_strip`.
    pub dead_strip_ms: f64,
    /// Splitting the strip into its three halves, which are incremental to very
    /// different degrees — see `reachability::StripTimings`. All three are
    /// inside `dead_strip_ms`.
    pub atoms_ms: f64,
    pub liveness_ms: f64,
    pub strip_build_ms: f64,
    /// Inside `liveness_ms`: grouping relocations, then traversing the graph.
    pub group_ms: f64,
    pub traverse_ms: f64,
    /// Objects whose reachability projection moved, of how many compared.
    pub digest_ms: f64,
    pub reach_moved: u64,
    pub reach_total: u64,
    /// Work between the named stages that no stage owned: building placements,
    /// scanning `__eh_frame` for personality fields, sizing the unwind table,
    /// and collecting commons. It was 1.9 ms of "unmeasured" and the only
    /// reason it looked small is that nothing was looking.
    pub prepare_ms: f64,
    /// The four unrelated jobs inside `prepare_ms`.
    pub placements_ms: f64,
    pub personality_ms: f64,
    pub unwind_size_ms: f64,
    pub commons_ms: f64,
    /// Counting the placement invariant, which is diagnostics rather than link.
    pub accounting_ms: f64,
    /// `__text` bytes the strip removed, as the analysis counts them.
    pub stripped_bytes: u64,
    /// Atoms the propagation left dead that something live then referred to.
    ///
    /// Reported because it must be zero: a non-zero count is the model failing
    /// to describe the input, caught by the verification pass rather than by a
    /// crash in the linked program.
    pub revived_atoms: u64,
    /// What the cache costs, separated from what it saves.
    ///
    /// Every part of this was previously charged to a stage that had nothing
    /// to do with it: decoding and planning to `relocate`, building and
    /// storing to nothing at all, because they happen after `emit_ms` stops.
    /// So the headline read "78% of relocations reused" while an edit relink
    /// with the cache on was 5.5 ms *slower* than one with it off, and no
    /// number in the record could say where the 5.5 ms went.
    ///
    /// A cache is a trade, and a trade shows up in a report as two numbers.
    pub cache_load_ms: f64,
    /// Whether the previous cache came from this process rather than from disk.
    pub cache_held: bool,
    /// Building the symbol-address table, and diffing it against the previous
    /// link's. This is the precondition for skipping relocation work, so it is
    /// measured before anything is built on top of it: if deciding what to skip
    /// costs what doing it costs, there is nothing here.
    pub address_table_ms: f64,
    pub address_diff_ms: f64,
    /// Inside `synthetic`: repairing `__eh_frame`, filling the indirect tables,
    /// and rebuilding `__unwind_info`.
    pub eh_frame_ms: f64,
    pub tables_ms: f64,
    pub unwind_ms: f64,
    /// How many addresses this link changed, out of how many there are.
    pub changed_addresses: u64,
    pub total_addresses: u64,
    pub cache_plan_ms: f64,
    pub cache_build_ms: f64,
    pub cache_store_ms: f64,
    /// Bytes the cache read and wrote, for the same reason.
    pub cache_bytes_read: u64,
    pub cache_bytes_written: u64,
    /// Contributions that kept the address the previous link gave them, and
    /// those that did not.
    ///
    /// The number "relocations reused" is standing in for. A hit rate is a
    /// property of the cache; this is a property of the *layout*, and it is the
    /// one that has to hold first — a reused relocation is only correct because
    /// what it points at did not move. Zero moved contributions among unchanged
    /// inputs is the invariant; anything else is a hit rate that happens to be
    /// high today.
    pub contributions_retained: u64,
    pub contributions_moved: u64,
    /// Contributions that moved **despite their input not having changed**.
    ///
    /// The invariant, as against the statistic above. A contribution of an
    /// edited crate is entitled to move — it may not fit where it was. One
    /// belonging to a file that is byte-for-byte what it was last link has no
    /// such excuse, and every one of them invalidates every relocation
    /// pointing at it. This number is the acceptance criterion for retained
    /// placement, and it is meant to be zero.
    pub contributions_moved_unchanged: u64,
    /// How long parsing the SDK's `.tbd` stubs took, on its own thread.
    ///
    /// Compare against `read_and_parse_ms`, which is the whole overlapped
    /// stage: if this is the smaller of the two it costs nothing, and if it is
    /// the larger it is setting the pace.
    pub stub_parse_ms: f64,
    /// Where the time inside `emit` went.
    pub emit_breakdown: blinker_output::EmitTimings,
    /// Inside `relocate`, which is a stage name that covers five different
    /// jobs. Splitting `emit` the same way found 4.4 ms of one hash sitting
    /// next to a better one (finding 102); a stage this composite has no
    /// business being a single number either.
    pub address_map_ms: f64,
    pub contents_ms: f64,
    pub synthetic_ms: f64,
    pub apply_ms: f64,
    /// Building the output symbol table and the debug map, which sit between
    /// `relocate` and `emit` and were in neither.
    pub symbols_ms: f64,
    /// Surveying every relocation to discover which GOT, stub and TLV slots the
    /// link needs. Between `resolve` and `layout`, and in neither.
    pub survey_ms: f64,
    /// Inputs served from memory, and inputs that had to be read. Both zero
    /// for a one-shot link, which holds nothing.
    /// Whether dead-stripping reused the previous link's answer whole.
    pub reused_strip: bool,
    pub inputs_held: u64,
    pub inputs_read: u64,
    /// Whether the archive extraction order was replayed rather than computed.
    pub replayed_extraction: bool,
    /// Whether symbol resolution was held rather than redone.
    pub held_resolution: bool,
    /// Inputs whose symbol interface moved, and the first one that did.
    pub interface_changes: u64,
    pub first_interface_change: Option<PathBuf>,
}

impl std::fmt::Display for LinkTimings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pct = |v: f64| {
            if self.total_ms > 0.0 {
                v / self.total_ms * 100.0
            } else {
                0.0
            }
        };
        if self.total_objects > 0 {
            let share = if self.total_relocations > 0 {
                self.reused_relocations as f64 / self.total_relocations as f64 * 100.0
            } else {
                0.0
            };
            writeln!(
                f,
                "  reused      {:7} of {} objects, {:.0}% of relocations{}",
                self.reused_objects,
                self.total_objects,
                share,
                if self.reused_finished_image {
                    " (whole image)"
                } else {
                    ""
                }
            )?;
        }
        writeln!(
            f,
            "  read+parse  {:7.1} ms  {:5.1}%",
            self.read_and_parse_ms,
            pct(self.read_and_parse_ms)
        )?;
        writeln!(
            f,
            "  resolve     {:7.1} ms  {:5.1}%",
            self.resolve_ms,
            pct(self.resolve_ms)
        )?;
        writeln!(
            f,
            "  layout      {:7.1} ms  {:5.1}%",
            self.layout_probe_ms,
            pct(self.layout_probe_ms)
        )?;
        if self.dead_strip_ms > 0.0 {
            writeln!(
                f,
                "  dead-strip  {:7.1} ms  {:5.1}%   {} KB of __text removed{}",
                self.dead_strip_ms,
                pct(self.dead_strip_ms),
                self.stripped_bytes / 1024,
                if self.revived_atoms > 0 {
                    format!(", {} revived", self.revived_atoms)
                } else {
                    String::new()
                }
            )?;
        }
        writeln!(
            f,
            "  relocate    {:7.1} ms  {:5.1}%",
            self.relocate_ms,
            pct(self.relocate_ms)
        )?;
        writeln!(
            f,
            "  emit+sign   {:7.1} ms  {:5.1}%",
            self.emit_ms,
            pct(self.emit_ms)
        )?;
        write!(f, "  total       {:7.1} ms", self.total_ms)
    }
}

/// Link, reporting how long each stage took.
pub fn link_timed(request: &LinkRequest) -> Result<(Image, LinkTimings), LinkError> {
    let mut timings = LinkTimings::default();
    let image = link_inner(request, &mut timings, &mut Session::default())?;
    Ok((image, timings))
}

pub mod reachability;

/// Analyse how much `__text` the program can reach, without linking.
///
/// Reports what dead-stripping would remove. Separate from [`link`] because it
/// changes no output: the model is checked against a linker that already
/// strips correctly before anything is rebuilt around it (finding 70).
pub fn reachability_report(request: &LinkRequest) -> Result<reachability::Report, LinkError> {
    let objects = load_objects(&request.objects, &mut Session::default())?;
    Ok(reachability::analyse(&objects, &request.entry_symbol))
}

/// What to link.
#[derive(Debug, Clone)]
pub struct LinkRequest {
    pub objects: Vec<PathBuf>,
    /// Symbol the image enters at.
    pub entry_symbol: String,
    /// Identifier embedded in the ad-hoc signature; conventionally the output
    /// file's base name.
    pub identifier: String,
    pub dylibs: Vec<Dylib>,
    /// `.tbd` stubs describing what the dylibs export.
    pub stub_libraries: Vec<PathBuf>,
    /// Where the incremental cache lives, when one is wanted.
    ///
    /// `None` disables the cache entirely, which is what every test that is
    /// not *about* the cache should use: a link that silently reads state from
    /// a previous run is a link whose result depends on history, and that is
    /// the one thing a correctness test must not tolerate.
    pub cache_path: Option<PathBuf>,
    /// Whether to discard input nothing reaches.
    pub dead_strip: bool,
    /// Whether to count how many contributions kept their address.
    ///
    /// It is a diagnostic and it is not free: it walks every contribution and
    /// probes every input, and probing an input whose path does not identify it
    /// means hashing its contents. Measured at 0.46 ms of a 20 ms link — small,
    /// and entirely paid by builds that will never read the number. So the
    /// driver turns it on when something has asked for diagnostics, and a
    /// production link does not compute it.
    pub count_placement: bool,
    /// Leave padding after each contribution, so an edit that grows one does
    /// not move everything after it.
    ///
    /// A property of the *request*, not of whether a cache is being written.
    /// Tying it to the cache made a cold link and a cached one lay out
    /// differently, which breaks the equivalence the whole design rests on:
    /// an incremental output must be the output a cold link would have
    /// produced.
    pub stable_layout: bool,
    /// Whether to record per-object relocation state and reuse it.
    ///
    /// Off by default, and separately from `cache_path`, because the two
    /// things `--blinker-cache` used to mean have opposite economics:
    ///
    /// - **Replaying an unchanged image** skips the entire linker. It is the
    ///   whole of the no-op rebuild case and it costs a fingerprint check.
    /// - **Reusing individual objects' relocated bytes** skips one stage of
    ///   six, and to be able to do it at all every link must record what each
    ///   object read. That recording *doubles* relocation: 5.6 ms to 11.9 on a
    ///   681-object link, against a stage that is 5.6 ms in total. Measured end
    ///   to end, asking for it cost 10.2 ms to save at most 5.6 — a loss at any
    ///   hit rate (finding 94).
    ///
    /// So an ordinary incremental link gets the first and not the second. This
    /// stays because the machinery is correct and is the scaffolding the
    /// retained-placement allocator will reuse; it is off because it is not
    /// yet worth its price, and that is a measurement rather than an opinion.
    pub reuse_relocations: bool,
}

impl LinkRequest {
    pub fn new(objects: Vec<PathBuf>) -> Self {
        LinkRequest {
            objects,
            entry_symbol: "_main".to_string(),
            identifier: "a.out".to_string(),
            dylibs: vec![Dylib::lib_system()],
            stub_libraries: default_stub_library().into_iter().collect(),
            cache_path: None,
            dead_strip: false,
            count_placement: false,
            stable_layout: false,
            reuse_relocations: false,
        }
    }

    /// Cache this link's relocated output under `path`, and reuse what a
    /// previous link left there.
    /// Discard code and data nothing reaches.
    ///
    /// Off unless asked for, as in `ld`: a link that drops input the user did
    /// not ask it to drop is one whose output cannot be explained from its
    /// command line. `-dead_strip` turns it on, which is what rustc passes.
    pub fn dead_stripped(mut self, on: bool) -> Self {
        self.dead_strip = on;
        self
    }

    /// Ask for the placement invariant to be counted; see `count_placement`.
    pub fn counting_placement(mut self, on: bool) -> Self {
        self.count_placement = on;
        self
    }

    /// Reserve slack after each contribution so later links can reuse more.
    pub fn with_stable_layout(mut self, on: bool) -> Self {
        self.stable_layout = on;
        self
    }

    pub fn cached_at(mut self, path: PathBuf) -> Self {
        self.cache_path = Some(path);
        self
    }

    /// Also record and reuse per-object relocation state. See
    /// [`LinkRequest::reuse_relocations`] for why this is not the default.
    pub fn reusing_relocations(mut self, on: bool) -> Self {
        self.reuse_relocations = on;
        self
    }

    pub fn identifier(mut self, identifier: &str) -> Self {
        self.identifier = identifier.to_string();
        self
    }

    /// Use a `.tbd` stub library as the source of importable symbols.
    /// Replace the stub libraries with the ones the command line resolved.
    ///
    /// Additive rather than replacing when the list is empty, so a caller that
    /// resolved nothing keeps the default libSystem — an in-process API user
    /// linking a pure-Rust program passes no `-l` at all.
    pub fn stub_libraries(mut self, paths: Vec<PathBuf>) -> Self {
        if !paths.is_empty() {
            self.stub_libraries = paths;
        }
        self
    }

    pub fn stub_library(mut self, path: PathBuf) -> Self {
        self.stub_libraries.push(path);
        self
    }

    /// Every symbol the stub libraries export for this target.
    ///
    /// `None` when no stub library was supplied, which is different from an
    /// empty set: with no library, nothing can be imported and every
    /// undefined reference is an error.
    fn dynamic_symbols(&self) -> Option<libraries::StubExports> {
        if self.stub_libraries.is_empty() {
            return None;
        }
        let mut all = libraries::StubExports::default();
        for path in &self.stub_libraries {
            let Ok(file) = blinker_tbd::parse_tbd_file(path) else {
                continue;
            };
            // Attributed to the file's *own* install name, not to whichever
            // sub-document a re-exported symbol was written in. `libSystem`
            // re-exports forty libraries; `_malloc` lives in
            // `libsystem_malloc.dylib` and binds against libSystem, because
            // libSystem is what the image loads and what the ordinal names.
            let Some(install_name) = file.primary().map(|d| d.install_name.clone()) else {
                continue;
            };
            let library = all.library(&install_name);
            for name in file.exported_symbols(blinker_tbd::Target::aarch64_macos()) {
                all.export(library, name);
            }
        }
        Some(all)
    }
}

/// Where the SDK keeps `libSystem`'s stub, if it can be found.
///
/// # Why this is cached
///
/// Asking `xcrun` costs **14 ms** — a third of a 40 ms link, spent spawning a
/// process to learn a path that cannot change while blinker is running. It was
/// called from `LinkRequest::new`, so every link paid it, and because it
/// happened before the link's own timers started it appeared as unexplained
/// overhead rather than as a phase.
///
/// `SDKROOT` is honoured first: the compiler driver sets it, so in a real
/// build the answer is already in the environment and no process need be
/// spawned at all. `xcrun` remains the fallback, because the SDK genuinely
/// does move between Xcode versions and the Command Line Tools.
pub fn default_stub_library() -> Option<PathBuf> {
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED.get_or_init(discover_stub_library).clone()
}

/// Where `xcode-select` records the active developer directory.
///
/// `xcode-select -p` prints the target of this symlink and `xcrun` resolves the
/// SDK beneath it, so reading the link answers the same question without
/// spawning either.
const XCODE_SELECT_LINK: &str = "/var/db/xcode_select_link";

/// Where the Command Line Tools install, when Xcode itself is not present.
const COMMAND_LINE_TOOLS: &str = "/Library/Developer/CommandLineTools";

/// The two layouts an SDK sits in under a developer directory: Xcode's, which
/// is organised by platform, and the Command Line Tools', which is not.
const SDK_UNDER_DEVELOPER_DIR: &[&str] = &[
    "Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk",
    "SDKs/MacOSX.sdk",
];

fn discover_stub_library() -> Option<PathBuf> {
    let sdk = sdk_root()?;
    let path = sdk.join("usr/lib/libSystem.tbd");
    path.exists().then_some(path)
}

/// The macOS SDK this link resolves libraries and frameworks against.
///
/// Cached for the same reason `default_stub_library` is: asking `xcrun` costs
/// 14 ms, and the answer cannot change while blinker is running.
pub fn sdk_root() -> Option<PathBuf> {
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED.get_or_init(discover_sdk_root).clone()
}

fn discover_sdk_root() -> Option<PathBuf> {
    // An SDK is recognised by containing the one library every link needs. A
    // directory that merely exists is not an SDK, and accepting one turns a
    // wrong `SDKROOT` into "undefined _printf" rather than into a miss here.
    let sdk_at = |sdk: &Path| sdk.join("usr/lib/libSystem.tbd").exists();
    let sdk_under = |developer: &Path| {
        SDK_UNDER_DEVELOPER_DIR
            .iter()
            .map(|suffix| developer.join(suffix))
            .find(|path| sdk_at(path))
    };

    // The compiler driver sets `SDKROOT` when it knows the answer, so in some
    // builds it is already in the environment.
    if let Ok(sdk) = std::env::var("SDKROOT") {
        let sdk = PathBuf::from(sdk);
        if sdk_at(&sdk) {
            return Some(sdk);
        }
    }
    if let Ok(developer) = std::env::var("DEVELOPER_DIR") {
        if let Some(path) = sdk_under(Path::new(&developer)) {
            return Some(path);
        }
    }
    // rustc sets neither, so in a real build this is the one that answers.
    // Reading a symlink rather than spawning `xcrun` is worth 7.5 ms — 30% of
    // a link's wall time, spent before its own timers start, which is why it
    // read as unexplained overhead rather than as a phase.
    for developer in [Path::new(XCODE_SELECT_LINK), Path::new(COMMAND_LINE_TOOLS)] {
        if let Some(path) = sdk_under(developer) {
            return Some(path);
        }
    }

    // The SDK genuinely does move — between Xcode versions, betas, and the
    // Command Line Tools — so the authority remains `xcrun`. It is the
    // fallback rather than the answer.
    let output = std::process::Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sdk = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    sdk_at(&sdk).then_some(sdk)
}

/// An object file and the bytes it was parsed from.
///
/// The bytes are kept because section *content* is read from them later;
/// `ParsedObject` describes where the content is, not what it is.
/// The bytes an object was parsed from, shared rather than copied.
///
/// An archive member's bytes are a window into the archive's own buffer, which
/// the link already holds and keeps for as long as the members do. Owning a
/// second copy per member cost **13 MB of `memcpy` on a 47-object Rust link**,
/// for data that was already in memory a few hundred bytes away.
///
/// `Deref` rather than an accessor so that reading these bytes stays spelled
/// the way it was: this is a change of ownership, not of meaning.
#[derive(Clone)]
struct SourceBytes {
    whole: std::sync::Arc<mapping::Backing>,
    range: std::ops::Range<usize>,
}

impl SourceBytes {
    fn whole(bytes: mapping::Backing) -> SourceBytes {
        let range = 0..bytes.len();
        SourceBytes {
            whole: std::sync::Arc::new(bytes),
            range,
        }
    }

    /// A whole file whose bytes are already shared.
    fn whole_shared(whole: &std::sync::Arc<mapping::Backing>) -> SourceBytes {
        let range = 0..whole.len();
        SourceBytes {
            whole: std::sync::Arc::clone(whole),
            range,
        }
    }

    /// The shared buffer behind this window.
    fn backing(&self) -> &std::sync::Arc<mapping::Backing> {
        &self.whole
    }

    /// Which bytes of that buffer this window covers.
    fn range(&self) -> std::ops::Range<usize> {
        self.range.clone()
    }

    /// A window into an archive, sharing its buffer.
    fn window(
        whole: &std::sync::Arc<mapping::Backing>,
        range: std::ops::Range<usize>,
    ) -> SourceBytes {
        SourceBytes {
            whole: std::sync::Arc::clone(whole),
            range,
        }
    }
}

impl std::ops::Deref for SourceBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // The range came from the archive index, which is bounds-checked when
        // the member is read; falling back to empty keeps a corrupt one from
        // panicking here rather than being reported where it is read.
        self.whole.get(self.range.clone()).unwrap_or(&[])
    }
}

struct LoadedObject {
    /// Shared, because a resident linker parses an unchanged input once and
    /// hands the same answer to every later link. Nothing mutates a
    /// `ParsedObject` after it is parsed, which is what makes that sound.
    parsed: std::sync::Arc<ParsedObject>,
    data: SourceBytes,
    /// The file this link read it from.
    ///
    /// Not `parsed.metadata.path`, which is the path it was *first* parsed
    /// from and is baked into the shared parse. Those differ exactly when the
    /// same bytes arrive under a new name, which rustc does to every object of
    /// a recompiled crate on every debug build (finding 144) — and reusing the
    /// parse across that rename is the whole point of the content index.
    /// Anything that names the file to a consumer — the debug map's `OSO`,
    /// the cache's record of what it read — must use this one.
    path: std::sync::Arc<Path>,
    /// The archive member name this link read it under, for the same reason
    /// `path` exists and with the same rule: never `parsed.metadata.member`.
    ///
    /// rustc renames every codegen unit of a recompiled crate, so an rlib whose
    /// members are byte-for-byte what they were still lists them under new
    /// names. The parse held for those bytes carries whatever name they had the
    /// first time; the `OSO` stab has to name the member that exists *now*.
    member: Option<std::sync::Arc<str>>,
    /// Whether the session proved these exact bytes unchanged since the link
    /// that last parsed them.
    ///
    /// Set only where [`Session::member`] answered, and that answer is a
    /// `memcmp` against the bytes the held parse was made from — so it means
    /// the object's content is what the *previous* link relocated, not merely
    /// that a parse was reused.
    ///
    /// It exists because the reuse plan's other way of asking is wrong for a
    /// member. `InputKey` describes a *file*, and a member's file is its
    /// archive: recompiling a crate downstream of an edit rewrites the rlib,
    /// so the archive key moves and every member inside it looks changed. On a
    /// rust-analyzer relink that rejected 3,370 of 5,637 objects — 3,370 whose
    /// bytes this flag had already proven identical, and which were relocated
    /// again to produce the bytes the cache was holding.
    unchanged: bool,
}

/// Sections that exist for the linker's benefit and must not reach the output.
///
/// `__LD,__compact_unwind` is the clearest case: it is *input* to unwind-table
/// generation, and ld64 consumes it to synthesise `__TEXT,__unwind_info`.
/// Copying it through would put a linker-internal segment in the image.
fn is_linker_internal(section: &InputSection) -> bool {
    section.segment == "__LD"
        || section.kind == SectionKind::Debug
        || section.name == "__compact_unwind"
}

/// Section id of the synthesised `__unwind_info`.
const UNWIND_SECTION: SectionId = SectionId(3);

/// Bytes in one `__LD,__compact_unwind` record.
const COMPACT_UNWIND_RECORD: u64 = 32;

/// Field offsets within a compact unwind record.
const CU_FUNCTION: u64 = 0;
const CU_LENGTH: u64 = 8;
const CU_ENCODING: u64 = 12;
const CU_PERSONALITY: u64 = 16;
const CU_LSDA: u64 = 24;

/// `UNWIND_ARM64_MODE_MASK` and the DWARF mode value.
const UNWIND_MODE_MASK: u32 = 0x0f00_0000;
const UNWIND_MODE_DWARF: u32 = 0x0300_0000;
/// Low 24 bits of a DWARF-mode encoding hold the FDE offset.
const UNWIND_DWARF_OFFSET_MASK: u32 = 0x00ff_ffff;

/// Read a ULEB128 value, returning it and the position after it.
fn uleb128(data: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let (mut value, mut shift) = (0u64, 0u32);
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, pos));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Skip a SLEB128 value, returning the position after it.
fn skip_sleb128(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        if byte & 0x80 == 0 {
            return Some(pos);
        }
    }
}

/// `DW_EH_PE_indirect` — the encoded value addresses a *slot* holding the
/// real pointer, rather than being the pointer.
const DW_EH_PE_INDIRECT: u8 = 0x80;

/// Where each CIE stores its personality reference.
///
/// # Why this has to parse the augmentation
///
/// A CIE names its personality routine only in its augmentation data, and
/// nothing else in the object says which relocation that is. blinker's first
/// attempt at this keyed on personality symbols collected from
/// `__compact_unwind` — which, in DWARF mode, contains none (finding 31), so
/// the code was inert (finding 49).
///
/// The layout being walked, per the DWARF CFI format:
///
/// ```text
/// length, CIE id (0), version, augmentation string,
/// code alignment (ULEB), data alignment (SLEB), return register,
/// if augmentation begins with 'z': augmentation length (ULEB), then one
///   entry per remaining character — 'P' is an encoding byte followed by the
///   personality pointer, 'L' and 'R' are a single byte each.
/// ```
///
/// Returns the offsets, *within each input section*, of personality fields
/// that use an indirect encoding — the only ones that must name a GOT slot.
fn eh_frame_personality_fields(object: &LoadedObject, section: &InputSection) -> HashSet<u64> {
    let mut fields = HashSet::default();
    let Some(file_offset) = section.file_offset else {
        return fields;
    };
    let data = &object.data;
    let base = file_offset as usize;

    let mut position = 0u64;
    while position + 8 <= section.size {
        let at = base + position as usize;
        let Some(bytes) = data.get(at..at + 8) else {
            break;
        };
        let length = u32::from_le_bytes(bytes[0..4].try_into().expect("4")) as u64;
        if length == 0 {
            break;
        }
        let id = u32::from_le_bytes(bytes[4..8].try_into().expect("4"));

        // Only CIEs carry an augmentation; an FDE's second word is the
        // distance back to its CIE.
        if id == 0 {
            if let Some(offset) = personality_field_in_cie(data, at + 8, position + 8) {
                fields.insert(offset);
            }
        }
        position += 4 + length;
    }
    fields
}

/// The section-relative offset of this CIE's indirect personality field.
fn personality_field_in_cie(data: &[u8], start: usize, start_offset: u64) -> Option<u64> {
    // `at` indexes the file; `offset` is the same position expressed relative
    // to the section, which is what a relocation records. They advance
    // together, so one delta keeps both correct.
    let mut at = start;
    let section_delta = start_offset.wrapping_sub(start as u64);

    let _version = *data.get(at)?;
    at += 1;

    let mut augmentation = Vec::new();
    while *data.get(at)? != 0 {
        augmentation.push(*data.get(at)?);
        at += 1;
    }
    at += 1; // the NUL

    if augmentation.first() != Some(&b'z') {
        return None;
    }

    (_, at) = uleb128(data, at)?; // code alignment factor
    at = skip_sleb128(data, at)?; // data alignment factor
    (_, at) = uleb128(data, at)?; // return address register
    (_, at) = uleb128(data, at)?; // augmentation data length

    for entry in &augmentation[1..] {
        match entry {
            b'P' => {
                let encoding = *data.get(at)?;
                at += 1;
                // The field starts here. Only an indirect encoding needs a GOT
                // slot; a direct one genuinely wants the symbol's address.
                return (encoding & DW_EH_PE_INDIRECT != 0)
                    .then_some((at as u64).wrapping_add(section_delta));
            }
            b'L' | b'R' => at += 1,
            _ => return None,
        }
    }
    None
}

/// Map each function to the offset of its FDE within the output `__eh_frame`.
///
/// # Why the records are walked but not decoded
///
/// A DWARF-mode compact unwind encoding is a *pointer*: its low 24 bits are
/// the offset of the function's FDE in `__eh_frame`, and the unwinder follows
/// it to the real description. Producing those offsets needs to know where
/// each FDE begins and which function it covers.
///
/// Finding the boundaries needs only the length field every record starts
/// with. Finding the *function* would normally mean decoding the CIE's
/// augmentation string to learn how the FDE's `PC begin` field is encoded —
/// but in a relocatable object that field carries a **relocation**, so the
/// answer is already available from the relocation list, in the same form the
/// rest of the linker uses. Decoding DWARF pointer encodings here would be
/// re-deriving something the object already states.
/// Each contributing section's offset within one output section.
///
/// `OutputSection::address_of` answers this with a linear `find` over every
/// contribution, which is fine once and quadratic in a loop — and both callers
/// are loops over every object with an `__eh_frame` section, against an output
/// section holding a contribution from every one of them. One pass builds all
/// the answers.
///
/// The offset rather than the address, because both callers subtract
/// `vm_address` from the address immediately.
fn contribution_offsets(output: &blinker_layout::OutputSection) -> HashMap<(u32, u32), u64> {
    output
        .contributions
        .iter()
        .map(|c| ((c.object.0, c.section.0), c.offset))
        .collect()
}

fn eh_frame_fde_offsets(
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    image: &Image,
    placed: &Placed,
    addresses: &AddressMap,
    strip: &Strip,
) -> HashMap<u64, u32> {
    let mut offsets = HashMap::default();

    let Some(output) = image
        .layout
        .sections
        .iter()
        .find(|s| s.name == "__eh_frame")
    else {
        return offsets;
    };
    let offsets_of = contribution_offsets(output);

    // Per chunk on every core, merged in chunk order. Merging in order matters
    // even though the key is an address: two records claiming one function is
    // possible, and a later object won sequentially, so it must win here.
    let chunks = crate::parallel::map_chunks(objects, |base, chunk| {
        fde_offsets_of(chunk, base, interned, placed, addresses, strip, &offsets_of)
    });
    // Sized before it is filled. The merge is 265,308 entries and the map
    // started empty, which is eighteen doublings and every entry already in it
    // reinserted at each one — finding 135's pattern, in the tail of a pass
    // whose parallel half costs 6 ms.
    offsets.reserve(chunks.iter().map(HashMap::len).sum());
    for chunk in chunks {
        offsets.extend(chunk);
    }
    offsets
}

/// One chunk's FDE offsets. `base` is where the chunk starts in `objects`.
#[allow(clippy::too_many_arguments)]
fn fde_offsets_of(
    objects: &[LoadedObject],
    base: usize,
    interned: &[Arc<Vec<SymbolNameId>>],
    placed: &Placed,
    addresses: &AddressMap,
    strip: &Strip,
    offsets_of: &HashMap<(u32, u32), u64>,
) -> HashMap<u64, u32> {
    let mut offsets: HashMap<u64, u32> = HashMap::default();
    for (at, object) in objects.iter().enumerate() {
        let ids = &interned[base + at];
        for section in &object.parsed.sections {
            if section.name != "__eh_frame" {
                continue;
            }
            let Some(file_offset) = section.file_offset else {
                continue;
            };
            // Where this object's records begin within the output section.
            let Some(&chunk_offset) = offsets_of.get(&(object.parsed.id.0, section.id.0)) else {
                continue;
            };

            // This section's relocations, in offset order, walked in lockstep
            // with the records below rather than hashed into a map.
            //
            // Two costs went away with the map. It resolved `target_address`
            // for *every* relocation in `__eh_frame` — the LSDA pointers, the
            // personality references, the CIE back-pointers — when the only one
            // ever read back is the FDE's `PC begin`. And it hashed and stored
            // every result to answer lookups that arrive in increasing order,
            // which a cursor answers without hashing anything.
            //
            // `eh_frame_fde_offsets` was 1.00 ms of `fill_unwind_info`'s 1.25;
            // encoding the actual table was 0.02.
            let mut relocations: Vec<&blinker_macho::InputRelocation> =
                object.parsed.relocations_for(section.id).iter().collect();
            // Stable, and the *last* match wins below. Both matter: a `PC
            // begin` field is a SUBTRACTOR pair — two relocations at one offset,
            // the anchor then the function — and the map this replaced kept
            // whichever was inserted last, which is the function. Taking the
            // first instead produced a binary that linked, ran, and unwound
            // into the wrong place; `a_caught_panic_still_runs_destructors_
            // after_stripping` caught it.
            relocations.sort_by_key(|r| r.offset);
            let mut cursor = 0usize;

            let mut position = 0u64;
            while position + 8 <= section.size {
                let at = (file_offset + position) as usize;
                let Some(length_bytes) = object.data.get(at..at + 4) else {
                    break;
                };
                let length = u32::from_le_bytes(length_bytes.try_into().expect("4 bytes")) as u64;
                // A zero length terminates the section.
                if length == 0 {
                    break;
                }
                let record_size = 4 + length;

                let id = object
                    .data
                    .get(at + 4..at + 8)
                    .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                    .unwrap_or(0);

                // Zero identifies a CIE; anything else is an FDE whose value is
                // the distance back to its CIE.
                // Zero identifies a CIE; anything else is an FDE whose value is
                // the distance back to its CIE. A record whose bytes were
                // stripped has no offset to publish, and no live function is
                // looking for one.
                if id != 0 {
                    // `PC begin` immediately follows the CIE pointer. Records
                    // are walked in increasing offset, so the cursor only ever
                    // moves forward.
                    let want = position + 8;
                    while relocations.get(cursor).is_some_and(|r| r.offset < want) {
                        cursor += 1;
                    }
                    let mut at_pc_begin = None;
                    let mut scan = cursor;
                    while let Some(relocation) = relocations.get(scan).filter(|r| r.offset == want)
                    {
                        at_pc_begin = Some(relocation);
                        scan += 1;
                    }
                    if let (Some(relocation), Some(moved)) = (
                        at_pc_begin,
                        strip.remap(object.parsed.id, section.id, position),
                    ) {
                        if let Ok(function) =
                            target_address(object, ids, placed, addresses, relocation.target)
                        {
                            offsets.insert(function, (chunk_offset + moved) as u32);
                        }
                    }
                }

                position += record_size;
            }
        }
    }
    offsets
}

/// Everything a single pass over the relocations can decide.
///
/// These four questions used to be four separate walks over every relocation
/// of every object — plus two more for the output symbol table and the
/// undefined set. Profiling put that repeated traversal at 31% of the link,
/// larger than any named stage, so they are answered together.
///
/// The order still matters: stubs are only needed for symbols that turned out
/// to be imports, so the caller supplies that set.
#[derive(Default)]
struct RelocationSurvey {
    got: Vec<TableEntry>,
    tlv: Vec<TableEntry>,
    stubs: Vec<String>,
    personalities: Vec<TableEntry>,
}

fn survey_relocations(
    objects: &[LoadedObject],
    imports: &[String],
    strip: &Strip,
) -> RelocationSurvey {
    let imported: HashSet<&str> = imports.iter().map(String::as_str).collect();

    // Each chunk surveys its own objects and reports what it saw first, in its
    // own order; the global "have I seen this name" question is answered once,
    // below, walking the chunks in order. That keeps the answer identical to
    // the sequential one — a name's place in `got` is where the first object
    // that wanted it sits — while the per-relocation work, which is all of the
    // cost, runs on every core.
    let surveyed =
        crate::parallel::map_chunks(objects, |_, chunk| survey_chunk(chunk, &imported, strip));

    let mut survey = RelocationSurvey::default();
    // Borrowed, because these are asked about once per *candidate* and only
    // answered "new" once per *name*. The names live in the parsed objects,
    // which outlive this call.
    let (mut got_seen, mut tlv_seen, mut stub_seen, mut personality_seen): (
        HashSet<&str>,
        HashSet<&str>,
        HashSet<&str>,
        HashSet<&str>,
    ) = (
        HashSet::default(),
        HashSet::default(),
        HashSet::default(),
        HashSet::default(),
    );
    for chunk in &surveyed {
        for entry in &chunk.got {
            if got_seen.insert(entry.name.as_str()) {
                survey.got.push(entry.clone());
            }
        }
        for entry in &chunk.tlv {
            if tlv_seen.insert(entry.name.as_str()) {
                survey.tlv.push(entry.clone());
            }
        }
        for name in &chunk.stubs {
            if stub_seen.insert(name.as_str()) {
                survey.stubs.push(name.clone());
            }
        }
        for entry in &chunk.personalities {
            if personality_seen.insert(entry.name.as_str()) {
                survey.personalities.push(entry.clone());
            }
        }
    }

    survey.stubs.sort();
    survey
}

/// One chunk's candidates, in its own order and deduplicated within itself.
///
/// Deduplicating here as well as in the merge is not redundant: a name wanted
/// by a thousand objects of one chunk would otherwise be carried a thousand
/// times to a merge that keeps one.
fn survey_chunk(
    objects: &[LoadedObject],
    imported: &HashSet<&str>,
    strip: &Strip,
) -> RelocationSurvey {
    let mut survey = RelocationSurvey::default();
    let (mut got_seen, mut tlv_seen, mut stub_seen, mut personality_seen): (
        HashSet<&str>,
        HashSet<&str>,
        HashSet<&str>,
        HashSet<&str>,
    ) = (
        HashSet::default(),
        HashSet::default(),
        HashSet::default(),
        HashSet::default(),
    );

    for object in objects {
        // Which sections are `__compact_unwind`, so personality relocations can
        // be recognised without a second scan. A `Vec` and not a set: an object
        // has a couple of dozen sections and at most one of these, so a linear
        // scan of one element beats hashing — and the set this replaced was
        // `std`'s, which is SipHash, built 5,637 times.
        let unwind_sections: Vec<SectionId> = object
            .parsed
            .sections
            .iter()
            .filter(|s| s.name == "__compact_unwind")
            .map(|s| s.id)
            .collect();

        for relocation in &object.parsed.relocations {
            // A relocation in bytes that were stripped patches nothing, and an
            // indirection reserved for it would be a slot holding the address
            // of something no longer in the image — which `fill_got` reports
            // as an undefined symbol, because that is what it looks like.
            if strip
                .remap(object.parsed.id, relocation.section, relocation.offset)
                .is_none()
            {
                continue;
            }
            let RelocationTarget::Symbol(id) = relocation.target else {
                continue;
            };
            let Some(symbol) = object.parsed.symbol(id) else {
                continue;
            };
            let entry = || TableEntry {
                object: object.parsed.id,
                name: symbol.name.clone(),
            };

            if needs_got(relocation.kind) && got_seen.insert(symbol.name.as_str()) {
                survey.got.push(entry());
            }
            if needs_tlv(relocation.kind) && tlv_seen.insert(symbol.name.as_str()) {
                survey.tlv.push(entry());
            }
            if relocation.kind == Arm64RelocationKind::Branch26
                && imported.contains(symbol.name.as_str())
                && stub_seen.insert(symbol.name.as_str())
            {
                survey.stubs.push(symbol.name.clone());
            }
            if unwind_sections.contains(&relocation.section)
                && relocation.offset % COMPACT_UNWIND_RECORD == CU_PERSONALITY
                && personality_seen.insert(symbol.name.as_str())
            {
                survey.personalities.push(entry());
            }
        }
    }

    survey
}

/// Read the compact unwind records the compiler emitted.
///
/// Each record names a function, how long it is, how to restore its frame, and
/// optionally a personality routine and an LSDA. The pointers are *relocations*
/// rather than values — the object is not laid out yet — so the targets come
/// from the relocation list and the scalars from the section bytes.
// Eight, all distinct: the objects, their names, the layout, where each
// contribution went, the addresses, the strip, the GOT and the FDE offsets.
#[allow(clippy::too_many_arguments)]
fn compact_unwind_entries(
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    image: &Image,
    placed: &Placed,
    addresses: &AddressMap,
    strip: &Strip,
    got_slots: &HashMap<String, u64>,
    fde_offsets: &HashMap<u64, u32>,
) -> Vec<UnwindEntry> {
    let Some(text) = image.layout.segment("__TEXT") else {
        return Vec::new();
    };
    let image_base = text.vm_address;
    // One entry per record, and the record count is `size / 32` — known from
    // the layout without reading anything (135).
    let mut entries = Vec::with_capacity(
        objects
            .iter()
            .flat_map(|object| object.parsed.sections.iter())
            .filter(|section| section.name == "__compact_unwind")
            .map(|section| (section.size / COMPACT_UNWIND_RECORD) as usize)
            .sum(),
    );
    // Per chunk on every core, concatenated in object order — which is the
    // order the table's records are numbered in, and so reaches the output.
    let chunks = crate::parallel::map_chunks(objects, |base, chunk| {
        compact_unwind_entries_of(
            chunk,
            base,
            interned,
            image_base,
            placed,
            addresses,
            strip,
            got_slots,
            fde_offsets,
        )
    });
    for chunk in chunks {
        entries.extend(chunk);
    }
    entries
}

/// One chunk's compact-unwind records.
#[allow(clippy::too_many_arguments)]
fn compact_unwind_entries_of(
    objects: &[LoadedObject],
    base: usize,
    interned: &[Arc<Vec<SymbolNameId>>],
    image_base: u64,
    placed: &Placed,
    addresses: &AddressMap,
    strip: &Strip,
    got_slots: &HashMap<String, u64>,
    fde_offsets: &HashMap<u64, u32>,
) -> Vec<UnwindEntry> {
    let mut entries = Vec::new();
    // Reused across objects rather than allocated per object. There are 5,637
    // objects in a debug rust-analyzer link and each was building two maps
    // from empty to hold a few dozen entries.
    let mut targets: HashMap<(u64, u64), u64> = HashMap::default();
    let mut personality_names: HashMap<u64, String> = HashMap::default();

    for (at, object) in objects.iter().enumerate() {
        let ids = &interned[base + at];
        for section in &object.parsed.sections {
            if section.name != "__compact_unwind" {
                continue;
            }
            let Some(file_offset) = section.file_offset else {
                continue;
            };
            targets.clear();
            personality_names.clear();

            // Relocation targets, indexed by (record, field).
            //
            // The addend is stored **inline**, in the eight bytes being
            // patched, not in the relocation entry. Ignoring it made every
            // record that points into `__text` resolve to the same section
            // base: 469 functions collapsed to 17 distinct offsets, and the
            // unwinder was handed a table describing almost nothing.
            //
            // For a section target the inline value is an address in the
            // object's own coordinate space, so the offset within that section
            // has to be recovered before rebasing onto the output address.
            for relocation in object.parsed.relocations_for(section.id) {
                let Ok(base) = target_address(object, ids, placed, addresses, relocation.target)
                else {
                    continue;
                };

                let field_at = (file_offset + relocation.offset) as usize;
                let inline = object
                    .data
                    .get(field_at..field_at + 8)
                    .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
                    .unwrap_or(0);

                // This is the one place in the link that reads a *section*
                // relocation with a meaningful inline offset, so it is also
                // the one place where stripping has to be undone by hand: the
                // recorded offset is where the function used to be.
                let address = match relocation.target {
                    RelocationTarget::Section(id) => {
                        let origin = object.parsed.section(id).map(|s| s.vm_address).unwrap_or(0);
                        let Some(offset) =
                            strip.remap(object.parsed.id, id, inline.saturating_sub(origin))
                        else {
                            // The function it describes is gone, so this record
                            // has nothing to describe. Leaving the field absent
                            // is what drops the record below.
                            continue;
                        };
                        base + offset
                    }
                    // For a symbol the inline value is a plain addend, and the
                    // symbol's own address has already been remapped.
                    RelocationTarget::Symbol(_) => base + inline,
                };

                let record = relocation.offset / COMPACT_UNWIND_RECORD;
                let field = relocation.offset % COMPACT_UNWIND_RECORD;

                if field == CU_PERSONALITY {
                    if let RelocationTarget::Symbol(id) = relocation.target {
                        if let Some(symbol) = object.parsed.symbol(id) {
                            personality_names.insert(record, symbol.name.clone());
                        }
                    }
                }
                targets.insert((record, field), address);
            }

            let count = section.size / COMPACT_UNWIND_RECORD;
            for record in 0..count {
                let base = file_offset + record * COMPACT_UNWIND_RECORD;
                let read_u32 = |at: u64| -> Option<u32> {
                    let start = (base + at) as usize;
                    object
                        .data
                        .get(start..start + 4)
                        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                };

                // A record whose function pointer has no relocation refers to
                // a function that was dead-stripped or never placed.
                let Some(function) = targets.get(&(record, CU_FUNCTION)) else {
                    continue;
                };
                let Some(encoding) = read_u32(CU_ENCODING) else {
                    continue;
                };
                let _length = read_u32(CU_LENGTH);

                // A DWARF-mode encoding must carry the offset of this
                // function's FDE. Without it the unwinder follows a zero and
                // reads the start of `__eh_frame` for every function.
                let encoding = if encoding & UNWIND_MODE_MASK == UNWIND_MODE_DWARF {
                    let fde = fde_offsets.get(function).copied().unwrap_or(0);
                    (encoding & !UNWIND_DWARF_OFFSET_MASK) | (fde & UNWIND_DWARF_OFFSET_MASK)
                } else {
                    encoding
                };

                entries.push(UnwindEntry {
                    function_offset: (function - image_base) as u32,
                    encoding,
                    personality: personality_names
                        .get(&record)
                        .and_then(|name| got_slots.get(name))
                        .map(|slot| (slot - image_base) as u32),
                    lsda: targets
                        .get(&(record, CU_LSDA))
                        .map(|a| (a - image_base) as u32),
                });
            }
        }
    }
    entries
}

/// Section id of the synthesised `__thread_ptrs`.
const TLV_SECTION: SectionId = SectionId(2);

/// Relocation kinds that reach their target through a thread-local pointer.
fn needs_tlv(kind: Arm64RelocationKind) -> bool {
    matches!(
        kind,
        Arm64RelocationKind::TlvpLoadPage21 | Arm64RelocationKind::TlvpLoadPageOff12
    )
}

/// One slot of a synthesised pointer table.
///
/// The owning object is carried, not just the name. A thread-local or a
/// GOT target may be a **local** symbol, and locals are keyed per object
/// because two objects may legitimately define the same local name. Looking
/// one up under the linker's own synthetic object id finds nothing, and the
/// slot was then left zero — which is a null descriptor pointer, and a crash
/// on first use rather than a link error.
#[derive(Debug, Clone)]
struct TableEntry {
    object: ObjectId,
    name: String,
}

/// Section id of the synthesised `__stubs`.
const STUBS_SECTION: SectionId = SectionId(1);

/// Bytes per stub: three instructions.
const STUB_SIZE: u64 = 12;

/// `BR x16` — the last instruction of every stub.
const BR_X16: u32 = 0xD61F_0200;

/// Build one stub: load the GOT slot's contents and jump to it.
///
/// ```text
/// adrp x16, <got page>
/// ldr  x16, [x16, <got page offset>]
/// br   x16
/// ```
///
/// This is the *non-lazy* form. ld64's default stubs jump through
/// `__la_symbol_ptr` into `__stub_helper`, which calls `dyld_stub_binder` on
/// first use — three more synthesised sections and a second opcode stream, all
/// to defer work that a short-lived process never saves. Binding eagerly is
/// simpler and correct; the lazy path is an optimisation to add once there is
/// something to measure it against.
fn stub_code(stub_address: u64, got_slot: u64) -> [u8; 12] {
    let adrp = blinker_relocations::encode::encode_adrp(
        0x9000_0000 | 16, // ADRP with Rd = x16
        page_distance(stub_address, got_slot),
    )
    .expect("a GOT slot is within ADRP range of its stub");

    // LDR (unsigned offset, 64-bit): the immediate is scaled by 8.
    let offset_in_page = got_slot & 0xfff;
    let ldr = 0xF940_0000 | ((offset_in_page / 8) as u32) << 10 | (16 << 5) | 16;

    let mut code = [0u8; 12];
    code[0..4].copy_from_slice(&adrp.to_le_bytes());
    code[4..8].copy_from_slice(&ldr.to_le_bytes());
    code[8..12].copy_from_slice(&BR_X16.to_le_bytes());
    code
}

/// Distance in 4 KiB pages from one address to another.
fn page_distance(from: u64, to: u64) -> i64 {
    let from_page = (from & !0xfff) as i64;
    let to_page = (to & !0xfff) as i64;
    (to_page - from_page) >> 12
}

/// Object id used for sections the linker synthesises rather than reads.
///
/// Layout keys contributions by `(object, section)`, so synthesised content
/// needs an id that cannot collide with a real input's.
const SYNTHETIC_OBJECT: ObjectId = ObjectId(u32::MAX);

/// Section id of the synthesised `__got`.
const GOT_SECTION: SectionId = SectionId(0);

/// Section id of the synthesised `__common`.
const COMMON_SECTION: SectionId = SectionId(4);

/// A tentative definition, and the storage it needs.
struct Common {
    name: String,
    size: u64,
    alignment: u32,
}

/// Tentative definitions that no object defines outright.
///
/// C's `int arr[64];` at file scope is a *tentative* definition: every
/// translation unit that declares it emits a common symbol carrying the size,
/// and the linker allocates one shared object of the largest size requested.
/// A real definition anywhere — `int arr[64] = {0};` — wins outright, and the
/// commons become references to it.
///
/// Sorted by name so the section's contents do not depend on object order.
fn common_symbols(objects: &[LoadedObject]) -> Vec<Common> {
    // Nothing here unless something actually asks for a common symbol, and on a
    // Rust link nothing does — rustc does not emit them at all.
    //
    // Checked first because the alternative is what this used to do
    // unconditionally: build a set of every defined name in the program, tens of
    // thousands of string hashes, so that the loop below can ask whether each
    // common symbol is already defined. When there are no common symbols that
    // set answers no questions. It cost 0.78 ms of a 16 ms link to find nothing.
    //
    // The scan below is the same walk without the hashing: a comparison per
    // symbol against an enum.
    if !objects
        .iter()
        .flat_map(|o| &o.parsed.symbols)
        .any(|s| s.strength == SymbolStrength::Common)
    {
        return Vec::new();
    }

    let defined: HashSet<&str> = objects
        .iter()
        .flat_map(|o| &o.parsed.symbols)
        .filter(|s| s.strength != SymbolStrength::Common && s.strength.is_definition())
        .map(|s| s.name.as_str())
        .collect();

    let mut wanted: HashMap<&str, Common> = HashMap::default();
    for symbol in objects.iter().flat_map(|o| &o.parsed.symbols) {
        if symbol.strength != SymbolStrength::Common || defined.contains(symbol.name.as_str()) {
            continue;
        }
        // `value` holds the size for a common symbol, not an address. Two
        // translation units may ask for different sizes of the same name; the
        // largest wins, which is what makes a mismatched declaration merely
        // wasteful rather than a buffer overrun.
        let entry = wanted.entry(symbol.name.as_str()).or_insert(Common {
            name: symbol.name.clone(),
            size: 0,
            alignment: 0,
        });
        entry.size = entry.size.max(symbol.value);
        entry.alignment = entry.alignment.max(natural_alignment(symbol.value));
    }

    let mut commons: Vec<Common> = wanted.into_values().collect();
    commons.sort_by(|a, b| a.name.cmp(&b.name));
    commons
}

/// The alignment a common symbol of this size is entitled to.
///
/// Mach-O keeps the requested alignment in `n_desc`, which the parser does not
/// carry. Deriving it from the size is what `ld` does when the field is absent
/// and is never *less* aligned than the object needs: a 256-byte array gets
/// 8-byte alignment, a single `int` gets 4.
fn natural_alignment(size: u64) -> u32 {
    match size {
        0..=1 => 1,
        2..=3 => 2,
        4..=7 => 4,
        _ => 8,
    }
}

/// Where each common symbol landed, once `__common` has an address.
fn common_addresses<'a>(commons: &'a [Common], image: &Image) -> Vec<(&'a str, u64)> {
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__common") else {
        return Vec::new();
    };
    let mut at = section.vm_address;
    commons
        .iter()
        .map(|common| {
            at = at.next_multiple_of(common.alignment.max(1) as u64);
            let address = at;
            at += common.size;
            (common.name.as_str(), address)
        })
        .collect()
}

/// The size `__common` must reserve, laid out the same way as above.
fn common_section_size(commons: &[Common]) -> (u64, u32) {
    let mut size = 0u64;
    let mut alignment = 1u32;
    for common in commons {
        alignment = alignment.max(common.alignment);
        size = size.next_multiple_of(common.alignment.max(1) as u64) + common.size;
    }
    (size, alignment)
}

/// Bytes per GOT entry: one 64-bit pointer.
const GOT_ENTRY_SIZE: u64 = 8;

/// Relocation kinds that reach their target through the GOT.
fn needs_got(kind: Arm64RelocationKind) -> bool {
    matches!(
        kind,
        Arm64RelocationKind::GotLoadPage21
            | Arm64RelocationKind::GotLoadPageOff12
            | Arm64RelocationKind::PointerToGot
    )
}

/// Symbols referenced but not defined by any object.
///
/// These become dynamic imports. They are checked against the SDK's stub for
/// `libSystem` rather than assumed: an unresolved symbol that libSystem does
/// not export is a typo or a missing input, and silently binding it would turn
/// a link error into a crash at first call.
/// Every symbol referenced by the loaded objects and defined by none of them.
///
/// Called once per extraction round, so it is on the hot path of a cold link:
/// four rounds over tens of thousands of symbols was 1.8 ms of a 6.9 ms stage
/// (finding 68). Almost all of that was allocation — a first version cloned
/// every symbol name into the `defined` set, which copies the entire symbol
/// table of every object, four times, to answer a question about set
/// membership. The sets borrow now, and only the handful of names actually
/// returned are copied.
/// The names still looking for a definition, carried as members arrive.
///
/// Pulling an archive member can only *satisfy* the names it defines and
/// *raise* the ones it references, so recomputing the whole undefined set from
/// every symbol of every object each round is work proportional to the link
/// rather than to what changed.
///
/// This was built once before, measured on a 47-object fixture, found to be
/// worth nothing, and reverted (76) — the fixture had four rounds over 47
/// objects, where the scan is cheap and owning the names is not. On blinker's
/// own binary it is **eleven rounds over 921 objects, 22.7 ms**, and the
/// trade reverses completely. Same code, same reasoning, opposite answer,
/// because the workload was two orders of magnitude apart (77).
///
/// # Why both sets hold ids rather than names
///
/// The sets are built by walking every symbol of every object — a million of
/// them — and the names were `String`s so the set could outlive the objects
/// being pushed to. Holding interned ids instead costs an integer hash per
/// symbol and no allocation at all, because the interning already happened
/// when each object was parsed.
///
/// The order the ids were handed out in is *not* usable as an ordering: it is
/// whatever this process happened to intern first, so it differs between a
/// cold link and a warm one. `wanted` is therefore unordered here and sorted
/// by name where it is read, which is the point where the order reaches the
/// output — through which archive member gets pulled, and so numbered, first.
#[derive(Default)]
struct Frontier {
    defined: HashSet<SymbolNameId>,
    wanted: HashSet<SymbolNameId>,
}

impl Frontier {
    /// Fold one newly arrived object in, given its names as ids.
    ///
    /// The two loops must stay in this order — a symbol defined later in an
    /// object satisfies a reference made earlier in it — or one pass would
    /// leave the name wanted and pull a member to define what had just
    /// arrived.
    fn absorb(&mut self, object: &LoadedObject, ids: &[SymbolNameId]) {
        for symbol in &object.parsed.symbols {
            if !symbol.strength.is_definition() {
                continue;
            }
            // Newly defined, so anything waiting on it is satisfied. A name
            // already defined cannot still be wanted — the second loop below
            // never admits one — so the removal is skipped with it.
            let name = ids[symbol.id.0 as usize];
            if self.defined.insert(name) {
                self.wanted.remove(&name);
            }
        }
        for symbol in &object.parsed.symbols {
            if symbol.strength.is_definition() {
                continue;
            }
            let name = ids[symbol.id.0 as usize];
            if self.defined.contains(&name) {
                continue;
            }
            self.wanted.insert(name);
        }
    }
}

/// Names referenced by some object and defined by none, in name order.
///
/// Two passes over every symbol of every object — a million of them on a debug
/// rust-analyzer link — which is why they are over ids rather than strings.
/// The interning that produced the ids happened once, when each object was
/// first parsed; here the work is an integer hash per symbol.
fn undefined_references(
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    names: &SymbolNames,
) -> Vec<String> {
    let mut defined: HashSet<SymbolNameId> = HashSet::default();
    for (slot, object) in objects.iter().enumerate() {
        for symbol in &object.parsed.symbols {
            if symbol.strength.is_definition() {
                defined.insert(interned[slot][symbol.id.0 as usize]);
            }
        }
    }

    let mut undefined = Vec::new();
    let mut seen: HashSet<SymbolNameId> = HashSet::default();
    for (slot, object) in objects.iter().enumerate() {
        for symbol in &object.parsed.symbols {
            let name_id = interned[slot][symbol.id.0 as usize];
            if symbol.strength.is_definition() || defined.contains(&name_id) {
                continue;
            }
            if seen.insert(name_id) {
                undefined.push(name_id);
            }
        }
    }
    // Sorted by name and not by id: an id is an artefact of what this process
    // happened to intern first, and the order here reaches the output through
    // which archive member gets pulled — and therefore numbered — first.
    let mut out: Vec<String> = undefined
        .into_iter()
        .filter_map(|id| names.resolve(id).map(str::to_string))
        .collect();
    out.sort();
    out
}

/// Link the request into an image.
pub fn link(request: &LinkRequest) -> Result<Image, LinkError> {
    link_inner(
        request,
        &mut LinkTimings::default(),
        &mut Session::default(),
    )
}

macro_rules! gap {
    ($t:expr, $name:expr) => {{
        #[allow(clippy::print_stderr)]
        if std::env::var_os("BLINKER_GAP_PARTS").is_some() {
            let ms = $t.elapsed().as_secs_f64() * 1000.0;
            if ms > 1.0 {
                eprintln!("  gap {:>28}: {ms:6.1} ms", $name);
            }
        }
        $t = std::time::Instant::now();
    }};
}

fn link_inner(
    request: &LinkRequest,
    timings: &mut LinkTimings,
    session: &mut Session,
) -> Result<Image, LinkError> {
    let overall = std::time::Instant::now();
    #[allow(unused_mut)]
    let mut _gap = std::time::Instant::now();

    // The stub library's export list is a pure function of a file on disk —
    // it depends on nothing the objects produce — and parsing it costs 5.6 ms
    // of YAML for `libSystem.B.tbd` alone: a quarter of the link, spent
    // answering "which of these names does the system provide?".
    //
    // So it runs *alongside* reading the objects rather than after them, and
    // costs whatever it costs beyond the 5.7 ms that read already takes. No
    // cache, and therefore no new state whose staleness could change an
    // output.
    let step = std::time::Instant::now();
    session.begin(&request.objects);
    let held_stubs = session.stub_exports(&request.stub_libraries);
    let (objects, exported, stubs_were_parsed, stub_ms) = std::thread::scope(|scope| {
        let held = held_stubs.clone();
        let stub = scope.spawn(move || {
            let started = std::time::Instant::now();
            // Held from a previous link when the SDK has not moved. This is
            // the parse finding 100 measured at 6.0 ms of the 7.6 ms stage —
            // hidden behind reading the objects, and therefore worth nothing
            // to cache until the objects stop being read either. Both stop
            // here at once.
            // `fresh` says whether this had to be parsed, which decides
            // whether the session should be told. Storing unconditionally
            // marked the SDK as re-read on every link, and resolution — which
            // is only valid against the exports it was computed from — was
            // therefore never held.
            let (exported, fresh) = match held {
                Some(held) => (Some(held), false),
                None => (request.dynamic_symbols().map(std::sync::Arc::new), true),
            };
            (exported, fresh, elapsed_ms(started))
        });
        let objects = load_objects(&request.objects, session);
        let (exported, fresh, stub_ms) = stub.join().expect("the stub reader did not panic");
        (objects, exported, fresh, stub_ms)
    });
    if let (Some(exported), true) = (&exported, stubs_were_parsed) {
        session.store_stub_exports(&request.stub_libraries, std::sync::Arc::clone(exported));
    }
    // `exported` is kept as the shared value rather than cloned out: it is
    // consulted again long after resolution, because every bind opcode needs
    // the ordinal of the library its symbol came from.
    // One `LC_LOAD_DYLIB` per library that resolved, in ordinal order. With no
    // stub libraries at all the request's own list stands, which is what the
    // in-process API tests supply.
    let dylibs: Vec<Dylib> = match exported.as_deref() {
        Some(exports) if !exports.install_names().is_empty() => exports
            .install_names()
            .iter()
            .map(|install_name| Dylib {
                install_name: install_name.clone(),
                ..Dylib::lib_system()
            })
            .collect(),
        _ => request.dylibs.clone(),
    };
    // Both halves are timed, not just the pair. Overlapped work is only free
    // while it is the *shorter* half, and a profile cannot tell the difference:
    // it counts CPU across threads, so a stub parse that is entirely hidden
    // and one that sets the pace look identical in it. Finding 91 cached this
    // parse, measured nothing, and reverted — this is the number that says
    // whether that was the cache being useless or the cost being hidden.
    timings.stub_parse_ms = stub_ms;
    let objects = objects?;
    timings.read_and_parse_ms = elapsed_ms(step);
    gap!(_gap, "after read_and_parse");

    // Every object's names as ids, gathered once for the whole link. A held
    // object's vector was interned by the link that first parsed it, so this
    // is an `Arc` clone each; a newly read one pays for its own names and
    // nobody else's.
    let interned: Vec<Arc<Vec<SymbolNameId>>> = objects
        .iter()
        .map(|object| session.interned(&object.parsed))
        .collect();

    // Decided before anything is placed, because it changes how big every
    // contribution is. Everything downstream asks it where an input byte went.
    let step = std::time::Instant::now();
    let (strip, report, strip_timings) = if request.dead_strip {
        reachability::plan(&objects, &request.entry_symbol, session)
    } else {
        (
            Strip::none(),
            reachability::Report::default(),
            reachability::StripTimings::default(),
        )
    };
    // Derived facts about parses this link did not use are dropped here, and
    // with them the `Arc`s holding those parses alive. Without it a resident
    // linker's memory grows with every rebuild instead of with the program.
    let live_parses: crate::hashing::FastMap<usize, ()> = objects
        .iter()
        .map(|o| (std::sync::Arc::as_ptr(&o.parsed) as usize, ()))
        .collect();
    session.forget_unused_memos(&live_parses);

    timings.dead_strip_ms = elapsed_ms(step);
    gap!(_gap, "after dead_strip");
    timings.atoms_ms = strip_timings.atoms_ms;
    timings.liveness_ms = strip_timings.liveness_ms;
    timings.strip_build_ms = strip_timings.build_ms;
    timings.group_ms = strip_timings.group_ms;
    timings.traverse_ms = strip_timings.traverse_ms;
    timings.digest_ms = strip_timings.digest_ms;
    timings.reused_strip = strip_timings.reused_strip;
    timings.reach_moved = strip_timings.reach_moved;
    timings.reach_total = strip_timings.reach_total;
    timings.stripped_bytes = report.dead_bytes();
    timings.revived_atoms = report.revived as u64;

    let prep = std::time::Instant::now();
    let mut placements = placements_for(&objects, &strip);
    timings.prepare_ms = elapsed_ms(prep);
    gap!(_gap, "after prepare");
    timings.placements_ms = timings.prepare_ms;

    if placements.is_empty() {
        return Err(LinkError::NothingToLink);
    }

    // Imports are resolved before the symbol table is checked, not after: an
    // undefined reference is only an error once the dylibs have had their
    // chance at it. Checking first reported `_printf` as undefined in a
    // program that links against libSystem.
    let step = std::time::Instant::now();
    // Held when no input's symbol interface moved and the SDK's exports are
    // the ones this answer was computed against. Both questions resolution
    // asks — which undefined names a dylib provides, and whether any is left
    // over — read names and strengths and nothing else.
    let imports = match session.imports() {
        Some(held) => held.to_vec(),
        None => {
            let imports =
                resolve_imports(&objects, &interned, session.names(), exported.as_deref())?;
            // Resolution runs for its diagnostics: it is what turns a
            // genuinely missing definition into a named error rather than a
            // relocation against zero.
            // Two strong definitions of one name is an error, and this is
            // where the link asks. It used to build the whole resolution
            // table to find out — 93 ms of maps that were then dropped
            // without anything reading the errors in them (162).
            let duplicated = duplicate_definitions(&objects, &interned, session.names());
            if !duplicated.is_empty() {
                return Err(LinkError::DuplicateSymbols {
                    names: explain_duplicates(
                        &objects,
                        &interned,
                        &imports,
                        session.names_mut(),
                        &duplicated,
                    ),
                });
            }
            session.store_imports(&imports);
            imports
        }
    };
    timings.resolve_ms = elapsed_ms(step);
    gap!(_gap, "after resolve");

    let sub = std::time::Instant::now();
    let survey = survey_relocations(&objects, &imports, &strip);
    timings.survey_ms = elapsed_ms(sub);
    gap!(_gap, "after survey");
    let stubs = survey.stubs;
    // Synthesise `__got` before layout, so it is placed and addressed like any
    // other section rather than appended afterwards. Internal targets and
    // imports share one table: the difference is only how each slot gets its
    // value — a rebase for an address we know, a bind for one dyld supplies.
    let mut got = survey.got;
    for entry in survey.personalities {
        if !got.iter().any(|e| e.name == entry.name) {
            got.push(entry);
        }
    }
    for name in &imports {
        if !got.iter().any(|e| &e.name == name) {
            got.push(TableEntry {
                object: SYNTHETIC_OBJECT,
                name: name.clone(),
            });
        }
    }
    // Personality routines named by CIE augmentation data — the only place they
    // appear in DWARF mode (finding 31), which is why collecting them from
    // `__compact_unwind` found none (finding 49).
    let prep = std::time::Instant::now();
    let personality_step = std::time::Instant::now();
    let mut eh_personality_fields: HashMap<(u32, u32), HashSet<u64>> = HashMap::default();
    for object in &objects {
        for section in &object.parsed.sections {
            if section.name != "__eh_frame" {
                continue;
            }
            let fields = eh_frame_personality_fields(object, section);
            for relocation in &object.parsed.relocations {
                if relocation.section != section.id || !fields.contains(&relocation.offset) {
                    continue;
                }
                if let RelocationTarget::Symbol(id) = relocation.target {
                    if let Some(symbol) = object.parsed.symbol(id) {
                        if !got.iter().any(|e| e.name == symbol.name) {
                            got.push(TableEntry {
                                object: object.parsed.id,
                                name: symbol.name.clone(),
                            });
                        }
                    }
                }
            }
            if !fields.is_empty() {
                eh_personality_fields.insert((object.parsed.id.0, section.id.0), fields);
            }
        }
    }

    if !stubs.is_empty() {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: STUBS_SECTION,
            segment: "__TEXT".into(),
            name: "__stubs".into(),
            kind: SectionKind::Code,
            size: stubs.len() as u64 * STUB_SIZE,
            alignment: 4,
        });
    }
    // `__unwind_info` needs addresses to be built, but its *size* must be known
    // before layout runs. Sized from the record count, which is known now: one
    // entry per record, and the encoder's own size formula.
    timings.personality_ms = elapsed_ms(personality_step);
    let unwind_step = std::time::Instant::now();
    let unwind_size = unwind_table_size(&objects, &strip);
    timings.unwind_size_ms = elapsed_ms(unwind_step);
    if unwind_size > 0 {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: UNWIND_SECTION,
            segment: "__TEXT".into(),
            name: "__unwind_info".into(),
            kind: SectionKind::Unwind,
            size: unwind_size,
            alignment: 4,
        });
    }

    let tlv = survey.tlv;
    if !tlv.is_empty() {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: TLV_SECTION,
            segment: "__DATA".into(),
            name: "__thread_ptrs".into(),
            kind: SectionKind::ThreadLocal,
            size: tlv.len() as u64 * GOT_ENTRY_SIZE,
            alignment: 8,
        });
    }
    // Tentative definitions need real storage, and its size must be known
    // before layout like any other section's.
    let commons_step = std::time::Instant::now();
    let commons = common_symbols(&objects);
    let (common_size, common_alignment) = common_section_size(&commons);
    timings.commons_ms = elapsed_ms(commons_step);
    if common_size > 0 {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: COMMON_SECTION,
            segment: "__DATA".into(),
            name: "__common".into(),
            kind: SectionKind::Bss,
            size: common_size,
            alignment: common_alignment as u64,
        });
    }
    if !got.is_empty() {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: GOT_SECTION,
            segment: "__DATA_CONST".into(),
            name: "__got".into(),
            kind: SectionKind::Data,
            size: got.len() as u64 * GOT_ENTRY_SIZE,
            alignment: 8,
        });
    }

    // Pass one: learn where everything lands.
    let step = std::time::Instant::now();
    // Names for every contribution that survive into the next link, and the
    // previous link's placements to lay this one out on. Read before the probe
    // because the probe *is* a layout: sizing the load commands against one
    // shape and emitting another is how a reservation comes to be wrong.
    timings.prepare_ms += elapsed_ms(prep);
    gap!(_gap, "got+prepare");

    let cache_step = std::time::Instant::now();
    // From this process first. A resident linker wrote this structure a moment
    // ago and then encoded it, wrote it, and is about to read and decode it
    // back — several megabytes through the filesystem to recover what it never
    // stopped having. The file is how a *restart* stays warm, not how two links
    // in one process talk to each other.
    let held_cache = request
        .cache_path
        .as_deref()
        .and_then(|path| session.take_cache(path));
    timings.cache_held = held_cache.is_some();
    let mut previous_cache = match held_cache {
        Some(cache) => Some(cache),
        None => request.cache_path.as_deref().and_then(blinker_cache::load),
    };
    timings.cache_load_ms = elapsed_ms(cache_step);
    // Only when the file was actually read: a `stat` per link to report a
    // number about a file nothing opened is the same species of cost as the
    // placement counter.
    if !timings.cache_held {
        timings.cache_bytes_read = request
            .cache_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map_or(0, |meta| meta.len());
    }

    let contribution_keys = match request.cache_path {
        Some(_) => identity::ContributionKeys::build(&objects),
        None => identity::ContributionKeys::default(),
    };
    // Only a table this same request produced. A layout is a set of decisions
    // about *these* inputs under *these* options, and one taken under others
    // is not wrong so much as not about this link.
    let previous_cache_inputs: Vec<(PathBuf, blinker_cache::InputKey)> = previous_cache
        .as_ref()
        .map(|cache| cache.inputs.clone())
        .unwrap_or_default();
    let reservations = request.cache_path.as_ref().map(|_| full_sizes(&objects));
    // Only the final pass signs, so only it needs these.
    // Taken, not copied. This is the previous link's finished binary — 194 MB
    // on a debug rust-analyzer link — and the cache it comes out of is rebuilt
    // from this link's image before it is stored again, so nothing later reads
    // the old bytes.
    let previous_signature = previous_cache.as_mut().and_then(|cache| {
        (!cache.image.is_empty() && !cache.page_hashes.is_empty()).then(|| {
            (
                std::mem::take(&mut cache.image),
                std::mem::take(&mut cache.page_hashes),
            )
        })
    });
    let previous_layout = previous_cache
        .as_ref()
        .filter(|cache| cache.request == request_hash(request))
        .filter(|cache| !cache.layout.slots.is_empty())
        .map(|cache| (cache.layout.clone(), contribution_keys.as_map()));

    let probe = assemble(
        request,
        &Assembly {
            placements: &placements,
            previous: previous_layout.as_ref(),
            reservations: reservations.as_ref(),
            dylibs: &dylibs,
            ..Assembly::default()
        },
    )?;

    timings.layout_probe_ms = elapsed_ms(step);

    gap!(_gap, "probe layout + cache load");
    // With addresses known, copy content and patch it.
    let step = std::time::Instant::now();
    // Built once from the layout, and consulted a few hundred thousand times.
    let sub = std::time::Instant::now();
    let placed = Placed::index(&probe);
    let mut addresses = address_map(&objects, &interned, &placed, &strip);
    // Commons have no section of their own in any input, so `address_map` —
    // which walks each symbol's defining section — cannot see them. They are
    // definitions all the same, and every reference resolves here.
    for (name, value) in common_addresses(&commons, &probe) {
        // Interned rather than looked up: a common has no defining section, so
        // `address_map` never saw it, and every consumer asks by id.
        let id = session.names_mut().intern(name);
        addresses.global.insert(id, value);
    }
    // Taken after the last name is interned, so it covers every id above.
    let name_digests = session.digests();
    let got_slots = got_slot_addresses(&got, &probe);
    let stub_slots = stub_addresses(&stubs, &probe);
    let tlv_slots = pointer_slot_addresses(&tlv, &probe, "__thread_ptrs");
    timings.address_map_ms = elapsed_ms(sub);

    let sub = std::time::Instant::now();
    let mut contents = build_contents(&objects, &probe, &strip)?;
    timings.contents_ms = elapsed_ms(sub);

    let sub = std::time::Instant::now();
    // Split because the five have nothing in common but the stage they share.
    // The tables are proportional to the number of *slots*, which barely moves;
    // `__unwind_info` is rebuilt from every function in the program, which is
    // the kind of global recomputation this linker is supposed to be getting
    // rid of. Which of those the 1.44 ms is decides whether there is anything
    // here worth doing.
    let part = std::time::Instant::now();
    repair_eh_frame(&mut contents, &probe, &objects, &strip);
    timings.eh_frame_ms = elapsed_ms(part);

    let part = std::time::Instant::now();
    fill_got(
        &mut contents,
        &probe,
        &got,
        &addresses,
        session.names(),
        &imports,
    )?;
    fill_stubs(&mut contents, &probe, &stubs, &got_slots)?;
    fill_pointer_table(
        &mut contents,
        &probe,
        &tlv,
        &addresses,
        session.names(),
        "__thread_ptrs",
    )?;
    timings.tables_ms = elapsed_ms(part);

    let part = std::time::Instant::now();
    fill_unwind_info(
        &mut contents,
        &probe,
        &objects,
        &interned,
        &placed,
        &addresses,
        &strip,
        &got_slots,
    )?;
    timings.unwind_ms = elapsed_ms(part);
    timings.synthetic_ms = elapsed_ms(sub);

    // Whether this link records what each object read, so a later one can skip
    // relocating it.
    //
    // Automatic in a resident process, and off otherwise, because the same
    // mechanism loses badly in one setting and wins clearly in the other. What
    // it costs is recording; what it saves is relocating. Finding 94 measured
    // it as a loss at any hit rate — and measured it going through a cache
    // *file*, where every link decoded a few megabytes to recover the entries
    // and encoded them again afterwards.
    //
    // With the session holding the cache (finding 110) that cost is gone, and
    // the same machinery on the same workload:
    //
    // ```
    //   apply     3.67 ms -> 0.83 ms    96% of 87,834 relocations reused
    //   link     19.6 ms  -> 16.8 ms
    // ```
    //
    // A one-shot link still gets the old answer, and correctly: it has no
    // previous entries to reuse, so recording would be pure cost.
    let reuse_relocations =
        request.cache_path.is_some() && (request.reuse_relocations || session.is_resident());

    // The addresses this link produced, in the form the cache compares. Built
    // before relocation because it is what decides which objects can skip it —
    // and not built at all when nothing will ask.
    let table_step = std::time::Instant::now();
    let current_addresses = request.cache_path.as_ref().map(|_| {
        address_table(
            &addresses,
            &name_digests,
            &got_slots,
            &stub_slots,
            &tlv_slots,
        )
    });
    timings.address_table_ms = elapsed_ms(table_step);

    // How much of the program's addressing actually moved. Not used to decide
    // anything yet — measured first, because "skip the objects that read no
    // changed address" is only worth building if finding out which those are is
    // cheaper than relocating them.
    let diff_step = std::time::Instant::now();
    if let (Some(previous), Some(current)) = (previous_cache.as_ref(), current_addresses.as_ref()) {
        if !previous.addresses.is_empty() {
            let changed = blinker_cache::changed_addresses(&previous.addresses, current);
            timings.changed_addresses = changed.len() as u64;
            timings.total_addresses = current.len() as u64;
        }
    }
    timings.address_diff_ms = elapsed_ms(diff_step);

    let previous = previous_cache.filter(|_| reuse_relocations);

    let cache_step = std::time::Instant::now();
    // One pass over the layout, shared by the reuse plan and the cache builder.
    let ranges_of = object_ranges_index(&probe);
    let plan = match (&previous, &current_addresses) {
        (Some(previous), Some(current)) => {
            Some(plan_reuse(&objects, previous, current, session, &ranges_of))
        }
        _ => None,
    };
    timings.cache_plan_ms = elapsed_ms(cache_step);
    timings.total_objects = objects.len() as u64;

    let sub = std::time::Instant::now();
    let patched = apply_relocations(
        &objects,
        &Names {
            interned: &interned,
            digests: &name_digests,
        },
        &probe,
        &Placement {
            addresses: &addresses,
            strip: &strip,
            placed: &placed,
        },
        &IndirectTables {
            got: &got_slots,
            stubs: &stub_slots,
            tlv: &tlv_slots,
            imports: &imports,
            exports: exported.as_deref(),
            personalities: &eh_personality_fields,
        },
        contents,
        reuse_relocations,
        plan.as_ref(),
    )?;
    // Built here, while the patched contents still exist, but not written
    // until the image does — the fast path needs the finished binary, and that
    // is the last thing produced.
    timings.apply_ms = elapsed_ms(sub);

    let cache_step = std::time::Instant::now();
    let mut cache = match (&request.cache_path, current_addresses) {
        (Some(_), Some(addresses)) => Some(build_cache(
            request,
            &objects,
            &probe,
            addresses,
            reuse_relocations.then_some(&patched.contents),
            &patched,
            (session, &ranges_of, &contribution_keys),
        )),
        _ => None,
    };
    timings.cache_build_ms = elapsed_ms(cache_step);

    timings.reused_objects = patched.reused;
    timings.reused_relocations = patched.reused_relocations;
    timings.total_relocations = objects
        .iter()
        .map(|o| o.parsed.relocations.len() as u64)
        .sum();
    let contents = patched.contents;
    let entry_offset = entry_offset(request, &objects, &probe, &strip)?;
    timings.relocate_ms = elapsed_ms(step);
    gap!(_gap, "after relocate");

    // Pass two: the same layout, with real bytes.
    //
    // The symbol table grows between passes, which changes `LC_SYMTAB`'s
    // contents but not the load commands' *sizes* — so the section addresses
    // the relocations were computed against still hold.
    let sub = std::time::Instant::now();
    let placed_symbols = placed_symbols(&objects, &interned, &placed, &strip);
    gap!(_gap, "sym: placed");
    let mut symbols = output_symbols(&placed_symbols);
    gap!(_gap, "sym: output");
    // After the ordinary locals, which is where `ld` puts them and where a
    // consumer walking the local range expects the debug map to begin.
    symbols.extend(debug_map(&objects, &placed_symbols));
    gap!(_gap, "sym: debug map");

    // Each GOT slot holds an absolute address, and the image is position
    // independent, so dyld must relocate every one of them at load time.
    // A slot whose value we know is rebased; a slot dyld fills is bound.
    let mut rebases = got_rebases(&probe, &got, &imports);
    let mut binds = got_binds(&probe, &got, &imports, exported.as_deref());
    binds.extend(patched.binds);
    rebases.extend(pointer_table_rebases(&probe, "__thread_ptrs", tlv.len()));
    rebases.extend(patched.rebases);

    timings.symbols_ms = elapsed_ms(sub);
    gap!(_gap, "after symbols");

    let step = std::time::Instant::now();
    let image = assemble(
        request,
        &Assembly {
            placements: &placements,
            symbols: &symbols,
            contents: Some(&contents),
            rebases: &rebases,
            binds: &binds,
            entry_offset,
            final_pass: true,
            previous: previous_layout.as_ref(),
            reservations: reservations.as_ref(),
            previous_signature: previous_signature.as_ref(),
            dylibs: &dylibs,
        },
    );
    timings.emit_ms = elapsed_ms(step);
    gap!(_gap, "after emit");
    if let Ok(image) = &image {
        timings.emit_breakdown = image.timings;
    }

    // What the allocator actually achieved, counted against the table it was
    // given rather than inferred from how fast the link was.
    //
    // Timed separately because it is *diagnostics*, and it turned out not to be
    // free: it walks every contribution and probes every input, and probing an
    // input that a path cannot identify means hashing its contents. A counter
    // that costs a millisecond to compute is a measurement changing the thing
    // it measures.
    let accounting = std::time::Instant::now();
    if let (true, Some((previous, _)), Ok(image)) =
        (request.count_placement, &previous_layout, &image)
    {
        // Which inputs are the ones the previous link saw. An archive member
        // shares its archive's key, so an rlib that changed marks all of its
        // members changed — coarse, and the reason member-level identity is
        // the next thing this needs.
        // Keys from the session, which proved every one of these inputs at the
        // top of the link. Probing again means a `stat` per path and a read and
        // a BLAKE3 for each of rustc's objects — 22 MB of them on a debug
        // rust-analyzer link, hashed here for a third time, to compute a
        // counter. The comment above says exactly this and the code did it
        // anyway (finding 184).
        let unchanged: HashSet<&Path> = previous_cache_inputs
            .iter()
            .filter(|(path, key)| {
                session
                    .key_for(path)
                    .or_else(|| blinker_cache::InputKey::probe(path))
                    .as_ref()
                    == Some(key)
            })
            .map(|(path, _)| path.as_path())
            .collect();
        let source_of: HashMap<u32, &Path> = objects
            .iter()
            .map(|o| (o.parsed.id.0, o.path.as_ref()))
            .collect();

        for section in &image.layout.sections {
            let qualified = section.qualified_name();
            for contribution in &section.contributions {
                let key = contribution_keys.key_or_fresh(contribution.object, contribution.section);
                let Some(slot) = previous.slots.get(&key) else {
                    continue;
                };
                if slot.section == qualified && slot.offset == contribution.offset {
                    timings.contributions_retained += 1;
                } else {
                    timings.contributions_moved += 1;
                    if source_of
                        .get(&contribution.object.0)
                        .is_some_and(|path| unchanged.contains(path))
                    {
                        timings.contributions_moved_unchanged += 1;
                    }
                }
            }
        }
    }

    timings.accounting_ms = elapsed_ms(accounting);
    gap!(_gap, "acct: total");
    gap!(_gap, "after accounting");

    if let (Some(path), Some(cache), Ok(image)) = (&request.cache_path, &mut cache, &image) {
        let cache_step = std::time::Instant::now();
        cache.image = image.bytes.clone();
        gap!(_gap, "image bytes clone");
        cache.page_hashes.clone_from(&image.page_hashes);
        gap!(_gap, "page hashes clone");
        // Written on this session's first link and held in memory thereafter;
        // see `Session::store_cache`. A cache that cannot be written is not an
        // error: the link succeeded, and the only consequence is that a future
        // process starts cold.
        let write = session.store_cache(path, std::mem::take(cache));
        if write {
            if let Some((_, held)) = session.cache_for(path) {
                let _ = blinker_cache::store(path, held);
            }
            timings.cache_bytes_written = std::fs::metadata(path).map_or(0, |meta| meta.len());
        }
        timings.cache_store_ms = elapsed_ms(cache_step);
    }

    gap!(_gap, "cache store tail");
    timings.total_ms = elapsed_ms(overall);
    image
}

/// Copy one object's cached bytes into the sections being assembled.
///
/// Returns whether every range landed. A range that does not fit is a cache
/// describing a layout this link did not produce, and the caller relocates the
/// object instead — bounds are checked rather than trusted because the file
/// came off disk and may have been written by anything.
fn copy_cached_bytes(
    entry: &blinker_cache::Entry,
    plan: &ReusePlan<'_>,
    bytes: &mut ObjectBytes<'_>,
) -> bool {
    for range in &entry.ranges {
        let source = plan.sections.get(&range.section);
        // A zero-filled section — `__bss` and the thread-local block — has a
        // contribution with a real length and no bytes anywhere, in this link
        // or the cached one. There is nothing to copy and nothing wrong. This
        // is why a first version reused nothing at all on a Rust link: every
        // object that touched `__bss` failed the copy and fell back, which
        // looked exactly like a cache that never matched.
        let (start, len) = (range.start as usize, range.len as usize);
        let destination = bytes.covering(range.section as usize, start, len);
        // A zero-filled section — `__bss` and the thread-local block — has a
        // contribution with a real length and no bytes anywhere, in this link
        // or the cached one. There is nothing to copy and nothing wrong.
        if destination.is_none() {
            if source.is_none() {
                continue;
            }
            return false;
        }
        let (Some(source), Some(to)) = (source, destination) else {
            return false;
        };
        let Some(from) = source.get(start..start + len) else {
            return false;
        };
        to.copy_from_slice(from);
    }
    true
}

/// What a previous link left that this one can reuse.
///
/// Built once, before relocation, so the relocation loop's decision is a map
/// lookup rather than a re-derivation per object.
struct ReusePlan<'a> {
    /// Entries whose three conditions all hold, by object.
    entries: HashMap<u32, &'a blinker_cache::Entry>,
    /// The previous link's patched section bytes.
    sections: HashMap<u32, &'a [u8]>,
}

impl ReusePlan<'_> {
    fn entry(&self, object: ObjectId) -> Option<&blinker_cache::Entry> {
        self.entries.get(&object.0).copied()
    }
}

/// Why [`plan_reuse`] turned an object down, under `BLINKER_REUSE_PARTS`.
#[derive(Default)]
struct Rejections {
    /// Contributed nothing to the layout, so there is nothing to reuse.
    unplaced: u64,
    /// Nothing in the previous link began where this object's bytes begin.
    no_entry: u64,
    /// The file could not be examined at all.
    unprobed: u64,
    /// The input it came from is not the one the entry was built from.
    key: u64,
    /// It landed somewhere else, or at a different size.
    ranges: u64,
    /// It reads an address that moved.
    deps: u64,
}

impl Rejections {
    fn add(&mut self, other: &Rejections) {
        self.unplaced += other.unplaced;
        self.no_entry += other.no_entry;
        self.unprobed += other.unprobed;
        self.key += other.key;
        self.ranges += other.ranges;
        self.deps += other.deps;
    }

    fn report(&self, kept: usize, total: usize, changed: usize) {
        if std::env::var_os("BLINKER_REUSE_PARTS").is_none() {
            return;
        }
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "plan_reuse: {kept} of {total} kept; turned down unplaced {} no-entry {} \
                 unprobed {} key {} ranges {} deps {}  ({changed} addresses changed)",
                self.unplaced, self.no_entry, self.unprobed, self.key, self.ranges, self.deps,
            );
        }
    }
}

/// Decide, for every object, whether its bytes survive from the previous link.
///
/// Entries are matched to objects by **where their bytes went**, not by
/// position in the cache: adding or removing one input shifts every later
/// object's id, and an entry matched by index would then be checked against a
/// different object's content hash and pass. The first range is unique to an
/// object — two contributions cannot begin at the same offset of the same
/// section — so it identifies the entry without needing a name.
fn plan_reuse<'a>(
    objects: &[LoadedObject],
    previous: &'a blinker_cache::LinkCache,
    current_addresses: &[(blinker_cache::NameHash, u64)],
    session: &Session,
    ranges_of: &HashMap<u32, Vec<blinker_cache::Range>>,
) -> ReusePlan<'a> {
    let changed: std::collections::HashSet<blinker_cache::NameHash> =
        blinker_cache::changed_addresses(&previous.addresses, current_addresses)
            .into_iter()
            .collect();

    let by_placement: HashMap<(u32, u64), &blinker_cache::Entry> = previous
        .entries
        .iter()
        .filter_map(|entry| entry.ranges.first().map(|r| ((r.section, r.start), entry)))
        .collect();

    // One probe per distinct file, ahead of the decision below: an archive is
    // proven unchanged once, not once per member pulled out of it, and the
    // decision itself has to touch nothing shared.
    let mut keys: HashMap<&Path, Option<blinker_cache::InputKey>> = HashMap::default();
    for object in objects.iter().filter(|object| !object.unchanged) {
        let path = object.path.as_ref();
        keys.entry(path).or_insert_with(|| {
            // From the session when it has one: it proved this input a moment
            // ago, and re-proving a rustc object means reading and hashing it
            // again.
            session
                .key_for(path)
                .or_else(|| blinker_cache::InputKey::probe(path))
        });
    }

    // On every core, because the dependency scan reads every address every
    // object read — 3.9 million of them, 30 MB of hashes — and finding 179
    // made that the whole of this function rather than the tail of it. Nothing
    // here writes anything shared; the answers are collected in order and the
    // map is filled from them.
    //
    // The counters are per chunk and summed afterwards for the same reason.
    let keys = &keys;
    let changed = &changed;
    let by_placement = &by_placement;
    let judged = crate::parallel::map_chunks(objects, |_, chunk| {
        let mut lost = Rejections::default();
        let verdicts: Vec<Option<(u32, &blinker_cache::Entry)>> = chunk
            .iter()
            .map(|object| {
                let ranges = ranges_of.get(&object.parsed.id.0)?;
                let first = ranges.first()?;
                let Some(entry) = by_placement.get(&(first.section, first.start)) else {
                    lost.no_entry += 1;
                    return None;
                };
                // A member the session proved byte-identical needs no key: the
                // comparison that proved it is stronger than the one a key
                // stands in for, and asking the key instead rejects every
                // member of a recompiled crate's rlib for the enclosing
                // archive's sake.
                if !object.unchanged {
                    let Some(Some(key)) = keys.get(object.path.as_ref()) else {
                        lost.unprobed += 1;
                        return None;
                    };
                    if &entry.key != key {
                        lost.key += 1;
                        return None;
                    }
                }
                if entry.is_content_reusable(ranges, changed) {
                    return Some((object.parsed.id.0, *entry));
                }
                if &entry.ranges != ranges {
                    lost.ranges += 1;
                } else {
                    lost.deps += 1;
                }
                None
            })
            .collect();
        // `unplaced` is what the two `?`s above swallow, and counting it inside
        // the closure would need a third branch on each.
        lost.unplaced = verdicts
            .iter()
            .zip(chunk)
            .filter(|(verdict, object)| {
                verdict.is_none()
                    && ranges_of
                        .get(&object.parsed.id.0)
                        .is_none_or(|r| r.is_empty())
            })
            .count() as u64;
        (verdicts, lost)
    });

    let mut entries = HashMap::default();
    let mut lost = Rejections::default();
    for (verdicts, chunk) in judged {
        entries.extend(verdicts.into_iter().flatten());
        lost.add(&chunk);
    }
    lost.report(entries.len(), objects.len(), changed.len());

    ReusePlan {
        entries,
        sections: previous
            .sections
            .iter()
            .map(|(index, bytes)| (*index, bytes.as_slice()))
            .collect(),
    }
}

/// Assemble the record a later link can reuse.
///
/// Built after relocation, from what that pass already produced: the patched
/// bytes, each object's fixups, and the addresses every object read. Nothing
/// here is computed for the cache's sake alone except the input keys and the
/// hashing of names, which is why writing a cache costs a fraction of using
/// one.
fn build_cache(
    request: &LinkRequest,
    objects: &[LoadedObject],
    image: &Image,
    addresses: Vec<(blinker_cache::NameHash, u64)>,
    // The patched section bytes, when a later link may reuse them per object.
    // `None` stores the finished image alone — every byte here is a second copy
    // of what the image already holds, and without per-object entries to index
    // them nothing can read them back.
    contents: Option<&SectionContents>,
    patched: &Patched,
    // What this link already worked out and should not work out again: the
    // session that proved every input, the layout partitioned by object, and
    // the contribution identities.
    known: (
        &Session,
        &HashMap<u32, Vec<blinker_cache::Range>>,
        &identity::ContributionKeys,
    ),
) -> blinker_cache::LinkCache {
    let (session, ranges_of, identities) = known;
    // Input keys, one probe per distinct file. Archive members share their
    // archive's path and therefore its key: an rlib is proven unchanged once,
    // not once per member pulled out of it.
    let mut keys: HashMap<&Path, Option<blinker_cache::InputKey>> = HashMap::default();
    let index_of = ObjectIndex::build(objects);

    let entries = patched
        .records
        .iter()
        .filter_map(|record| {
            let object = index_of.get(record.object)?;
            let path = object.path.as_ref();
            // From the session, for the same reason `plan_reuse` asks it: the
            // input was proven a moment ago and re-proving one of rustc's
            // objects means reading and hashing it again.
            let key = keys
                .entry(path)
                .or_insert_with(|| {
                    session
                        .key_for(path)
                        .or_else(|| blinker_cache::InputKey::probe(path))
                })
                .clone()?;
            Some(blinker_cache::Entry {
                key,
                ranges: ranges_of.get(&record.object.0).cloned().unwrap_or_default(),
                deps: record.deps.clone(),
                binds: patched.binds[record.binds.clone()]
                    .iter()
                    .map(|bind| blinker_cache::CachedBind {
                        segment: bind.segment,
                        offset: bind.offset,
                        symbol: bind.symbol.clone(),
                        library_ordinal: bind.library_ordinal,
                        addend: bind.addend,
                    })
                    .collect(),
                rebases: patched.rebases[record.rebases.clone()]
                    .iter()
                    .map(|rebase| blinker_cache::CachedRebase {
                        segment: rebase.segment,
                        offset: rebase.offset,
                    })
                    .collect(),
            })
        })
        .collect();

    let mut sections: Vec<_> = contents
        .into_iter()
        .flatten()
        .map(|(index, bytes)| (*index as u32, bytes.clone()))
        .collect();
    sections.sort_unstable_by_key(|(index, _)| *index);

    blinker_cache::LinkCache {
        entries,
        addresses,
        sections,
        inputs: input_keys(request).unwrap_or_default(),
        request: request_hash(request),
        // Filled in with the image, for the same reason.
        page_hashes: Vec::new(),
        // Read back off the layout this link produced, keyed by an identity
        // that survives the next one. This is what the retained-placement
        // allocator consumes; recording it costs a walk over the contributions
        // that already exist.
        layout: blinker_layout::PreviousLayout::record(&image.layout, |object, section| {
            identities.key_or_fresh(object, section)
        }),
        // Filled in once the image exists; a cache written without it simply
        // has no fast path, which is a slower link and not a wrong one.
        image: Vec::new(),
    }
}

/// Where one object's bytes sit in the output, in cache terms.
/// Every object's output ranges, from one pass over the layout.
///
/// `object_ranges` answers the question for one object by scanning every
/// contribution of every section. Asking it once per object — which both the
/// reuse plan and the cache builder did — makes that quadratic: 237 objects
/// against 1,063 contributions is a quarter of a million iterations, twice a
/// link, to produce a partition of the very list being scanned.
fn object_ranges_index(image: &Image) -> HashMap<u32, Vec<blinker_cache::Range>> {
    let mut index: HashMap<u32, Vec<blinker_cache::Range>> = HashMap::default();
    for (section_index, section) in image.layout.sections.iter().enumerate() {
        for contribution in &section.contributions {
            index
                .entry(contribution.object.0)
                .or_default()
                .push(blinker_cache::Range {
                    section: section_index as u32,
                    start: contribution.offset,
                    len: contribution.size,
                });
        }
    }
    // Sorted so that comparing two links compares placement, not the order the
    // layout happened to visit sections in — the same guarantee `object_ranges`
    // gave.
    for ranges in index.values_mut() {
        ranges.sort_unstable_by_key(|r| (r.section, r.start));
    }
    index
}

/// Every address a relocation could have read, in one sorted table.
///
/// The indirect tables are included alongside the symbols because they move
/// independently of them: inserting a GOT entry shifts every slot after it
/// while leaving every symbol address untouched, and an entry whose bytes
/// reference the shifted slot must not look unchanged.
///
/// The names are not hashed here. Each one's BLAKE3 digest was taken when the
/// object introducing it was first parsed, and all that is left per entry is
/// folding in the scope and the table — which is what turned 140 ms of BLAKE3
/// over half a million unchanged names into a subscript (finding 158).
fn address_table(
    addresses: &AddressMap,
    digests: &[blinker_cache::NameHash],
    got_slots: &HashMap<String, u64>,
    stub_slots: &HashMap<String, u64>,
    tlv_slots: &HashMap<String, u64>,
) -> Vec<(blinker_cache::NameHash, u64)> {
    use blinker_cache::{combine, Table, GLOBAL};
    let digest = |name: SymbolNameId| digests[name.0 as usize];
    let _t = std::time::Instant::now();
    let mut table: Vec<_> = addresses
        .global
        .iter()
        .map(|(name, address)| (combine(digest(*name), GLOBAL, Table::Symbol), *address))
        .chain(addresses.local.iter().flat_map(|(object, names)| {
            names.iter().map(move |(name, address)| {
                (combine(digest(*name), *object, Table::Symbol), *address)
            })
        }))
        .chain(indirect_entries(got_slots, Table::Got))
        .chain(indirect_entries(stub_slots, Table::Stub))
        .chain(indirect_entries(tlv_slots, Table::ThreadLocal))
        .collect();
    let collect_ms = _t.elapsed().as_secs_f64() * 1000.0;
    let _s = std::time::Instant::now();
    table.sort_unstable();
    table.dedup();
    if std::env::var_os("BLINKER_ADDR_PARTS").is_some() {
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "address_table: hash+collect {collect_ms:.0} sort+dedup {:.0}  ({} entries)",
                _s.elapsed().as_secs_f64() * 1000.0,
                table.len()
            );
        }
    }
    table
}

fn indirect_entries(
    slots: &HashMap<String, u64>,
    table: blinker_cache::Table,
) -> impl Iterator<Item = (blinker_cache::NameHash, u64)> + '_ {
    slots.iter().map(move |(name, address)| {
        (
            blinker_cache::dep_hash(blinker_cache::GLOBAL, table, name),
            *address,
        )
    })
}

fn elapsed_ms(start: std::time::Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Undefined references, checked against what `libSystem` actually exports.
fn resolve_imports(
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    names: &SymbolNames,
    exported: Option<&libraries::StubExports>,
) -> Result<Vec<String>, LinkError> {
    let undefined = undefined_references(objects, interned, names);
    if undefined.is_empty() {
        return Ok(Vec::new());
    }

    let Some(available) = exported else {
        // No stub library was supplied, so nothing can be imported and every
        // undefined reference is an error.
        return Err(LinkError::UndefinedSymbols { names: undefined });
    };

    let mut imports = Vec::new();
    let mut missing = Vec::new();
    for name in undefined {
        if available.contains(&name) {
            imports.push(name);
        } else {
            missing.push(name);
        }
    }
    if !missing.is_empty() {
        return Err(LinkError::UndefinedSymbols { names: missing });
    }
    Ok(imports)
}

/// Size the `__unwind_info` table will come to.
///
/// Computed from the record count rather than by building the table, because
/// layout needs the size before any address exists. Building it twice would
/// need addresses that do not exist yet.
fn unwind_table_size(objects: &[LoadedObject], strip: &Strip) -> u64 {
    let records: usize = objects
        .iter()
        .map(|object| {
            object
                .parsed
                .sections
                .iter()
                .filter(|s| s.name == "__compact_unwind")
                .map(|s| live_unwind_records(object, s, strip))
                .sum::<usize>()
        })
        .sum();
    if records == 0 {
        return 0;
    }
    // Deliberately generous: the real table is smaller once duplicate function
    // offsets collapse and only some entries carry an LSDA. Over-reserving
    // wastes a few kilobytes; under-reserving is a link failure.
    blinker_output::unwind::upper_bound_size(records) as u64
}

/// How many of a section's compact unwind records describe a function that
/// survived.
///
/// Sizing from the input's record count instead left `__unwind_info` reserved
/// at 35 KB where 5 KB was used — the largest single thing separating blinker's
/// stripped output from the system linker's, and invisible because
/// over-reserving is safe.
fn live_unwind_records(object: &LoadedObject, section: &InputSection, strip: &Strip) -> usize {
    let total = (section.size / COMPACT_UNWIND_RECORD) as usize;
    let mut live = 0;
    for relocation in object.parsed.relocations_for(section.id) {
        if relocation.offset % COMPACT_UNWIND_RECORD != CU_FUNCTION {
            continue;
        }
        let survives = match relocation.target {
            RelocationTarget::Section(id) => {
                let origin = object.parsed.section(id).map(|s| s.vm_address).unwrap_or(0);
                let inline = inline_addend(object, relocation) as u64;
                strip
                    .remap(object.parsed.id, id, inline.saturating_sub(origin))
                    .is_some()
            }
            RelocationTarget::Symbol(_) => true,
        };
        live += usize::from(survives);
    }
    // A record with no relocation on its function pointer names nothing and is
    // dropped anyway, so counting relocations rather than records is the same
    // number — except when there are none at all, where the section is not
    // something this understands and the whole of it is reserved for.
    if live == 0 && object.parsed.relocations_for(section.id).is_empty() {
        return total;
    }
    live.min(total)
}

/// Build the unwind table and write it into its section.
// Eight for the same reason as `compact_unwind_entries`, which it calls.
#[allow(clippy::too_many_arguments)]
fn fill_unwind_info(
    contents: &mut SectionContents,
    image: &Image,
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    placed: &Placed,
    addresses: &AddressMap,
    strip: &Strip,
    got_slots: &HashMap<String, u64>,
) -> Result<(), LinkError> {
    let Some((index, section)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == "__unwind_info")
    else {
        return Ok(());
    };

    // Of this function's ~1.25 ms, `eh_frame_fde_offsets` is 1.00, collecting
    // the compact-unwind entries is 0.23, and encoding the table is 0.02. The
    // cost is finding where each function's FDE landed, not building the table
    // — which is the opposite of what the name suggests, and is where any work
    // on this should go.
    let fde_offsets = eh_frame_fde_offsets(objects, interned, image, placed, addresses, strip);
    let entries = compact_unwind_entries(
        objects,
        interned,
        image,
        placed,
        addresses,
        strip,
        got_slots,
        &fde_offsets,
    );
    let mut table = blinker_output::unwind::build(entries);

    if table.len() as u64 > section.size {
        return Err(LinkError::UnwindTableTooLarge {
            reserved: section.size,
            needed: table.len(),
        });
    }
    // The reservation is an upper bound, so the tail is padding.
    table.resize(section.size as usize, 0);
    contents.insert(index, table);
    Ok(())
}

/// Address of each stub.
fn stub_addresses(stubs: &[String], image: &Image) -> HashMap<String, u64> {
    let mut slots = HashMap::default();
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__stubs") else {
        return slots;
    };
    for (index, name) in stubs.iter().enumerate() {
        slots.insert(name.clone(), section.vm_address + index as u64 * STUB_SIZE);
    }
    slots
}

/// Write each stub's three instructions.
fn fill_stubs(
    contents: &mut HashMap<usize, Vec<u8>>,
    image: &Image,
    stubs: &[String],
    got_slots: &HashMap<String, u64>,
) -> Result<(), LinkError> {
    if stubs.is_empty() {
        return Ok(());
    }
    let Some((index, section)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == "__stubs")
    else {
        return Ok(());
    };
    let buffer = contents
        .entry(index)
        .or_insert_with(|| vec![0u8; stubs.len() * STUB_SIZE as usize]);

    for (slot, name) in stubs.iter().enumerate() {
        let stub_address = section.vm_address + slot as u64 * STUB_SIZE;
        let got = *got_slots
            .get(name)
            .ok_or_else(|| LinkError::UndefinedSymbols {
                names: vec![name.clone()],
            })?;
        let start = slot * STUB_SIZE as usize;
        buffer[start..start + STUB_SIZE as usize].copy_from_slice(&stub_code(stub_address, got));
    }
    Ok(())
}

/// Bind entries: one per GOT slot dyld has to fill.
fn got_binds(
    image: &Image,
    got: &[TableEntry],
    imports: &[String],
    exports: Option<&libraries::StubExports>,
) -> Vec<Bind> {
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__got") else {
        return Vec::new();
    };
    let Some((segment_index, segment)) = image
        .layout
        .segments
        .iter()
        .enumerate()
        .find(|(_, seg)| seg.name == section.segment)
    else {
        return Vec::new();
    };
    let base = section.vm_address - segment.vm_address;
    got.iter()
        .enumerate()
        .filter(|(_, entry)| imports.contains(&entry.name))
        .map(|(slot, entry)| Bind {
            segment: segment_index as u8,
            offset: base + slot as u64 * GOT_ENTRY_SIZE,
            symbol: entry.name.clone(),
            // Which library, not just that there is one: under the two-level
            // namespace dyld looks in this ordinal's library and nowhere else.
            library_ordinal: exports.map(|e| e.ordinal(&entry.name)).unwrap_or(1),
            addend: 0,
        })
        .collect()
}

/// Address of each GOT slot, in the order the symbols were collected.
fn got_slot_addresses(got: &[TableEntry], image: &Image) -> HashMap<String, u64> {
    pointer_slot_addresses(got, image, "__got")
}

/// Address of each slot in a synthesised pointer table.
fn pointer_slot_addresses(
    names: &[TableEntry],
    image: &Image,
    section_name: &str,
) -> HashMap<String, u64> {
    let mut slots = HashMap::default();
    let Some(section) = image
        .layout
        .sections
        .iter()
        .find(|s| s.name == section_name)
    else {
        return slots;
    };
    for (index, entry) in names.iter().enumerate() {
        slots.insert(
            entry.name.clone(),
            section.vm_address + index as u64 * GOT_ENTRY_SIZE,
        );
    }
    slots
}

/// Fill a synthesised pointer table with the addresses its slots point at.
///
/// A slot whose target is not defined in this image is left zero: dyld fills
/// it, and writing a wrong value would be worse than writing none.
fn fill_pointer_table(
    contents: &mut HashMap<usize, Vec<u8>>,
    image: &Image,
    names: &[TableEntry],
    addresses: &AddressMap,
    interner: &SymbolNames,
    section_name: &str,
) -> Result<(), LinkError> {
    if names.is_empty() {
        return Ok(());
    }
    let Some((index, _)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == section_name)
    else {
        return Ok(());
    };
    let buffer = contents
        .entry(index)
        .or_insert_with(|| vec![0u8; names.len() * GOT_ENTRY_SIZE as usize]);

    for (slot, entry) in names.iter().enumerate() {
        // Looked up against the object that *referenced* it, so a local
        // definition is visible.
        // By name, because a table entry is named rather than carried from a
        // symbol — a few thousand of them against the half million that go
        // through `target_address`, so the string hash here is not the one
        // that mattered.
        let Some(address) = interner
            .get(&entry.name)
            .and_then(|name| addresses.lookup(entry.object, name))
        else {
            continue;
        };
        let start = slot * GOT_ENTRY_SIZE as usize;
        buffer[start..start + 8].copy_from_slice(&address.to_le_bytes());
    }
    Ok(())
}

/// Write each GOT slot's initial value: the address of the symbol it points at.
fn fill_got(
    contents: &mut HashMap<usize, Vec<u8>>,
    image: &Image,
    got: &[TableEntry],
    addresses: &AddressMap,
    interner: &SymbolNames,
    imports: &[String],
) -> Result<(), LinkError> {
    if got.is_empty() {
        return Ok(());
    }
    let Some((index, _)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == "__got")
    else {
        return Ok(());
    };
    let buffer = contents
        .entry(index)
        .or_insert_with(|| vec![0u8; got.len() * 8]);

    for (slot, entry) in got.iter().enumerate() {
        let name = &entry.name;
        if imports.contains(name) {
            // dyld writes this slot at load time; it starts as zero.
            continue;
        }
        let address = interner
            .get(name)
            .and_then(|id| addresses.lookup(SYNTHETIC_OBJECT, id))
            .ok_or_else(|| LinkError::UndefinedSymbols {
                names: vec![name.clone()],
            })?;
        let start = slot * GOT_ENTRY_SIZE as usize;
        buffer[start..start + 8].copy_from_slice(&address.to_le_bytes());
    }
    Ok(())
}

/// One rebase entry per slot of a synthesised pointer table.
///
/// The GOT is not the only such table: `__thread_ptrs` holds absolute
/// addresses of thread-local descriptors and needs sliding just the same.
/// Missing these produced a `SIGSEGV` in `lang_start_internal`, the first code
/// to walk a thread-local pointer, with a fault address in the *unslid*
/// address space — the signature of a pointer dyld was never told about.
fn pointer_table_rebases(image: &Image, section_name: &str, count: usize) -> Vec<Rebase> {
    let Some(section) = image
        .layout
        .sections
        .iter()
        .find(|s| s.name == section_name)
    else {
        return Vec::new();
    };
    let Some((segment_index, segment)) = image
        .layout
        .segments
        .iter()
        .enumerate()
        .find(|(_, seg)| seg.name == section.segment)
    else {
        return Vec::new();
    };
    let base = section.vm_address - segment.vm_address;
    (0..count as u64)
        .map(|slot| Rebase {
            segment: segment_index as u8,
            offset: base + slot * GOT_ENTRY_SIZE,
        })
        .collect()
}

/// One rebase entry per GOT slot.
fn got_rebases(image: &Image, got: &[TableEntry], imports: &[String]) -> Vec<Rebase> {
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__got") else {
        return Vec::new();
    };
    let Some((segment_index, segment)) = image
        .layout
        .segments
        .iter()
        .enumerate()
        .find(|(_, seg)| seg.name == section.segment)
    else {
        return Vec::new();
    };
    let base = section.vm_address - segment.vm_address;
    got.iter()
        .enumerate()
        // An imported slot is bound, not rebased: rebasing it would add the
        // load bias to a value dyld is about to overwrite.
        .filter(|(_, entry)| !imports.contains(&entry.name))
        .map(|(slot, _)| Rebase {
            segment: segment_index as u8,
            offset: base + slot as u64 * GOT_ENTRY_SIZE,
        })
        .collect()
}

/// Whether a path is an archive rather than a single object.
///
/// By extension, not by content: `lib.rmeta` inside an `.rlib` is a *genuine*
/// Mach-O object holding crate metadata (finding 9), so sniffing magic numbers
/// misclassifies in the other direction too. The toolchain names these files
/// consistently, and the name is the reliable signal.
fn is_archive(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("a") | Some("rlib")
    )
}

/// Load the inputs, extracting from archives only what the link needs.
///
/// # Why archives are not simply expanded
///
/// `libstd.rlib` holds hundreds of objects. Linking all of them would work and
/// produce a binary tens of megabytes larger than it should be, full of code
/// nothing calls. The rule every linker follows instead: a member is pulled in
/// only when it defines a symbol something already in the link needs — and
/// pulling it in can create new undefined symbols, so the process repeats to a
/// fixed point.
///
/// Order matters within a pass and between passes, which is why this is a loop
/// rather than a single sweep.
/// One input, read and either parsed or indexed.
enum Loaded {
    Object(LoadedObject),
    Archive(
        PathBuf,
        std::sync::Arc<blinker_archive::ArchiveIndex>,
        std::sync::Arc<mapping::Backing>,
        /// A digest of the archive's external symbol table, worked out here
        /// because this runs on a worker thread and the alternative was
        /// hashing every symbol name of fifteen re-read rlibs in a row on the
        /// one thread that had just indexed them in parallel.
        u64,
    ),
}

/// Read and parse one input. Pure with respect to the others, which is what
/// lets [`load_objects`] run them concurrently.
fn load_one(path: &Path, id: Option<ObjectId>) -> Result<Loaded, LinkError> {
    let data = mapping::read(path).map_err(|source| LinkError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    match id {
        None => {
            let index = blinker_archive::index_archive(&data, path)
                .map_err(|source| LinkError::Archive(Box::new(source)))?;
            let symbols = crate::session::external_symbol_digest(&index.symbol_map);
            Ok(Loaded::Archive(
                path.to_path_buf(),
                std::sync::Arc::new(index),
                std::sync::Arc::new(data),
                symbols,
            ))
        }
        Some(id) => {
            let parsed = parse_object(&data, path, None, id)
                .map_err(|source| LinkError::Parse(Box::new(source)))?;
            Ok(Loaded::Object(LoadedObject {
                parsed: std::sync::Arc::new(parsed),
                data: SourceBytes::whole(data),
                path: Arc::from(path),
                member: None,
                unchanged: false,
            }))
        }
    }
}

fn load_objects(paths: &[PathBuf], session: &mut Session) -> Result<Vec<LoadedObject>, LinkError> {
    #[allow(unused_mut)]
    let mut _lap = std::time::Instant::now();
    // Object ids are assigned by position, before anything is read, so that
    // running the reads out of order cannot change them. `is_archive` looks
    // only at the path, so the assignment needs no I/O.
    let mut next_id = 0u32;
    let ids: Vec<Option<ObjectId>> = paths
        .iter()
        .map(|path| {
            (!is_archive(path)).then(|| {
                let id = ObjectId(next_id);
                next_id += 1;
                id
            })
        })
        .collect();

    // Reading and parsing an input touches nothing shared, and it is the
    // largest stage of a cold link. Results are collected positionally rather
    // than as they finish: the order of `objects` decides the layout, and a
    // link whose output depends on thread scheduling is not a link.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(paths.len().max(1));
    let mut loaded: Vec<Option<Result<Loaded, LinkError>>> =
        (0..paths.len()).map(|_| None).collect();

    // Whatever this process already holds, taken first and serially: a probe is
    // a `stat` (or a read and a hash for the inputs whose paths do not identify
    // them), and the session is not shareable across the worker threads below.
    // What is left is what has to be read.
    // Every input probed at once, before the serial pass below can ask about
    // any of them. A probe is a `stat` for a content-addressed path and a read
    // and a BLAKE3 for one of rustc's; a debug rust-analyzer link has 133 loose
    // objects and 22 MB of them, and hashing that on one thread is the whole of
    // this phase. Nothing here touches the session, which is what makes it
    // possible — the serial pass exists because the session is not shareable,
    // not because the probing is not.
    let probes = parallel::map_chunks(paths, |_, chunk| {
        chunk
            .iter()
            .map(|path| blinker_cache::InputKey::probe(path))
            .collect::<Vec<_>>()
    });
    let probes: Vec<Option<blinker_cache::InputKey>> = probes.into_iter().flatten().collect();

    let mut todo: Vec<usize> = Vec::with_capacity(paths.len());
    for (at, path) in paths.iter().enumerate() {
        let held = match ids[at] {
            // Only when the held parse carries the id this link would assign.
            // Ids are positional, so a session that survived a changed input
            // list may be holding a parse under a number that now means a
            // different object; everything downstream keys on that number.
            // This is the check that lets `Session::begin` keep anything at
            // all (finding 144), and it is the same argument the archive
            // member cache has always made.
            Some(id) => session
                .object(path, probes[at].as_ref())
                .and_then(|(parsed, data)| {
                    (parsed.id == id).then(|| {
                        Loaded::Object(LoadedObject {
                            parsed,
                            data: SourceBytes::whole_shared(&data),
                            path: Arc::from(path.as_path()),
                            member: None,
                            unchanged: false,
                        })
                    })
                }),
            None => session
                .archive(path, probes[at].as_ref())
                .map(|(index, data)| Loaded::Archive(path.clone(), index, data, 0)),
        };
        match held {
            Some(entry) => loaded[at] = Some(Ok(entry)),
            None => todo.push(at),
        }
    }
    gap!(_lap, "load: session probe");

    let threads = threads.min(todo.len().max(1));
    if threads > 1 {
        // A shared cursor rather than a contiguous slice each. The inputs are
        // wildly uneven — 37 objects averaging 8 KB, then 19 rlibs averaging
        // 900 KB, and the rlibs arrive last because that is the order a linker
        // command line puts them in. Handing each thread a contiguous chunk
        // gave the last thread every large file and saved 1.2 ms of 6.9;
        // letting threads take the next unclaimed input balances itself.
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        let (paths, ids, todo) = (&paths, &ids, &todo);
        let claimed: Vec<Vec<(usize, Result<Loaded, LinkError>)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let cursor = &cursor;
                    scope.spawn(move || {
                        let mut mine = Vec::new();
                        loop {
                            let next = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(&at) = todo.get(next) else {
                                return mine;
                            };
                            mine.push((at, load_one(&paths[at], ids[at])));
                        }
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("a loader thread panicked"))
                .collect()
        });
        for (at, result) in claimed.into_iter().flatten() {
            loaded[at] = Some(result);
        }
    } else {
        for &at in &todo {
            loaded[at] = Some(load_one(&paths[at], ids[at]));
        }
    }

    gap!(_lap, "load: read the rest");
    // Their interface digests first, for the same reason as the members below.
    let digestible: Vec<Arc<ParsedObject>> = todo
        .iter()
        .filter_map(
            |at| match loaded[*at].as_ref().and_then(|r| r.as_ref().ok()) {
                Some(Loaded::Object(object)) => Some(Arc::clone(&object.parsed)),
                _ => None,
            },
        )
        .collect();
    session.seed_interfaces(&digestible);

    // Everything freshly read goes into the session for the next link.
    for &at in &todo {
        match loaded[at].as_ref().and_then(|r| r.as_ref().ok()) {
            Some(Loaded::Object(object)) => {
                session.store_object(&paths[at], &object.parsed, object.data.backing())
            }
            Some(Loaded::Archive(path, index, data, symbols)) => {
                session.store_archive(path, index, data, *symbols)
            }
            None => {}
        }
    }

    let mut objects: Vec<LoadedObject> = Vec::new();
    let mut archives: Vec<ArchiveInput> = Vec::new();
    for slot in loaded {
        match slot.expect("every input was visited")? {
            Loaded::Object(object) => objects.push(object),
            Loaded::Archive(path, index, data, _) => archives.push((path, index, data)),
        }
    }

    if archives.is_empty() {
        return Ok(objects);
    }
    // The first id an extracted member gets: every top-level object has one by
    // now, and members are numbered after them in extraction order.
    let first_member_id = next_id;

    // The archive sub-list the extraction order is indexed against. A rename
    // of the loose objects — every debug rebuild — leaves this identical, so
    // the order survives; a changed set of archives invalidates it.
    let archive_paths: Vec<PathBuf> = archives.iter().map(|(path, _, _)| path.clone()).collect();

    // The order a previous link settled on, when every input came from memory
    // and so cannot have changed what any archive is asked for. Replaying it
    // skips the frontier entirely: with the parses held, those rounds are the
    // whole of what this stage still costs.
    gap!(_lap, "load: archive indexes");
    if let Some(order) = session.extraction(&archive_paths) {
        let order = order.to_vec();
        for (position, (archive_index, member)) in order.iter().enumerate() {
            let (path, index, data) = &archives[*archive_index];
            let id = ObjectId(first_member_id + position as u32);
            let member_id = blinker_archive::MemberId(*member);
            let entry = index.member(member_id);
            let fresh = entry
                .and_then(|entry| blinker_archive::member_data(data, entry, path).ok())
                .unwrap_or_default();
            let window = entry
                .map(|entry| entry.offset as usize..entry.offset as usize + fresh.len())
                .unwrap_or(0..0);
            let loaded = match session.member(path, *member, fresh) {
                Some((parsed, _)) if parsed.id == id => LoadedObject {
                    parsed,
                    data: SourceBytes::window(data, window),
                    path: Arc::from(path.as_path()),
                    member: entry.map(|entry| Arc::from(entry.name.as_str())),
                    unchanged: true,
                },
                // A member the session lost — it can only have been dropped
                // with its archive, and its archive cannot have changed, so
                // this is a plan recorded before the member cache had it.
                _ => parse_member(path, index, data, member_id, id)?,
            };
            objects.push(loaded);
        }
        return Ok(objects);
    }

    // Which archive defines each name, across all of them at once.
    //
    // The frontier used to ask every archive in turn for every name it wanted,
    // and each of those is a binary search with string comparisons. On a debug
    // rust-analyzer link — 341 archives, tens of thousands of names — that was
    // 517 ms of the 876 ms `read_and_parse` stage, on a relink whose members
    // all came out of memory and where `parse` measured zero.
    //
    // First definition wins, which is what the loop it replaces did twice
    // over: `member_defining` takes the first entry within an archive, and the
    // scan stopped at the first archive that answered. Iterating the archives
    // in order and inserting only when absent gives the same answer.
    gap!(_lap, "load: probe + read + parse");
    let defining = DefiningIndex::build(&archives);

    gap!(_lap, "load: defining map");
    // Pull members in until nothing new is needed.
    let mut extracted: HashSet<(usize, u32)> = HashSet::default();
    let mut order: Vec<(usize, u32)> = Vec::new();
    let mut frontier = Frontier::default();
    let internable: Vec<Arc<ParsedObject>> = objects
        .iter()
        .map(|object| Arc::clone(&object.parsed))
        .collect();
    session.seed_interned(&internable);
    for object in &objects {
        let ids = session.interned(&object.parsed);
        frontier.absorb(object, &ids);
    }
    // Names already offered to every archive. One that no archive defines will
    // still be wanted next round, and asking again cannot produce a different
    // answer — the archives do not change.
    gap!(_lap, "load: seed + absorb");
    let mut probed: HashSet<SymbolNameId> = HashSet::default();
    loop {
        let unprobed: Vec<SymbolNameId> = frontier
            .wanted
            .iter()
            .copied()
            .filter(|name| !probed.contains(name))
            .collect();
        probed.extend(unprobed.iter().copied());
        let mut added = false;

        // Which members this round wants, in the order it wants them. Chosen
        // before any of them is parsed, so the ids below are assigned by
        // position and no thread's timing can reach the output.
        //
        // Scoped, because everything in here reads the interning table and
        // everything after it needs the session mutably. The names used to be
        // copied out to end that borrow — sixty-two thousand `String`s a link,
        // allocated to be sorted once and dropped.
        let round: Vec<(usize, blinker_archive::MemberId)> = {
            // Sorted by name, which is what decides the order members are
            // pulled in and therefore what id each one gets. `frontier.wanted`
            // is a hash set of ids, and neither its iteration order nor the
            // ids' numeric order is stable across processes — this is where
            // that is repaired.
            let names = session.names();
            let mut wanted: Vec<&str> = unprobed
                .iter()
                .filter_map(|name| names.resolve(*name))
                .collect();
            gap!(_lap, "pick: collect");
            wanted.sort_unstable();
            gap!(_lap, "pick: sort");

            // Which archive defines each wanted name, asked on every core.
            //
            // `defining` is read-only here, and a name's answer does not depend
            // on any other name's — so the question is the shape finding 170
            // found in the interner, and it was costing the same way. Sixty-two
            // thousand lookups at 450 ns each on a warm link: hash sixty bytes
            // of mangled name, miss into a half-million-entry table, chase a
            // pointer to the `String` it holds to compare the text. One name in
            // flight at a time.
            //
            // The answers come back in `wanted` order, which is the order the
            // members are pulled in and therefore what id each one gets — so
            // the chunking has to preserve it, and `map_chunks` is what does.
            let found = parallel::map_chunks(&wanted, |_, chunk| {
                chunk
                    .iter()
                    .map(|name| defining.get(name))
                    .collect::<Vec<_>>()
            });

            let mut round = Vec::new();
            for (archive_index, member_id) in found.into_iter().flatten().flatten() {
                if !extracted.insert((archive_index, member_id.0)) {
                    continue; // already in the link
                }
                round.push((archive_index, member_id));
            }
            round
        };
        gap!(_lap, "round: pick");
        if round.is_empty() {
            gap!(_lap, "load: extraction rounds tail");
            session.store_extraction(archive_paths, order);
            return Ok(objects);
        }
        order.extend(round.iter().map(|(archive, member)| (*archive, member.0)));

        // Parsing a member touches nothing shared. A round is typically
        // dozens of them and a Rust link has 900 in total, all of which were
        // parsed one after another on one thread.
        let base = next_id;
        next_id += round.len() as u32;

        // Members this process already parsed, taken before any thread starts.
        // The id check is the whole safety argument: a held member carries the
        // id it was parsed with, and everything downstream keys on that id. It
        // matches whenever the extraction order does, and when it does not the
        // member is simply parsed again rather than served under a name that
        // now means something else.
        //
        // On every core, because proving a member unchanged is a `memcmp` and
        // a round of them is the whole archive: 5,504 members against 800 MB.
        // Sequentially that was 80 ms — more than the re-parsing it replaced
        // (174). Nothing here mutates the session, so the only thing the
        // chunking has to preserve is the position each answer belongs at, and
        // `map_chunks` returns them in order.
        let archives_ref = &archives;
        let held_session = &*session;
        let held: Vec<Option<LoadedObject>> = parallel::map_chunks(&round, |start, chunk| {
            chunk
                .iter()
                .enumerate()
                .map(|(offset, (archive_index, member_id))| {
                    let at = start + offset;
                    let (path, index, data) = &archives_ref[*archive_index];
                    let entry = index.member(*member_id)?;
                    let fresh = blinker_archive::member_data(data, entry, path).ok()?;
                    let (parsed, _) = held_session.member(path, member_id.0, fresh)?;
                    if parsed.id != ObjectId(base + at as u32) {
                        return None;
                    }
                    let begin = entry.offset as usize;
                    Some(LoadedObject {
                        parsed,
                        data: SourceBytes::window(data, begin..begin + fresh.len()),
                        path: Arc::from(path.as_path()),
                        member: Some(Arc::from(entry.name.as_str())),
                        unchanged: true,
                    })
                })
                .collect::<Vec<_>>()
        })
        .into_iter()
        .flatten()
        .collect();
        gap!(_lap, "round: prove held");
        let todo: Vec<usize> = (0..round.len()).filter(|at| held[*at].is_none()).collect();

        let round = &round;
        let todo_ref = &todo;
        let parsed: Vec<Result<LoadedObject, LinkError>> = {
            let cursor = std::sync::atomic::AtomicUsize::new(0);
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(todo.len());
            let claimed: Vec<Vec<(usize, Result<LoadedObject, LinkError>)>> =
                std::thread::scope(|scope| {
                    let workers: Vec<_> = (0..threads.max(1))
                        .map(|_| {
                            let cursor = &cursor;
                            scope.spawn(move || {
                                let mut mine = Vec::new();
                                loop {
                                    let next =
                                        cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    let Some(&at) = todo_ref.get(next) else {
                                        return mine;
                                    };
                                    let (archive_index, member_id) = round[at];
                                    let (path, index, data) = &archives_ref[archive_index];
                                    mine.push((
                                        at,
                                        parse_member(
                                            path,
                                            index,
                                            data,
                                            member_id,
                                            ObjectId(base + at as u32),
                                        ),
                                    ));
                                }
                            })
                        })
                        .collect();
                    workers
                        .into_iter()
                        .map(|worker| worker.join().expect("a member parser panicked"))
                        .collect()
                });
            let mut ordered: Vec<Option<Result<LoadedObject, LinkError>>> =
                (0..round.len()).map(|_| None).collect();
            for (at, result) in claimed.into_iter().flatten() {
                ordered[at] = Some(result);
            }
            for (at, member) in held.into_iter().enumerate() {
                if let Some(member) = member {
                    ordered[at] = Some(Ok(member));
                }
            }
            ordered
                .into_iter()
                .map(|slot| slot.expect("every member was visited"))
                .collect()
        };

        // The round's names and interface digests at once, while there are
        // still fifteen cores to do it on. Both are wanted one object at a time
        // below — `store_member` needs a digest each, `interned` an id vector.
        let derivable: Vec<Arc<ParsedObject>> = parsed
            .iter()
            .filter_map(|loaded| loaded.as_ref().ok())
            .map(|loaded| Arc::clone(&loaded.parsed))
            .collect();
        gap!(_lap, "round: parse");
        session.seed_interned(&derivable);
        let digestible: Vec<Arc<ParsedObject>> = todo
            .iter()
            .filter_map(|at| parsed[*at].as_ref().ok())
            .map(|loaded| Arc::clone(&loaded.parsed))
            .collect();
        session.seed_interfaces(&digestible);

        // Freshly parsed members go into the session; held ones are already in
        // it, and re-storing them would be a map write per member per link.
        let mut fresh = todo.iter().copied().peekable();
        for (at, loaded) in parsed.iter().enumerate() {
            if fresh.peek() != Some(&at) {
                continue;
            }
            fresh.next();
            if let Ok(loaded) = loaded {
                let (path, _, data) = &archives[round[at].0];
                session.store_member(
                    path,
                    round[at].1 .0,
                    &loaded.parsed,
                    loaded.data.range(),
                    data,
                );
            }
        }

        gap!(_lap, "round: seed + store");
        for loaded in parsed {
            let loaded = loaded?;
            let ids = session.interned(&loaded.parsed);
            frontier.absorb(&loaded, &ids);
            objects.push(loaded);
            added = true;
        }
        gap!(_lap, "round: absorb");

        if !added {
            gap!(_lap, "load: extraction rounds");
            session.store_extraction(archive_paths, order);
            return Ok(objects);
        }
    }
}

/// Build the global symbol table and check it is complete.
///
/// Takes the session's interning table by reference and puts it back before
/// returning, on both the success and the failure path. It must not be moved
/// out and dropped on an error: the id vectors in `interned` are only
/// meaningful against this table, and a link that failed is followed by a link
/// that reuses every one of them.
fn duplicate_definitions(
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    names: &SymbolNames,
) -> Vec<SymbolNameId> {
    // Saturating counts, indexed by name id — no hashing at all. A `Vec` and
    // not a map because the ids are dense from zero by construction, and the
    // whole question is asked once per symbol in the link.
    let mut strong: Vec<u8> = vec![0; names.len()];
    for (slot, object) in objects.iter().enumerate() {
        let ids = &interned[slot];
        for symbol in &object.parsed.symbols {
            // A local definition is invisible outside its object, and two
            // objects may legitimately define the same local name.
            if symbol.strength != SymbolStrength::Strong
                || symbol.visibility == SymbolVisibility::Local
            {
                continue;
            }
            let Some(name) = ids.get(symbol.id.0 as usize) else {
                continue;
            };
            let count = &mut strong[name.0 as usize];
            *count = count.saturating_add(1);
        }
    }
    strong
        .into_iter()
        .enumerate()
        .filter(|(_, count)| *count > 1)
        .map(|(index, _)| SymbolNameId(index as u32))
        .collect()
}

/// Explain a set of duplicate definitions, naming every competitor.
///
/// The slow path, and only the slow path: it builds the full resolution table,
/// which is what carries the candidates. That build is 93 ms on a debug
/// rust-analyzer link, and paying it on every successful link to describe
/// errors that were not there is what finding 162 is about.
fn explain_duplicates(
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    imports: &[String],
    names: &mut SymbolNames,
    duplicated: &[SymbolNameId],
) -> Vec<String> {
    let wanted: crate::hashing::FastSet<SymbolNameId> = duplicated.iter().copied().collect();
    let mut table = SymbolTable::with_names(std::mem::take(names));
    for name in imports {
        table.define_dynamic(name, 0);
    }
    for (slot, object) in objects.iter().enumerate() {
        let ids = &interned[slot];
        for symbol in &object.parsed.symbols {
            let Some(name_id) = ids.get(symbol.id.0 as usize).copied() else {
                continue;
            };
            if !wanted.contains(&name_id) {
                continue;
            }
            if symbol.strength.is_definition() {
                table.define_id(
                    name_id,
                    SymbolProvider::Object {
                        object: object.parsed.id,
                        symbol: symbol.id,
                    },
                    symbol.strength,
                    symbol.visibility,
                );
            } else {
                table.reference_id(name_id, object.parsed.id, symbol.strength);
            }
        }
    }
    let reported = table
        .errors()
        .iter()
        .filter_map(|error| match error {
            blinker_symbols::SymbolError::Duplicate(duplicate) => {
                let name = table.name_of(duplicate.name).unwrap_or("<unknown>");
                let objects: Vec<String> = duplicate
                    .candidates
                    .iter()
                    .filter_map(|candidate| match candidate.provider {
                        SymbolProvider::Object { object, .. }
                        | SymbolProvider::ArchiveMember { object, .. } => Some(object),
                        _ => None,
                    })
                    .map(|object| {
                        objects
                            .get(object.0 as usize)
                            .map(|held| held.path.display().to_string())
                            .unwrap_or_else(|| format!("object {}", object.0))
                    })
                    .collect();
                Some(format!("{name} (defined in {})", objects.join(", ")))
            }
            blinker_symbols::SymbolError::Undefined(_) => None,
        })
        .collect();
    *names = table.into_names();
    reported
}

/// Read and parse one archive member.
fn parse_member(
    path: &Path,
    index: &blinker_archive::ArchiveIndex,
    data: &std::sync::Arc<mapping::Backing>,
    member_id: blinker_archive::MemberId,
    id: ObjectId,
) -> Result<LoadedObject, LinkError> {
    let member = index
        .member(member_id)
        .ok_or(LinkError::MissingObject { object: id })?;
    let bytes = blinker_archive::member_data(data, member, path)
        .map_err(|source| LinkError::Archive(Box::new(source)))?;
    let parsed = parse_object(bytes, path, Some(&member.name), id)
        .map_err(|source| LinkError::Parse(Box::new(source)))?;
    let start = member.offset as usize;
    Ok(LoadedObject {
        parsed: std::sync::Arc::new(parsed),
        data: SourceBytes::window(data, start..start + bytes.len()),
        path: Arc::from(path),
        member: Some(Arc::from(member.name.as_str())),
        unchanged: false,
    })
}

/// One archive as the loader holds it: where it came from, its index, and the
/// bytes both borrow from.
type ArchiveInput = (
    PathBuf,
    std::sync::Arc<blinker_archive::ArchiveIndex>,
    std::sync::Arc<mapping::Backing>,
);

/// Which archive defines each name, across all of them at once.
///
/// The frontier used to ask every archive in turn for every name it wanted,
/// and each of those was a binary search with string comparisons. On a debug
/// rust-analyzer link — 341 archives, tens of thousands of names — that was
/// 517 ms of the 876 ms `read_and_parse` stage, on a relink whose members all
/// came out of memory and where `parse` measured zero (finding 78).
///
/// # Keyed by the name's hash, not by the name
///
/// The replacement was a `HashMap<&str, _>`, and it inherited the cost the
/// binary search had: hashing the text. Two million entries of sixty-odd bytes
/// is 120 MB of mangled name hashed on one thread to build an index that
/// answers sixty-two thousand questions.
///
/// Hashing a name touches nothing shared, so it belongs on every core; filing
/// the answer has to be serial. Keying the table by the `u64` instead is what
/// lets the two be separated — the same split [`blinker_symbols::SymbolNames`]
/// makes for the same reason (finding 175), and for the same reason the text
/// is kept and compared rather than trusted: a table keyed by a hash is a
/// lookup structure, not a claim that the hash is unique.
struct DefiningIndex<'a> {
    /// Hash -> the first archive defining a name that hashes there.
    first: blinker_hashing::FastMap<u64, Definition<'a>>,
    /// Definitions displaced by a *different* name hashing the same, in the
    /// order they were seen. Empty on every link measured; present because
    /// "empty in practice" and "cannot happen" are different claims, and the
    /// consequence of the second being wrong is extracting the wrong member.
    collided: Vec<(u64, Definition<'a>)>,
}

/// Where one name is defined: the archive's position in the link, and the
/// member within it.
#[derive(Clone, Copy)]
struct Definition<'a> {
    name: &'a str,
    archive: usize,
    member: blinker_archive::MemberId,
}

impl<'a> DefiningIndex<'a> {
    fn build(archives: &'a [ArchiveInput]) -> DefiningIndex<'a> {
        // Every name's hash, on every core, in archive order.
        let hashed = parallel::map_chunks(archives, |start, chunk| {
            chunk
                .iter()
                .enumerate()
                .flat_map(|(at, (_, index, _))| {
                    index.symbol_map.iter().map(move |(name, member)| {
                        (
                            blinker_symbols::hash_of(name),
                            Definition {
                                name: name.as_str(),
                                archive: start + at,
                                member: *member,
                            },
                        )
                    })
                })
                .collect::<Vec<_>>()
        });

        let total = archives
            .iter()
            .map(|(_, index, _)| index.symbol_map.len())
            .sum();
        DefiningIndex::from_hashed(hashed.into_iter().flatten(), total)
    }

    /// File already-hashed definitions, in the order they must be preferred.
    ///
    /// Separate from [`DefiningIndex::build`] so the collision branch can be
    /// tested. Two distinct names hashing to one `u64` cannot be produced by
    /// writing symbol names in a test — that is the point of a 64-bit hash —
    /// so a test that goes through `build` can only ever exercise the path
    /// that does not collide, and the branch whose failure extracts the wrong
    /// member would be the one nothing ran.
    fn from_hashed(
        entries: impl Iterator<Item = (u64, Definition<'a>)>,
        capacity: usize,
    ) -> DefiningIndex<'a> {
        let mut first =
            blinker_hashing::FastMap::with_capacity_and_hasher(capacity, Default::default());
        let mut collided = Vec::new();
        // First definition wins, which is what the archive scan it replaced did
        // twice over: the symbol table lists a name under the earliest member
        // defining it, and the scan stopped at the first archive that answered.
        // Walking the archives in order and inserting only when absent gives
        // the same answer.
        for (hash, definition) in entries {
            match first.entry(hash) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(definition);
                }
                std::collections::hash_map::Entry::Occupied(held) => {
                    if held.get().name != definition.name {
                        collided.push((hash, definition));
                    }
                }
            }
        }
        DefiningIndex { first, collided }
    }

    fn get(&self, name: &str) -> Option<(usize, blinker_archive::MemberId)> {
        self.at(blinker_symbols::hash_of(name), name)
    }

    /// The lookup, with the hash supplied. Split for the same reason
    /// [`DefiningIndex::from_hashed`] is: a test cannot make `hash_of` collide,
    /// so a test that goes in through `get` cannot reach the branch below.
    fn at(&self, hash: u64, name: &str) -> Option<(usize, blinker_archive::MemberId)> {
        let found = self.first.get(&hash)?;
        let definition = if found.name == name {
            *found
        } else {
            // Only reachable when two distinct names hash alike, and then only
            // for the later one. Insertion order is archive order, so the first
            // match here is the first definition, as above.
            *self
                .collided
                .iter()
                .find(|(at, held)| *at == hash && held.name == name)
                .map(|(_, held)| held)?
        };
        Some((definition.archive, definition.member))
    }
}

/// Every input section that belongs in the output, in object order.
///
/// A section contributes its *stripped* size. Nothing else in layout changes:
/// the survivors keep their order inside the contribution, so the section is
/// still one run of bytes — just a shorter one.
fn placements_for(objects: &[LoadedObject], strip: &Strip) -> Vec<InputPlacement> {
    let mut placements = Vec::new();
    for object in objects {
        for section in &object.parsed.sections {
            if is_linker_internal(section) {
                continue;
            }
            placements.push(InputPlacement {
                object: object.parsed.id,
                section: section.id,
                segment: section.segment.clone(),
                name: section.name.clone(),
                kind: section.kind,
                size: strip.size_of(object.parsed.id, section.id, section.size),
                alignment: section.alignment,
            });
        }
    }
    placements
}

/// Assemble an image from the current knowledge.
///
/// `contents` is keyed by output-section index; an empty map produces an image
/// whose sections are zero-filled, which is what the first pass wants.
/// Everything the emitter needs, gathered so the parameter list stays a list
/// of *decisions* rather than of accumulated arguments.
#[derive(Default)]
struct Assembly<'a> {
    /// Whether this pass produces the real image. The layout probe does not,
    /// and signing what it produces hashes megabytes that are then dropped.
    final_pass: bool,
    placements: &'a [InputPlacement],
    symbols: &'a [OutputSymbol<'a>],
    contents: Option<&'a HashMap<usize, Vec<u8>>>,
    rebases: &'a [Rebase],
    binds: &'a [Bind],
    entry_offset: u64,
    /// The previous link's placements, when there are any to build on.
    ///
    /// Both passes get the same one. The probe exists to size the load
    /// commands, and sizing them against a layout the real pass will not
    /// produce is how a reservation comes to be wrong.
    previous: Option<&'a (blinker_layout::PreviousLayout, PlacementKeys)>,
    /// Room to reserve per contribution. See [`full_sizes`].
    reservations: Option<&'a blinker_output::PlacementReservations>,
    /// The previous link's signed bytes and page hashes, so pages that did not
    /// change are not hashed again.
    previous_signature: Option<&'a (Vec<u8>, Vec<[u8; 32]>)>,
    /// The dynamic libraries this image loads, in ordinal order.
    ///
    /// Derived from the stub libraries that were actually resolved rather than
    /// taken from the request, because the install name is written inside the
    /// `.tbd` and is not knowable from the command line: `-framework
    /// CoreFoundation` names a directory, and what goes in `LC_LOAD_DYLIB` is
    /// `/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation`.
    dylibs: &'a [Dylib],
}

/// Contribution identities in the form the output crate accepts: a plain map,
/// because that crate must not be able to reach an `ObjectId` through it.
type PlacementKeys = std::collections::HashMap<(u32, u32), blinker_layout::ContributionKey>;

/// Room to reserve per contribution: its size *before* dead-stripping.
///
/// A contribution occupies what survived stripping, and what survives is
/// decided by what reaches it — so an object whose file nobody touched can
/// keep more of itself on the next link and outgrow a slot sized from this
/// one. Reserving against the unstripped size is what stops that from moving
/// an input that did not change (finding 97). It costs image size in
/// proportion to what stripping removes, which is why it is reserved and not
/// occupied: the bytes are never written, only skipped over.
fn full_sizes(objects: &[LoadedObject]) -> blinker_output::PlacementReservations {
    let mut sizes = blinker_output::PlacementReservations::new();
    for object in objects {
        for section in &object.parsed.sections {
            sizes.insert((object.parsed.id.0, section.id.0), section.size);
        }
    }
    sizes
}

fn assemble(request: &LinkRequest, assembly: &Assembly<'_>) -> Result<Image, LinkError> {
    let Assembly {
        placements,
        symbols: output_symbols,
        contents,
        rebases,
        binds,
        entry_offset,
        final_pass: _,
        previous: _,
        reservations: _,
        previous_signature: _,
        dylibs: _,
    } = *assembly;
    let mut builder = ImageBuilder::new();
    if !assembly.final_pass {
        builder.unsigned();
    }
    // Padding is for the next link, not this one: it costs image size and buys
    // the property that an edit which grows one contribution does not move
    // every contribution after it — which is what keeps the cache's placement
    // keys valid across an edit. A link that is not writing a cache has no
    // next link to help, so it pays nothing.
    if let Some((image, hashes)) = assembly.previous_signature {
        builder.reusing_signature(image.clone(), hashes.clone());
    }
    if let Some((previous, keys)) = assembly.previous {
        builder.reusing_layout(previous.clone(), keys.clone());
        if let Some(reservations) = assembly.reservations {
            builder.reserving(reservations.clone());
        }
    }
    if request.stable_layout {
        builder.slop(blinker_layout::Slop::DEFAULT);
    }
    for placement in placements {
        builder.input(placement.clone());
    }
    for dylib in assembly.dylibs {
        builder.dylib(dylib.clone());
    }
    builder.identifier(&request.identifier);
    builder.entry_offset(entry_offset);

    // Sections with no supplied content are emitted as zeroes of the right
    // size, so the first pass produces a valid image to read the layout from.
    if let Some(contents) = contents {
        for index in 0..placements.len() {
            if let Some(bytes) = contents.get(&index) {
                builder.content(index, bytes.clone());
            }
        }
    }

    for symbol in output_symbols {
        builder.symbols().add(symbol.clone());
    }
    for rebase in rebases {
        builder.rebase(*rebase);
    }
    for bind in binds {
        builder.bind(bind.clone());
    }
    builder.build().map_err(LinkError::Emit)
}

/// A name the assembler invented for its own use, which does not belong in the
/// output.
///
/// Mach-O reserves the `L` prefix for temporary labels — `ltmp0`, `Lloh4`,
/// `l_.str` — and the assembler emits them in bulk to anchor section starts
/// and literals. They outnumber real symbols, they are meaningless outside the
/// object that made them, and emitting them makes a backtrace *worse*: a
/// section-anchor label sits at the exact address a real function starts, so
/// the symbolicator ties for nearest-match and can name the frame `ltmp0`.
///
/// The prefix is unambiguous because every C and Rust symbol reaching the
/// linker carries a leading underscore, so a static named `ltmp` arrives as
/// `_ltmp`. `ld` applies the same rule; a binary it links contains no `L`
/// symbol at all.
fn is_temporary_label(name: &str) -> bool {
    name.starts_with('L') || name.starts_with('l')
}

/// The output symbol table: every definition, at the address layout gave it.
///
/// # Locals are not optional
///
/// They were dropped once, on the reasoning that a local is invisible outside
/// its object and only a debugger would want it. Both halves are wrong, and
/// the second is the dangerous one. Most Rust functions are local — anything
/// not `pub`, plus nearly every monomorphisation out of `std` — and the
/// consumer is not a debugger but the panicking program itself, symbolicating
/// its own backtrace from its own symbol table.
///
/// A symbolicator resolves an address to the nearest symbol at or below it, so
/// omitting the locals does not omit the answer. It moves the answer to
/// whatever global happens to precede the frame, and prints that with no mark
/// of uncertainty. `crates/cli/tests/backtraces_name_the_right_function.rs`
/// has the observed output: four frames inside a private recursive function
/// reported as `core::fmt::rt::Argument::new_display`.
fn output_symbols<'a>(placed: &[PlacedSymbol<'a>]) -> Vec<OutputSymbol<'a>> {
    // Sorted per chunk on every core, then merged. A single sort of 380,000
    // entries compares mangled names about 6.9 million times; sixty sorted
    // runs merged by a heap is 2.2 million, and the sorts themselves overlap.
    let sorted = crate::parallel::map_chunks(placed, |_, chunk| {
        let mut chunk = output_symbols_of(chunk);
        chunk.sort_unstable_by(compare_output_symbols);
        chunk
    });
    merge_sorted(sorted)
}

/// Order two entries of the symbol table.
///
/// Locals may share a name across objects, so the name alone no longer orders
/// the table — address and section break the tie.
fn compare_output_symbols(a: &OutputSymbol<'_>, b: &OutputSymbol<'_>) -> std::cmp::Ordering {
    a.name
        .cmp(&b.name)
        .then(a.value.cmp(&b.value))
        .then(a.section.cmp(&b.section))
}

/// Merge sorted runs into one sorted vector, ties resolved by run order.
///
/// A tree of two-way merges rather than one k-way heap. Both do the same
/// `n log k` comparisons, but a heap pays a sift per element on one core,
/// while the tree is a single comparison per element and its early rounds —
/// which are nearly all of the work — run on every core. The heap version of
/// this merged 379,857 entries in 41 ms against 4.9 ms for the sorts feeding
/// it, which is the whole reason the sorts were parallelised.
///
/// Ties take the earlier run, at every level, so the result is exactly the
/// order a single sort of the concatenation would give — a fixed function of
/// the chunk boundaries, which are fixed before any thread starts.
fn merge_sorted<'a>(mut runs: Vec<Vec<OutputSymbol<'a>>>) -> Vec<OutputSymbol<'a>> {
    /// One pending two-way merge: where it belongs in the next round, and the
    /// two runs feeding it.
    type Pending<'a> = (usize, Vec<OutputSymbol<'a>>, Vec<OutputSymbol<'a>>);

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    while runs.len() > 1 {
        // Adjacent pairs, so "earlier run wins a tie" composes up the tree.
        let mut pairs: Vec<Pending<'a>> = Vec::new();
        let mut odd: Option<Vec<OutputSymbol<'a>>> = None;
        let mut drained = runs.into_iter();
        let mut at = 0usize;
        while let Some(left) = drained.next() {
            match drained.next() {
                Some(right) => {
                    pairs.push((at, left, right));
                    at += 1;
                }
                // An odd run rides to the next round untouched, and stays last.
                None => odd = Some(left),
            }
        }

        let mut buckets: Vec<Vec<Pending<'a>>> = (0..threads.min(pairs.len().max(1)))
            .map(|_| Vec::new())
            .collect();
        for (index, pair) in pairs.into_iter().enumerate() {
            let bucket = index % buckets.len();
            buckets[bucket].push(pair);
        }

        let done: Vec<Vec<(usize, Vec<OutputSymbol<'a>>)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = buckets
                .into_iter()
                .map(|bucket| {
                    scope.spawn(move || {
                        bucket
                            .into_iter()
                            .map(|(index, left, right)| (index, merge_two(left, right)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("a merge worker panicked"))
                .collect()
        });

        let count = done.iter().map(Vec::len).sum();
        let mut ordered: Vec<Option<Vec<OutputSymbol<'a>>>> = (0..count).map(|_| None).collect();
        for (index, merged) in done.into_iter().flatten() {
            ordered[index] = Some(merged);
        }
        runs = ordered
            .into_iter()
            .map(|run| run.expect("every pair was merged exactly once"))
            .chain(odd)
            .collect();
    }
    runs.pop().unwrap_or_default()
}

/// Two sorted runs into one. A tie takes `left`, which is the earlier run.
fn merge_two<'a>(
    left: Vec<OutputSymbol<'a>>,
    right: Vec<OutputSymbol<'a>>,
) -> Vec<OutputSymbol<'a>> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    loop {
        let take_right = match (left.peek(), right.peek()) {
            (Some(a), Some(b)) => compare_output_symbols(b, a) == std::cmp::Ordering::Less,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => break,
        };
        if take_right {
            out.push(right.next().expect("peeked"));
        } else {
            out.push(left.next().expect("peeked"));
        }
    }
    out
}

/// One chunk's placed definitions as symbol-table entries, unsorted.
fn output_symbols_of<'a>(placed: &[PlacedSymbol<'a>]) -> Vec<OutputSymbol<'a>> {
    placed
        .iter()
        .map(|symbol| match symbol.visibility {
            SymbolVisibility::Local => {
                OutputSymbol::local(symbol.name, symbol.section_number, symbol.address)
                    .keyed(symbol.key)
            }
            SymbolVisibility::Global => {
                OutputSymbol::exported(symbol.name, symbol.section_number, symbol.address)
                    .keyed(symbol.key)
            }
            SymbolVisibility::PrivateExternal => {
                let mut exported =
                    OutputSymbol::exported(symbol.name, symbol.section_number, symbol.address)
                        .keyed(symbol.key);
                exported.private_external = true;
                exported
            }
        })
        .collect()
}

/// One definition, with everywhere it ended up.
///
/// Computed once because two consumers need the same answer and disagreeing
/// would be worse than either being wrong: the symbol table says where a name
/// is, and the debug map says which object's DWARF describes that address. A
/// symbol present in one and absent from the other is a frame that resolves to
/// a name with no source, or to a source with the wrong name.
struct PlacedSymbol<'a> {
    name: &'a str,
    /// The interned id of `name`, so the string table can deduplicate it
    /// without hashing the text. See `OutputSymbol::key`.
    key: u32,
    visibility: SymbolVisibility,
    /// Index into the `objects` slice this came from.
    object: usize,
    section: SectionId,
    /// One-based output section number, for `n_sect`.
    section_number: u8,
    address: u64,
    /// Where the containing chunk ends, so the last definition in it can be
    /// sized without looking at the next chunk.
    chunk_end: u64,
    is_code: bool,
}

/// Every definition that survived to the output, placed.
///
/// # Locals are not optional
///
/// They were dropped once, on the reasoning that a local is invisible outside
/// its object and only a debugger would want it. Both halves are wrong, and
/// the second is the dangerous one. Most Rust functions are local — anything
/// not `pub`, plus nearly every monomorphisation out of `std` — and the
/// consumer is not a debugger but the panicking program itself, symbolicating
/// its own backtrace from its own symbol table.
///
/// A symbolicator resolves an address to the nearest symbol at or below it, so
/// omitting the locals does not omit the answer. It moves the answer to
/// whatever global happens to precede the frame, and prints that with no mark
/// of uncertainty. `crates/cli/tests/backtraces_name_the_right_function.rs`
/// has the observed output: four frames inside a private recursive function
/// reported as `core::fmt::rt::Argument::new_display`.
fn placed_symbols<'a>(
    objects: &'a [LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    placed: &Placed,
    strip: &Strip,
) -> Vec<PlacedSymbol<'a>> {
    // Per chunk, on every core, then concatenated in object order — which is
    // the order a single pass produced and which `debug_map` relies on to find
    // each object's run without a map.
    let chunks = crate::parallel::map_chunks(objects, |start, chunk| {
        placed_symbols_of(chunk, start, interned, placed, strip)
    });
    let mut out = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
    for chunk in chunks {
        out.extend(chunk);
    }
    out
}

/// One chunk's placed definitions. `base` is where the chunk starts in
/// `objects`, since a `PlacedSymbol` records which object it came from.
fn placed_symbols_of<'a>(
    objects: &'a [LoadedObject],
    base: usize,
    interned: &[Arc<Vec<SymbolNameId>>],
    placed: &Placed,
    strip: &Strip,
) -> Vec<PlacedSymbol<'a>> {
    let mut out = Vec::with_capacity(objects.iter().map(|o| o.parsed.symbols.len()).sum());
    for (at, object) in objects.iter().enumerate() {
        let index = base + at;
        for symbol in &object.parsed.symbols {
            if !symbol.strength.is_definition() || is_temporary_label(&symbol.name) {
                continue;
            }
            let Some(section_id) = symbol.section else {
                continue;
            };
            let Some(input) = object.parsed.section(section_id) else {
                continue;
            };
            // A stripped definition leaves the symbol table with its bytes; an
            // entry pointing at whatever moved into its place would be worse
            // than no entry at all.
            let Some(offset_in_section) = strip.remap(
                object.parsed.id,
                section_id,
                symbol.value.saturating_sub(input.vm_address),
            ) else {
                continue;
            };
            let Some((output_section, chunk)) = placed.chunk(object.parsed.id, section_id) else {
                continue;
            };
            // `n_sect` is one-based, and it is read: a debugger checks the
            // symbol's section against the address's before trusting the pair.
            // Every symbol claiming section 1 made data symbols look like
            // stray text.
            let Ok(number) = u8::try_from(output_section + 1) else {
                continue;
            };
            out.push(PlacedSymbol {
                name: &symbol.name,
                key: interned[index][symbol.id.0 as usize].0,
                visibility: symbol.visibility,
                object: index,
                section: section_id,
                section_number: number,
                address: chunk + offset_in_section,
                chunk_end: chunk + strip.size_of(object.parsed.id, section_id, input.size),
                is_code: input.kind == SectionKind::Code,
            });
        }
    }
    out
}

/// The debug map: where each function came from, so a debugger can find the
/// DWARF that describes it.
///
/// # What it is
///
/// A Mach-O executable does not carry its debug information. The DWARF stays
/// in the `.o` files, and the executable carries a table of stabs saying which
/// object each definition came from and what address it ended up at. `lldb`
/// reads it directly; `dsymutil` reads it to build a `.dSYM`. Without it a
/// binary has names and no line numbers — `deep` instead of `deep at
/// hello.rs:1:38` — and no way to break on a source line.
///
/// # The shape, copied from what `ld` emits
///
/// ```text
///   SO    "<dir>/"      the compilation unit opens
///   SO    "<file>"
///   OSO   "<path.o>"    n_desc 1, n_value the object's mtime
///     BNSYM              at the function's address
///     FUN  "_name"       at the function's address
///     FUN  ""            n_value is the function's *size*
///     ENSYM              at the function's address
///     GSYM "_data"       n_value 0: the address is in the symbol table
///     STSYM "_static"    at its address
///   SO    ""            and closes
/// ```
///
/// # The `SO` names are approximate, and measured to be harmless
///
/// `ld` fills them from the object's DWARF `DW_AT_comp_dir` and `DW_AT_name`,
/// which needs a DWARF parser. blinker derives them from the object's own
/// path. That was checked rather than assumed: rewriting `ld`'s own `SO`
/// strings in a linked binary to `/nowhere/XXX...` and `z.c` left `atos` still
/// reporting `a.c:2`, because the file and line come from the DWARF the `OSO`
/// points at, not from the `SO`. The `SO` names a compilation unit; it does
/// not locate anything.
///
/// # What it costs to be wrong here
///
/// An `OSO` naming an object that has moved, or whose mtime has changed, makes
/// a debugger report stale or missing line information rather than fail — so
/// this is a place where a silent wrong answer is easy. Objects with no debug
/// sections are skipped entirely rather than given an entry that points at
/// nothing.
fn debug_map<'a>(
    objects: &'a [LoadedObject],
    placed: &[PlacedSymbol<'a>],
) -> Vec<OutputSymbol<'a>> {
    // The map is per compilation unit, and sorted by address within one so a
    // definition's size is the distance to the next.
    //
    // `placed` is built by walking the objects in order, so each object's
    // symbols are already a contiguous run of it. Grouping them into a map
    // re-derived that — one hash and one push per symbol, and a `Vec`
    // allocation per object — from a fact the ordering already carried. Found
    // by looking for containers built from empty (135); this one was not too
    // small, it was unnecessary.
    let mut runs: Vec<(usize, usize)> = vec![(0, 0); objects.len()];
    let mut at = 0usize;
    while at < placed.len() {
        let object = placed[at].object;
        let start = at;
        while at < placed.len() && placed[at].object == object {
            at += 1;
        }
        if let Some(run) = runs.get_mut(object) {
            *run = (start, at);
        }
    }

    // Per chunk on every core, concatenated in object order. Each object's
    // stabs depend on that object and its own run of `placed` and on nothing
    // else, so the only thing the chunking has to preserve is the order the
    // compilation units appear in.
    let chunks = crate::parallel::map_chunks(objects, |base, chunk| {
        debug_map_of(chunk, base, placed, &runs)
    });
    let mut out = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
    for chunk in chunks {
        out.extend(chunk);
    }
    out
}

/// One chunk's compilation units. `base` is where the chunk starts in
/// `objects`, which is what indexes `runs`.
fn debug_map_of<'a>(
    objects: &'a [LoadedObject],
    base: usize,
    placed: &[PlacedSymbol<'a>],
    runs: &[(usize, usize)],
) -> Vec<OutputSymbol<'a>> {
    use blinker_output::symtab::stab;

    let mut out = Vec::new();
    // Reused across objects rather than allocated per object: only the order
    // within a run changes, and the run is a borrow of `placed`.
    let mut symbols: Vec<&PlacedSymbol<'_>> = Vec::new();
    for (at, object) in objects.iter().enumerate() {
        let index = base + at;
        if !object.parsed.metadata.has_debug_info {
            continue;
        }
        let (start, end) = runs[index];
        if start == end {
            continue;
        }
        symbols.clear();
        symbols.extend(placed[start..end].iter());
        symbols.sort_by_key(|symbol| (symbol.section.0, symbol.address));

        // The path this link read it from, not the one it was first parsed
        // from: an `OSO` stab names a file a debugger will go and open.
        let path: &Path = object.path.as_ref();
        let directory = path
            .parent()
            .map(|parent| format!("{}/", parent.display()))
            .unwrap_or_default();
        let file = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // An archive member is named the way `ld` names it, and carries a
        // timestamp of zero: the member has no mtime of its own, and the
        // archive's would go stale for reasons unrelated to this member.
        // `object.member`, not `parsed.metadata.member`, for the reason `path`
        // is used above: the parse may be one held from before the crate was
        // recompiled, and rustc renames every codegen unit when it is.
        let (oso, mtime) = match &object.member {
            Some(member) => (format!("{}({member})", path.display()), 0),
            None => (path.display().to_string(), object_mtime(path)),
        };

        out.push(OutputSymbol::stab(stab::SO, directory, NO_SECTION, 0, 0));
        out.push(OutputSymbol::stab(stab::SO, file, NO_SECTION, 0, 0));
        out.push(OutputSymbol::stab(stab::OSO, oso, NO_SECTION, 1, mtime));

        for (at, symbol) in symbols.iter().enumerate() {
            if symbol.is_code {
                // The distance to the next definition in the same chunk, or to
                // the end of the chunk for the last one. A function's size is
                // not recorded in a Mach-O symbol table, so it is the gap.
                let end = symbols
                    .get(at + 1)
                    .filter(|next| next.section == symbol.section)
                    .map(|next| next.address)
                    .unwrap_or(symbol.chunk_end);
                let size = end.saturating_sub(symbol.address);
                let n = symbol.section_number;
                out.push(OutputSymbol::stab(stab::BNSYM, "", n, 0, symbol.address));
                out.push(
                    OutputSymbol::stab(stab::FUN, symbol.name, n, 0, symbol.address)
                        .keyed(symbol.key),
                );
                out.push(OutputSymbol::stab(stab::FUN, "", NO_SECTION, 0, size));
                out.push(OutputSymbol::stab(stab::ENSYM, "", n, 0, symbol.address));
            } else if symbol.visibility == SymbolVisibility::Local {
                out.push(
                    OutputSymbol::stab(
                        stab::STSYM,
                        symbol.name,
                        symbol.section_number,
                        0,
                        symbol.address,
                    )
                    .keyed(symbol.key),
                );
            } else {
                // A global's address is already in the symbol table, so the
                // stab carries the name alone — which is what `ld` emits.
                out.push(
                    OutputSymbol::stab(stab::GSYM, symbol.name, NO_SECTION, 0, 0).keyed(symbol.key),
                );
            }
        }

        out.push(OutputSymbol::stab(stab::SO, "", 1, 0, 0));
    }
    out
}

/// `n_sect` for a stab that describes no particular section.
const NO_SECTION: u8 = 0;

/// The object's modification time, as the debug map records it.
///
/// Zero where it cannot be read: a debugger treats a mismatch as "the object
/// changed since the link" and says so, which is the right answer for an
/// object that has since been deleted, and better than refusing to link.
fn object_mtime(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Objects by id, so finding one is an index rather than a search.
///
/// The same shape as `Placed`, for the same reason (finding 77): a lookup
/// written as `objects.iter().find(...)` inside a loop over contributions is
/// quadratic, and a Rust link has ~900 objects and several thousand
/// contributions. It is invisible on a fixture with twelve objects, which is
/// exactly how the first one survived.
///
/// Ids are assigned by position and are dense, so this is a `Vec` rather than
/// a map — no hashing, and the gaps a sparse id space would leave cost one
/// `None` each.
struct ObjectIndex<'a> {
    by_id: Vec<Option<&'a LoadedObject>>,
}

impl<'a> ObjectIndex<'a> {
    fn build(objects: &'a [LoadedObject]) -> Self {
        let highest = objects
            .iter()
            .map(|object| object.parsed.id.0 as usize)
            .max();
        let mut by_id = vec![None; highest.map_or(0, |n| n + 1)];
        for object in objects {
            by_id[object.parsed.id.0 as usize] = Some(object);
        }
        ObjectIndex { by_id }
    }

    fn get(&self, id: ObjectId) -> Option<&'a LoadedObject> {
        self.by_id.get(id.0 as usize).copied().flatten()
    }
}

/// Copy each input section's bytes into its output section's buffer.
///
/// A stripped section arrives as a list of surviving runs rather than one, and
/// the gaps between them are simply not copied.
fn build_contents(
    objects: &[LoadedObject],
    image: &Image,
    strip: &Strip,
) -> Result<HashMap<usize, Vec<u8>>, LinkError> {
    let mut contents: HashMap<usize, Vec<u8>> = HashMap::default();
    let index_of = ObjectIndex::build(objects);

    for (index, section) in image.layout.sections.iter().enumerate() {
        if section.is_zero_filled() {
            continue;
        }
        let mut buffer = vec![0u8; section.size as usize];

        for contribution in &section.contributions {
            // Synthesised content (the GOT) has no input object to copy from;
            // it is filled in separately once addresses are known.
            if contribution.object == SYNTHETIC_OBJECT {
                continue;
            }
            let object = index_of
                .get(contribution.object)
                .ok_or(LinkError::MissingObject {
                    object: contribution.object,
                })?;
            let input =
                object
                    .parsed
                    .section(contribution.section)
                    .ok_or(LinkError::MissingSection {
                        object: contribution.object,
                        section: contribution.section,
                    })?;

            // A section with no file bytes (zero-filled in the input) leaves
            // its span in the buffer as the zeroes it already holds.
            let Some(file_offset) = input.file_offset else {
                continue;
            };
            // One run for a section that kept everything, several for one that
            // did not.
            let whole = [reachability::Piece {
                from: 0,
                size: input.size,
                to: 0,
            }];
            let pieces = strip
                .pieces(contribution.object, contribution.section)
                .unwrap_or(&whole);
            for piece in pieces {
                let start = (file_offset + piece.from) as usize;
                let end = start + piece.size as usize;
                let bytes = object
                    .data
                    .get(start..end)
                    .ok_or(LinkError::SectionOutOfBounds {
                        object: contribution.object,
                        section: contribution.section,
                    })?;
                let target = (contribution.offset + piece.to) as usize;
                buffer[target..target + bytes.len()].copy_from_slice(bytes);
            }
        }

        contents.insert(index, buffer);
    }

    Ok(contents)
}

/// Re-point every `__eh_frame` FDE at its CIE.
///
/// An FDE's second word is the distance from that word *back* to the CIE that
/// describes it. The assembler computed it, and no relocation covers it — so
/// compaction, which moves records apart, leaves every FDE pointing at
/// whatever now sits that far behind it. `lldb` reads the result and says so
/// plainly: "unable to find CIE at 0x1a8 for cie_id = 0xfc".
///
/// This is the only field in the link that is self-relative *and* unrelocated,
/// and it is the reason `__eh_frame` cannot be moved as opaque bytes the way
/// every other section can.
fn repair_eh_frame(
    contents: &mut SectionContents,
    image: &Image,
    objects: &[LoadedObject],
    strip: &Strip,
) {
    let Some((index, output)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == "__eh_frame")
    else {
        return;
    };
    let Some(buffer) = contents.get_mut(&index) else {
        return;
    };
    let offsets_of = contribution_offsets(output);

    for object in objects {
        for section in &object.parsed.sections {
            if section.name != "__eh_frame" {
                continue;
            }
            // A section that kept every record kept every distance with it.
            if strip.pieces(object.parsed.id, section.id).is_none() {
                continue;
            }
            let (Some(records), Some(&base), Some(file_offset)) = (
                reachability::eh_frame_boundaries(object, section),
                offsets_of.get(&(object.parsed.id.0, section.id.0)),
                section.file_offset,
            ) else {
                continue;
            };

            for record in records {
                let Some(placed) = strip.remap(object.parsed.id, section.id, record) else {
                    continue;
                };
                let at = (file_offset + record + 4) as usize;
                let Some(stored) = object
                    .data
                    .get(at..at + 4)
                    .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                else {
                    continue;
                };
                // Zero identifies a CIE, which points at nothing.
                if stored == 0 {
                    continue;
                }
                // The field holds `here - cie`, measured in the input.
                let Some(cie) = (record + 4).checked_sub(stored as u64) else {
                    continue;
                };
                let Some(cie_now) = strip.remap(object.parsed.id, section.id, cie) else {
                    continue;
                };
                let repaired = ((placed + 4) - cie_now) as u32;
                let write_at = (base + placed + 4) as usize;
                if let Some(slot) = buffer.get_mut(write_at..write_at + 4) {
                    slot.copy_from_slice(&repaired.to_le_bytes());
                }
            }
        }
    }
}

/// Patch every relocation against the addresses layout assigned.
/// Start of the per-thread block: the address `__thread_data` was placed at.
///
/// A TLV descriptor's third word is the variable's **offset within this
/// block**, not its address. dyld copies the block per thread, so an absolute
/// address there is meaningless — it rejects the image with "malformed
/// thread-local, offset=… is larger than total size".
fn thread_local_base(image: &Image) -> Option<u64> {
    image
        .layout
        .sections
        .iter()
        .filter(|s| s.name == "__thread_data" || s.name == "__thread_bss")
        .map(|s| s.vm_address)
        .min()
}

/// Whether an address falls inside the per-thread block.
fn in_thread_local_block(image: &Image, address: u64) -> bool {
    image
        .layout
        .sections
        .iter()
        .filter(|s| s.name == "__thread_data" || s.name == "__thread_bss")
        .any(|s| address >= s.vm_address && address < s.vm_address + s.size)
}

/// Section content keyed by output-section index.
type SectionContents = HashMap<usize, Vec<u8>>;

/// The indirection tables a relocation may need to reach its target.
///
/// Grouped because they travel together and are consulted by the same rules:
/// which one applies is decided by the relocation's kind, not by the caller.
struct IndirectTables<'a> {
    got: &'a HashMap<String, u64>,
    stubs: &'a HashMap<String, u64>,
    tlv: &'a HashMap<String, u64>,
    imports: &'a [String],
    /// Which dynamic library exports each importable name, for bind ordinals.
    exports: Option<&'a libraries::StubExports>,
    /// Offsets, per `(object, section)`, of CIE personality fields that use an
    /// indirect encoding.
    ///
    /// A CIE's augmentation encodes its personality with `DW_EH_PE_indirect`:
    /// the stored value addresses a *slot* holding the routine's address, not
    /// the routine. Resolving it like any other symbol reference wrote a
    /// function address where libunwind expected a pointer slot, and it
    /// segfaulted dereferencing it (finding 48).
    personalities: &'a HashMap<(u32, u32), HashSet<u64>>,
}

/// Patched content, plus the fixups dyld must apply at load time.
struct Patched {
    contents: SectionContents,
    binds: Vec<Bind>,
    /// Absolute pointers written into data, which dyld must slide.
    ///
    /// **Every** such pointer needs an entry, not just the GOT's. A
    /// position-independent image is loaded at a random offset, so an absolute
    /// address baked in at link time is stale the moment it loads. C programs
    /// hid this for a long time: their globals are reached PC-relatively, so
    /// they contain almost no absolute pointers. Rust's vtables, statics and
    /// panic metadata are full of them, and the result was a `SIGSEGV` inside
    /// `std::rt::lang_start_internal` — the first code to walk one.
    rebases: Vec<Rebase>,
    /// What each object read and produced, for the cache.
    records: Vec<ObjectRecord>,
    /// Objects whose bytes were **actually copied** from the cache.
    ///
    /// Counted here, at the copy, and not from the plan. A first version
    /// reported the plan's size, which is decided before the copy is
    /// attempted: when every copy failed and every object fell through to a
    /// full relocation, it still reported 47 of 47 reused. The counter added to
    /// make a dead cache visible had the dead cache's own failure mode, and
    /// only the relocate time — 10.1 ms against 4.7 — gave it away.
    reused: u64,
    /// Relocations skipped because their object's bytes were reused.
    reused_relocations: u64,
}

/// One object's trace through the relocation pass.
///
/// The fixups are ranges into `Patched`'s flat vectors rather than copies: the
/// link needs them flat to encode, and the cache needs them attributed, and
/// slicing serves both without duplicating either.
struct Patch {
    binds: Vec<Bind>,
    rebases: Vec<Rebase>,
    records: Vec<ObjectRecord>,
    reused: u64,
    reused_relocations: u64,
}

/// What one object read and produced, for the cache.
struct ObjectRecord {
    object: ObjectId,
    deps: std::sync::Arc<[blinker_cache::NameHash]>,
    binds: std::ops::Range<usize>,
    rebases: std::ops::Range<usize>,
}

/// Note that a relocation reads an address, without resolving it.
///
/// Both the symbol and *which table it is read from* are recorded: a symbol,
/// its GOT slot, its stub and its thread-local slot are four addresses that
/// move independently, and a GOT entry inserted ahead of this one shifts the
/// slot while leaving the symbol exactly where it was.
fn note_reference(
    referenced: &mut HashSet<(u32, u8)>,
    relocation: &blinker_macho::InputRelocation,
) {
    let RelocationTarget::Symbol(id) = relocation.target else {
        // A section target resolves within this object, so its address is
        // already covered by the entry's own ranges.
        return;
    };
    // The symbol's own address is read whenever the indirect one is not, and
    // which of the two applies depends on whether the symbol turned out to be
    // imported — known here only for some kinds. Recording both is correct and
    // costs one extra hash.
    referenced.insert((id.0, blinker_cache::Table::Symbol as u8));
    let indirect = if needs_got(relocation.kind) {
        blinker_cache::Table::Got
    } else if needs_tlv(relocation.kind) {
        blinker_cache::Table::ThreadLocal
    } else if relocation.kind == Arm64RelocationKind::Branch26 {
        blinker_cache::Table::Stub
    } else {
        return;
    };
    referenced.insert((id.0, indirect as u8));
}

/// Turn noted references into the hashes the cache compares.
///
/// The scope must mirror `AddressMap::lookup` exactly: a name this object
/// defines locally is a different address from a global of the same name, and
/// resolving one against the other is the bug finding 57 traced through the
/// `__eh_frame` LSDAs.
fn dependency_hashes(
    object: &LoadedObject,
    ids: &[SymbolNameId],
    digests: &[blinker_cache::NameHash],
    addresses: &AddressMap,
    referenced: &HashSet<(u32, u8)>,
) -> Vec<blinker_cache::NameHash> {
    let mut hashes: Vec<_> = referenced
        .iter()
        .filter_map(|(symbol, table)| {
            let name = *ids.get(*symbol as usize)?;
            let table = match table {
                1 => blinker_cache::Table::Got,
                2 => blinker_cache::Table::Stub,
                3 => blinker_cache::Table::ThreadLocal,
                _ => blinker_cache::Table::Symbol,
            };
            Some(blinker_cache::combine(
                digests[name.0 as usize],
                addresses.scope_of(object.parsed.id, name),
                table,
            ))
        })
        .collect();
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

/// Where each input section's bytes landed, by `(object, section)`.
///
/// The same question `Layout::address_of` answers, asked once per input
/// section instead of once per relocation.
///
/// `address_of` scans every output section, and within each one every
/// contribution. That is fine at 27 inputs and quadratic at 79: relocation
/// went from 3.1 ms to **187 ms** on a link five times the size, and the
/// linker that was 0.92x the system linker on a fixture was 7.4x on a real
/// binary (finding 77). Nothing was wrong with the lookup except how often it
/// was asked.
#[derive(Default)]
struct Placed {
    /// `(object, section)` -> the output section's index, and the address the
    /// chunk starts at.
    chunks: FastMap<(u32, u32), (usize, u64)>,
}

impl Placed {
    fn index(image: &Image) -> Placed {
        let mut chunks = FastMap::default();
        for (index, section) in image.layout.sections.iter().enumerate() {
            for contribution in &section.contributions {
                chunks.insert(
                    (contribution.object.0, contribution.section.0),
                    (index, section.vm_address + contribution.offset),
                );
            }
        }
        Placed { chunks }
    }

    /// The output section index and chunk address for one input section.
    fn chunk(&self, object: ObjectId, section: SectionId) -> Option<(usize, u64)> {
        self.chunks.get(&(object.0, section.0)).copied()
    }

    /// Just the address, which is what most callers want.
    fn address(&self, object: ObjectId, section: SectionId) -> Option<u64> {
        self.chunk(object, section).map(|(_, address)| address)
    }
}

/// Where the inputs ended up.
///
/// The three travel together because they answer parts of one question: the
/// address map says where a *name* went, the strip says where a *byte* went,
/// and `placed` says where a *section* went — and a relocation needs all three
/// to place its field and find its target.
struct Placement<'a> {
    addresses: &'a AddressMap,
    strip: &'a Strip,
    placed: &'a Placed,
}

/// The bytes one object is allowed to write, one slice per contribution.
///
/// # Why the buffers are cut up
///
/// A relocation only ever patches inside the contribution it belongs to: the
/// field's place is `chunk_offset + field`, where the chunk is that object's
/// contribution and the field is bounded by the input section's size. So the
/// objects write to disjoint bytes, and the pass could run on every core — but
/// only if the compiler can see that, and it cannot see it through one shared
/// `Vec<u8>` per output section.
///
/// Cutting each section buffer into its contributions, in offset order, turns
/// the property the layout already guarantees into one the borrow checker
/// holds. Nothing is copied; the slices are the same bytes.
struct ObjectBytes<'a> {
    /// Per contribution: its input section, which output section it landed in,
    /// where it starts there, and the bytes themselves.
    spans: Vec<(u32, usize, usize, &'a mut [u8])>,
}

impl ObjectBytes<'_> {
    /// This object's contribution from `section`, which is where a relocation
    /// against that section patches.
    ///
    /// A linear scan: an object has a couple of dozen sections, and this used
    /// to be a hash of the output section index into a map of every section in
    /// the image.
    fn contribution(&mut self, section: SectionId) -> Option<&mut [u8]> {
        self.spans
            .iter_mut()
            .find(|(input, ..)| *input == section.0)
            .map(|(.., bytes)| &mut **bytes)
    }

    /// The contribution covering `[start, start + len)` of output section
    /// `index`, for a cached copy that names output ranges rather than inputs.
    fn covering(&mut self, index: usize, start: usize, len: usize) -> Option<&mut [u8]> {
        let (_, _, base, bytes) = self.spans.iter_mut().find(|(_, section, base, bytes)| {
            *section == index && start >= *base && start + len <= *base + bytes.len()
        })?;
        let from = start - *base;
        bytes.get_mut(from..from + len)
    }
}

/// Cut every output section's buffer into its objects' contributions.
///
/// `None` if the contributions of some section overlap or run past its end,
/// which would make the slices unsound — the caller then keeps the buffers
/// whole and works sequentially. It has never happened; it is checked because
/// the alternative is trusting the layout to be an invariant it merely is.
fn carve<'a>(
    buffers: &'a mut [(usize, Vec<u8>)],
    image: &Image,
    slot_of: &HashMap<u32, usize>,
    objects: usize,
) -> Option<Vec<ObjectBytes<'a>>> {
    let mut per_object: Vec<ObjectBytes<'a>> = (0..objects)
        .map(|_| ObjectBytes { spans: Vec::new() })
        .collect();
    for (index, buffer) in buffers.iter_mut() {
        let section = image.layout.section(*index)?;
        let mut ordered: Vec<&blinker_layout::Contribution> = section
            .contributions
            .iter()
            .filter(|c| c.object != SYNTHETIC_OBJECT)
            .collect();
        ordered.sort_by_key(|c| c.offset);

        let total = buffer.len();
        let mut rest: &mut [u8] = buffer;
        let mut consumed = 0usize;
        for contribution in ordered {
            let start = contribution.offset as usize;
            let len = contribution.size as usize;
            // Out of order, overlapping, or off the end.
            if start < consumed || start + len > total {
                return None;
            }
            let (_, tail) = rest.split_at_mut(start - consumed);
            let (mine, tail) = tail.split_at_mut(len);
            rest = tail;
            consumed = start + len;
            let slot = *slot_of.get(&contribution.object.0)?;
            per_object[slot]
                .spans
                .push((contribution.section.0, *index, start, mine));
        }
    }
    Some(per_object)
}

// Eight, and each one is a distinct thing the pass consults. The two already
// bundled — `Placement` and `IndirectTables` — group parameters that belong
// together; grouping the rest would only be hiding the count.
#[allow(clippy::too_many_arguments)]
fn apply_relocations(
    objects: &[LoadedObject],
    names: &Names<'_>,
    image: &Image,
    placement: &Placement<'_>,
    tables: &IndirectTables<'_>,
    mut contents: SectionContents,
    // Whether to trace what each object read and produced.
    //
    // Off by default because it is not free: noting references and hashing
    // their names costs 1.9 ms on a 27.6 ms link — worth paying to *write* a
    // cache, and pure waste on a link that will not.
    record: bool,
    // Objects whose patched bytes a previous link already produced.
    reuse: Option<&ReusePlan<'_>>,
) -> Result<Patched, LinkError> {
    let mut extra_rebases: Vec<Rebase> = Vec::new();
    let Placement {
        addresses,
        strip,
        placed,
    } = *placement;
    let IndirectTables {
        got: got_slots,
        stubs: stub_slots,
        tlv: tlv_slots,
        imports,
        exports,
        personalities,
    } = *tables;

    // A reference from `__eh_frame` to a personality routine must name that
    // routine's GOT slot.
    // A relocation whose field is a CIE's indirect personality reference must
    // resolve to that symbol's GOT slot. Identified by *offset* — the CIE's
    // augmentation is the only thing that says which field this is, and it was
    // parsed before layout.
    let indirect_personality =
        |object: &LoadedObject, relocation: &blinker_macho::InputRelocation| {
            let fields = personalities.get(&(object.parsed.id.0, relocation.section.0))?;
            if !fields.contains(&relocation.offset) {
                return None;
            }
            let RelocationTarget::Symbol(id) = relocation.target else {
                return None;
            };
            let symbol = object.parsed.symbol(id)?;
            got_slots.get(&symbol.name).copied()
        };
    // Out of the map and into a vector so the buffers can be borrowed
    // independently of one another; `contents` is rebuilt from it at the end.
    let mut buffers: Vec<(usize, Vec<u8>)> = contents.drain().collect();
    buffers.sort_unstable_by_key(|(index, _)| *index);
    let slot_of: HashMap<u32, usize> = objects
        .iter()
        .enumerate()
        .map(|(slot, object)| (object.parsed.id.0, slot))
        .collect();
    let mut carved =
        carve(&mut buffers, image, &slot_of, objects.len()).ok_or(LinkError::NothingToLink)?;

    // One chunk of objects, relocated into its own slices and its own
    // accumulators. Everything it produces is chunk-local — including the
    // `binds`/`rebases` ranges each `ObjectRecord` carries — and is rebased
    // onto the whole link's vectors when the chunks are merged in order.
    let run_chunk = |base: usize, mine: &mut [ObjectBytes<'_>]| -> Result<Patch, LinkError> {
        let mut extra_binds: Vec<Bind> = Vec::new();
        let mut extra_rebases: Vec<Rebase> = Vec::new();
        let mut records: Vec<ObjectRecord> = Vec::new();
        let mut reused = 0u64;
        let mut reused_relocations = 0u64;
        for (at, bytes) in mine.iter_mut().enumerate() {
            let slot = base + at;
            let object = &objects[slot];
            let ids = &names.interned[slot];
            // Where this object's fixups start. Binds and rebases are produced as
            // a side effect of relocating, so an object whose bytes are later
            // reused from the cache must carry its own away with it — and the
            // cheapest way to attribute them is to remember where its run began
            // rather than to thread a second collection through every push site.
            let bind_start = extra_binds.len();
            let rebase_start = extra_rebases.len();
            // Addresses this object read, deduplicated by (symbol, table) so the
            // hashing below is proportional to distinct references rather than to
            // relocations — an object typically has several times more of the
            // latter.
            let mut referenced: HashSet<(u32, u8)> = HashSet::default();

            // The whole point of the cache: this object's bytes were relocated by
            // a previous link, nothing it reads has moved, and it has not moved
            // itself — so copy them and skip every relocation it holds.
            if let Some(entry) = reuse.and_then(|plan| plan.entry(object.parsed.id)) {
                let plan = reuse.expect("just matched");
                if copy_cached_bytes(entry, plan, bytes) {
                    reused += 1;
                    reused_relocations += object.parsed.relocations.len() as u64;
                    extra_binds.extend(entry.binds.iter().map(|bind| Bind {
                        segment: bind.segment,
                        offset: bind.offset,
                        symbol: bind.symbol.clone(),
                        library_ordinal: bind.library_ordinal,
                        addend: bind.addend,
                    }));
                    extra_rebases.extend(entry.rebases.iter().map(|rebase| Rebase {
                        segment: rebase.segment,
                        offset: rebase.offset,
                    }));
                    if record {
                        records.push(ObjectRecord {
                            object: object.parsed.id,
                            deps: entry.deps.clone(),
                            binds: bind_start..extra_binds.len(),
                            rebases: rebase_start..extra_rebases.len(),
                        });
                    }
                    continue;
                }
                // The cached bytes did not fit where they claimed to. Nothing is
                // wrong with the link, only with the cache, so fall through and
                // relocate this object as though there had been no entry at all.
            }

            // Indexed rather than iterated: `SUBTRACTOR` is one half of a pair and
            // needs the relocation that follows it, so the loop has to be able to
            // consume two entries at once.
            let relocations = &object.parsed.relocations;
            let mut index = 0;
            while index < relocations.len() {
                let relocation = &relocations[index];
                index += 1;

                // Where the patched field lives in the output.
                let Some((section_index, chunk_address)) =
                    placed.chunk(object.parsed.id, relocation.section).and_then(
                        |(index, address)| image.layout.section(index).map(|_| (index, address)),
                    )
                else {
                    // The relocation patches a section that was dropped as
                    // linker-internal; nothing in the output refers to it.
                    continue;
                };

                // Recorded before any branch, so no relocation kind can be added
                // later that reads an address without declaring it. Over-recording
                // only costs an unnecessary rebuild; under-recording reuses bytes
                // that are wrong.
                if record {
                    note_reference(&mut referenced, relocation);
                }

                let output_section = image.layout.section(section_index).expect("just matched");
                // Where the field moved to. `None` means the bytes holding it were
                // stripped, so there is nothing to patch — and, for a `SUBTRACTOR`,
                // its partner must be stepped over with it.
                let Some(field) =
                    strip.remap(object.parsed.id, relocation.section, relocation.offset)
                else {
                    if relocation.kind == Arm64RelocationKind::Subtractor {
                        index += 1;
                    }
                    continue;
                };
                let place = chunk_address + field;

                // `SUBTRACTOR` computes a *difference* between two addresses, so
                // it is meaningless alone: the pair is emitted as SUBTRACTOR (the
                // value being subtracted) immediately followed by UNSIGNED (the
                // value being subtracted from). Relative pointers in unwind and
                // exception tables are built this way, which is why Rust hits it
                // and simple C does not.
                if relocation.kind == Arm64RelocationKind::Subtractor {
                    let Some(pair) = relocations.get(index) else {
                        return Err(LinkError::UnpairedSubtractor {
                            object: object.parsed.id,
                            offset: relocation.offset,
                        });
                    };
                    index += 1;

                    if record {
                        note_reference(&mut referenced, pair);
                    }
                    let subtrahend =
                        target_address(object, ids, placed, addresses, relocation.target)?;
                    let minuend = match indirect_personality(object, pair) {
                        Some(slot) => slot,
                        None => target_address(object, ids, placed, addresses, pair.target)?,
                    };

                    // Mach-O relocations carry their addend **in the bytes being
                    // patched**, not in the relocation entry — `addend` is zero on
                    // every one of them. For a pair that difference is not a small
                    // correction: the subtrahend is the section's own anchor label
                    // (`ltmpN`), so `minuend - subtrahend` is measured from the
                    // start of the contribution, while the field wants it measured
                    // from the field. The inline value is exactly that gap.
                    //
                    // Which makes the stored gap wrong the moment stripping moves
                    // either end of it, so it is re-measured against where the two
                    // ended up.
                    let anchor = anchor_offset(object, relocation);
                    let correction = anchor
                        .map(|anchor| {
                            strip.pair_correction(
                                object.parsed.id,
                                relocation.section,
                                pair.offset,
                                anchor,
                            )
                        })
                        .unwrap_or(0);
                    let addend = pair.addend + inline_addend(object, pair) + correction;

                    let Some(pair_field) =
                        strip.remap(object.parsed.id, relocation.section, pair.offset)
                    else {
                        continue;
                    };
                    let Some(buffer) = bytes.contribution(relocation.section) else {
                        continue;
                    };
                    blinker_relocations::apply_pair(
                        pair.length,
                        pair_field,
                        subtrahend,
                        minuend,
                        addend,
                        place,
                        buffer,
                    )
                    .map_err(|source| LinkError::Relocation {
                        object: object.parsed.id,
                        kind: relocation.kind,
                        source: Box::new(source),
                    })?;
                    continue;
                }

                // GOT-based kinds are patched with the address of the *slot*, not
                // of the symbol; the symbol's address is what the slot contains.
                let got = if needs_got(relocation.kind) {
                    match relocation.target {
                        RelocationTarget::Symbol(id) => object
                            .parsed
                            .symbol(id)
                            .and_then(|s| got_slots.get(&s.name))
                            .copied(),
                        RelocationTarget::Section(_) => None,
                    }
                } else {
                    None
                };

                let tlv = if needs_tlv(relocation.kind) {
                    match relocation.target {
                        RelocationTarget::Symbol(id) => object
                            .parsed
                            .symbol(id)
                            .and_then(|s| tlv_slots.get(&s.name))
                            .copied(),
                        RelocationTarget::Section(_) => None,
                    }
                } else {
                    None
                };

                // A branch to an imported function goes to its stub: dyld has not
                // filled anything in yet, and a BRANCH26 cannot reach an address
                // that does not exist until load time.
                let stub = if relocation.kind == Arm64RelocationKind::Branch26 {
                    match relocation.target {
                        RelocationTarget::Symbol(id) => object
                            .parsed
                            .symbol(id)
                            .and_then(|s| stub_slots.get(&s.name))
                            .copied(),
                        RelocationTarget::Section(_) => None,
                    }
                } else {
                    None
                };

                // A pointer-sized data reference to an imported symbol cannot be
                // patched at all: the address does not exist until dyld supplies
                // it. The field stays zero and a bind entry tells dyld where to
                // write. TLV descriptors are built this way — their first word is
                // a pointer to `__tlv_bootstrap`, which lives in libdyld.
                if relocation.kind == Arm64RelocationKind::Unsigned {
                    if let RelocationTarget::Symbol(id) = relocation.target {
                        if let Some(symbol) = object.parsed.symbol(id) {
                            if imports.contains(&symbol.name) {
                                if let Some((segment_index, segment)) = image
                                    .layout
                                    .segments
                                    .iter()
                                    .enumerate()
                                    .find(|(_, seg)| seg.name == output_section.segment)
                                {
                                    extra_binds.push(Bind {
                                        segment: segment_index as u8,
                                        offset: place - segment.vm_address,
                                        symbol: symbol.name.clone(),
                                        library_ordinal: exports
                                            .map(|e| e.ordinal(&symbol.name))
                                            .unwrap_or(1),
                                        addend: relocation.addend,
                                    });
                                }
                                continue;
                            }
                        }
                    }
                }

                // A descriptor's pointer to its variable is stored as an offset
                // into the per-thread block rather than as an address.
                let thread_local_offset = if relocation.kind == Arm64RelocationKind::Unsigned
                    && output_section.name == "__thread_vars"
                {
                    thread_local_base(image)
                } else {
                    None
                };

                let target = match (stub, got.or(tlv)) {
                    (Some(address), _) => address,
                    // A GOT-based reference to an *imported* symbol has no address
                    // of its own — that is the point of importing it. The
                    // instruction is patched from the slot's address, and `target`
                    // is unused for these kinds, so a failed lookup here is
                    // expected rather than an error.
                    (None, Some(_)) => {
                        target_address(object, ids, placed, addresses, relocation.target)
                            .unwrap_or(0)
                    }
                    (None, None) => {
                        target_address(object, ids, placed, addresses, relocation.target)?
                    }
                };

                let Some(buffer) = bytes.contribution(relocation.section) else {
                    continue; // zero-filled section: nothing to patch
                };

                // Rewrite an address into a block-relative offset where the
                // descriptor expects one. Only targets that actually land in the
                // block are converted: a descriptor also holds a bound thunk
                // pointer and a key, and those are not offsets.
                let target = match thread_local_offset {
                    Some(base) if in_thread_local_block(image, target) => target - base,
                    _ => target,
                };

                // An absolute pointer stored in data has to be slid by dyld.
                // Excluded: fields in read-only segments (nothing writes them),
                // and thread-local descriptor offsets, which are offsets rather
                // than addresses and must not move.
                if relocation.kind == Arm64RelocationKind::Unsigned
                    && relocation.length == blinker_macho::RelocationLength::Long
                    && thread_local_offset.is_none()
                    && output_section.segment != "__TEXT"
                {
                    if let Some((segment_index, segment)) = image
                        .layout
                        .segments
                        .iter()
                        .enumerate()
                        .find(|(_, seg)| seg.name == output_section.segment)
                    {
                        extra_rebases.push(Rebase {
                            segment: segment_index as u8,
                            offset: place - segment.vm_address,
                        });
                    }
                }

                // Relative to this object's contribution, which is what `buffer`
                // now is — the field was `chunk_offset + field` into the whole
                // output section, and the chunk is where the slice begins.
                let field_offset = field;
                apply(
                    relocation.kind,
                    relocation.length,
                    field_offset,
                    Context {
                        place,
                        target,
                        addend: relocation.addend,
                        got,
                        tlv,
                        pc_relative: relocation.pc_relative,
                    },
                    buffer,
                )
                .map_err(|source| LinkError::Relocation {
                    object: object.parsed.id,
                    kind: relocation.kind,
                    source: Box::new(source),
                })?;
            }

            if !record {
                continue;
            }
            records.push(ObjectRecord {
                object: object.parsed.id,
                deps: dependency_hashes(
                    object,
                    &names.interned[slot],
                    names.digests,
                    addresses,
                    &referenced,
                )
                .into(),
                binds: bind_start..extra_binds.len(),
                rebases: rebase_start..extra_rebases.len(),
            });
        }
        Ok(Patch {
            binds: extra_binds,
            rebases: extra_rebases,
            records,
            reused,
            reused_relocations,
        })
    };

    // Handed out a chunk at a time rather than split evenly: an object whose
    // bytes came from the cache costs almost nothing and one that is relocated
    // costs everything, and on an edit the expensive ones are the objects of
    // the crate that changed — which are consecutive. A static split would put
    // all of them on one thread.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let size = carved.len().div_ceil(threads * 4).max(1);
    let pieces: Vec<(usize, &mut [ObjectBytes<'_>])> = carved
        .chunks_mut(size)
        .enumerate()
        .map(|(index, slice)| (index * size, slice))
        .collect();
    let queue = std::sync::Mutex::new(pieces.into_iter());
    let (queue, run_chunk) = (&queue, &run_chunk);
    let claimed: Vec<Vec<(usize, Result<Patch, LinkError>)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        // The lock is held for the length of a `next`, once
                        // per chunk — not once per object, and never while any
                        // relocation is applied.
                        let next = queue.lock().expect("the queue is not poisoned").next();
                        let Some((base, slice)) = next else {
                            return mine;
                        };
                        mine.push((base, run_chunk(base, slice)));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a relocation worker panicked"))
            .collect()
    });

    // Back into object order, and only then merged: a record's `binds` range
    // means nothing except against the vector it is appended to.
    let mut patches: Vec<(usize, Result<Patch, LinkError>)> =
        claimed.into_iter().flatten().collect();
    patches.sort_by_key(|(base, _)| *base);
    let mut extra_binds: Vec<Bind> = Vec::new();
    let mut records: Vec<ObjectRecord> = Vec::new();
    let mut reused = 0u64;
    let mut reused_relocations = 0u64;
    for (_, patch) in patches {
        let patch = patch?;
        let bind_base = extra_binds.len();
        let rebase_base = extra_rebases.len();
        for record in patch.records {
            records.push(ObjectRecord {
                binds: record.binds.start + bind_base..record.binds.end + bind_base,
                rebases: record.rebases.start + rebase_base..record.rebases.end + rebase_base,
                ..record
            });
        }
        extra_binds.extend(patch.binds);
        extra_rebases.extend(patch.rebases);
        reused += patch.reused;
        reused_relocations += patch.reused_relocations;
    }
    // The slices go out of scope here, which is what releases the buffers.
    drop(carved);
    contents.extend(buffers);
    Ok(Patched {
        contents,
        binds: extra_binds,
        rebases: extra_rebases,
        records,
        reused,
        reused_relocations,
    })
}

/// Where a `SUBTRACTOR`'s subtrahend sits inside the field's own section.
///
/// `None` when it is somewhere else — a function start, say, which stripping
/// moves as a whole and whose distance to the field is not a layout fact.
fn anchor_offset(object: &LoadedObject, relocation: &InputRelocation) -> Option<u64> {
    let RelocationTarget::Symbol(id) = relocation.target else {
        return None;
    };
    let symbol = object.parsed.symbol(id)?;
    if symbol.section != Some(relocation.section) {
        return None;
    }
    let section = object.parsed.section(relocation.section)?;
    Some(symbol.value.saturating_sub(section.vm_address))
}

/// The addend a Mach-O relocation stores in the bytes it patches.
///
/// Mach-O has no addend field: `InputRelocation::addend` is zero on every
/// relocation an object file actually contains, and the value is written into
/// the patch site instead. Read from the *input* bytes rather than from the
/// output buffer being assembled, so applying a relocation twice cannot
/// accumulate.
///
/// Sign-extended from the relocation's own width — these are signed
/// displacements, and a 4-byte field holding `-125` is `0xffffff83`, not
/// four billion.
fn inline_addend(object: &LoadedObject, relocation: &InputRelocation) -> i64 {
    let Some(file_offset) = object
        .parsed
        .section(relocation.section)
        .and_then(|s| s.file_offset)
    else {
        return 0;
    };
    let at = (file_offset + relocation.offset) as usize;
    match relocation.length {
        RelocationLength::Long => object
            .data
            .get(at..at + 8)
            .map(|b| i64::from_le_bytes(b.try_into().expect("8 bytes"))),
        RelocationLength::Word => object
            .data
            .get(at..at + 4)
            .map(|b| i32::from_le_bytes(b.try_into().expect("4 bytes")) as i64),
        RelocationLength::Half => object
            .data
            .get(at..at + 2)
            .map(|b| i16::from_le_bytes(b.try_into().expect("2 bytes")) as i64),
        RelocationLength::Byte => object.data.get(at).map(|b| *b as i8 as i64),
    }
    .unwrap_or(0)
}

/// The output address a relocation refers to.
fn target_address(
    object: &LoadedObject,
    ids: &[SymbolNameId],
    placed: &Placed,
    addresses: &AddressMap,
    target: RelocationTarget,
) -> Result<u64, LinkError> {
    match target {
        RelocationTarget::Section(section) => {
            placed
                .address(object.parsed.id, section)
                .ok_or(LinkError::MissingSection {
                    object: object.parsed.id,
                    section,
                })
        }
        RelocationTarget::Symbol(symbol_id) => {
            let symbol = object
                .parsed
                .symbol(symbol_id)
                .ok_or(LinkError::MissingSymbol { symbol: symbol_id })?;
            let name = *ids
                .get(symbol_id.0 as usize)
                .ok_or(LinkError::MissingSymbol { symbol: symbol_id })?;
            addresses
                .lookup(object.parsed.id, name)
                .ok_or(LinkError::UndefinedSymbols {
                    // The text only on the failing path, where the cost of
                    // finding it is beside the point.
                    names: vec![symbol.name.clone()],
                })
        }
    }
}

/// Where every name in the link resolved to, by interned id.
///
/// # Why the key is an id and not the name
///
/// This is the hottest lookup in the linker: once per relocation in `apply`,
/// and once per `__eh_frame` FDE while finding where each function's unwind
/// record landed. Keyed by text it was two hashes of a mangled Rust name and a
/// `memcmp` per question — 93 ms for the 265,308 FDEs alone, which was most of
/// the unwind stage (finding 160). The asker always holds the symbol, and so
/// its id.
///
/// Where every defined symbol ended up in the output image.
///
/// Built once, across *all* objects. The first version of this searched only
/// the object holding the relocation, which works for locals and for
/// self-contained files, and fails the moment one object calls into another —
/// the definition is elsewhere, so the lookup reported the symbol undefined
/// even though resolution had already found it. Two coordinate systems that
/// happen to coincide in the single-object case is exactly the seam an
/// isolated test cannot see.
///
/// Locals are keyed per object because two objects may legitimately define the
/// same local name; globals are keyed by name alone.
/// The names are borrowed from the parsed objects, which outlive this. Every
/// definition in the link used to go through `name.clone()` to build a map
/// thrown away at the end of the same link — around two hundred thousand heap
/// allocations, for strings that already existed a pointer away.
#[derive(Default)]
struct AddressMap {
    global: HashMap<SymbolNameId, u64>,
    /// Locals, by object and then by name.
    ///
    /// Nested rather than keyed by `(u32, SymbolNameId)` because a local's
    /// object is the same for every symbol of it, so the outer lookup is
    /// hoisted out of the loops that build and read this.
    local: HashMap<u32, HashMap<SymbolNameId, u64>>,
}

/// Every name in the link, as ids and as digests.
///
/// The two always travel together — an id is how you find a digest — and
/// bundling them keeps the relocation pass's signature to the things it
/// relocates.
struct Names<'a> {
    /// Per object, its symbols' interned names, indexed by `SymbolId`.
    interned: &'a [Arc<Vec<SymbolNameId>>],
    /// Per interned name, `blake3(name)`. See `Session::digests`.
    digests: &'a [blinker_cache::NameHash],
}

impl AddressMap {
    /// The scope in which `lookup` would find `name` from `object`.
    ///
    /// Paired with `lookup` so the cache hashes the address the linker would
    /// actually have read, rather than one that merely shares its name.
    fn scope_of(&self, object: ObjectId, name: SymbolNameId) -> u32 {
        if self
            .local
            .get(&object.0)
            .is_some_and(|names| names.contains_key(&name))
        {
            object.0
        } else {
            blinker_cache::GLOBAL
        }
    }

    fn lookup(&self, object: ObjectId, name: SymbolNameId) -> Option<u64> {
        // A local definition in this object shadows a global of the same name,
        // which is what "local" means.
        self.local
            .get(&object.0)
            .and_then(|names| names.get(&name))
            .or_else(|| self.global.get(&name))
            .copied()
    }
}

/// Compute the output address of every definition.
fn address_map(
    objects: &[LoadedObject],
    interned: &[Arc<Vec<SymbolNameId>>],
    placed: &Placed,
    strip: &Strip,
) -> AddressMap {
    let mut map = AddressMap::default();
    // Sized once rather than grown into: the global map ends up holding every
    // non-local definition in the link, and growing there from empty rehashes
    // everything already inserted, once per doubling (finding 135).
    map.global.reserve(
        objects
            .iter()
            .flat_map(|o| o.parsed.symbols.iter())
            .filter(|s| s.strength.is_definition() && s.visibility != SymbolVisibility::Local)
            .count(),
    );

    // Per chunk on every core, merged in chunk order. Order matters for the
    // globals: two objects defining one name is legal — a weak definition and
    // its winner — and sequentially the later one overwrote, so it must here.
    // Locals are keyed by object, so their keys are disjoint by construction.
    let chunks = crate::parallel::map_chunks(objects, |base, chunk| {
        address_map_of(chunk, base, interned, placed, strip)
    });
    for chunk in chunks {
        map.global.extend(chunk.global);
        for (object, locals) in chunk.local {
            map.local.entry(object).or_default().extend(locals);
        }
    }
    map
}

/// One chunk's addresses. `base` is where the chunk starts in `objects`.
fn address_map_of(
    objects: &[LoadedObject],
    base: usize,
    interned: &[Arc<Vec<SymbolNameId>>],
    placed: &Placed,
    strip: &Strip,
) -> AddressMap {
    let mut map = AddressMap::default();
    // Split so the two maps can be borrowed independently below.
    let AddressMap { global, local } = &mut map;
    for (at, object) in objects.iter().enumerate() {
        let ids = &interned[base + at];
        // Hoisted: the object is the same for every symbol below, so finding
        // its sub-map inside the loop is one hash per local definition to
        // answer a question that changes once per object.
        let locals = local.entry(object.parsed.id.0).or_default();
        for symbol in &object.parsed.symbols {
            if !symbol.strength.is_definition() {
                continue;
            }
            let Some(section_id) = symbol.section else {
                continue;
            };
            let Some(input) = object.parsed.section(section_id) else {
                continue;
            };
            let Some(chunk) = placed.address(object.parsed.id, section_id) else {
                // Its section was dropped as linker-internal.
                continue;
            };
            // The symbol's value is an address in the object's own coordinate
            // space, so the offset within its section has to be recovered
            // first. Using the value directly would be right only when the
            // section begins at zero.
            let offset = symbol.value.saturating_sub(input.vm_address);
            // A symbol whose bytes were stripped has no address at all, and
            // must not get one: an entry here would let a relocation resolve
            // to bytes that are no longer in the image.
            let Some(offset) = strip.remap(object.parsed.id, section_id, offset) else {
                continue;
            };
            let address = chunk + offset;

            let Some(name) = ids.get(symbol.id.0 as usize).copied() else {
                continue;
            };
            if symbol.visibility == SymbolVisibility::Local {
                locals.insert(name, address);
            } else {
                global.insert(name, address);
            }
        }
    }
    map
}

/// `LC_MAIN`'s entry offset: a **file** offset, not an address.
fn entry_offset(
    request: &LinkRequest,
    objects: &[LoadedObject],
    image: &Image,
    strip: &Strip,
) -> Result<u64, LinkError> {
    for object in objects {
        let Some(symbol) = object
            .parsed
            .symbols
            .iter()
            .find(|s| s.name == request.entry_symbol && s.strength.is_definition())
        else {
            continue;
        };
        let Some(section_id) = symbol.section else {
            continue;
        };
        let input = object
            .parsed
            .section(section_id)
            .ok_or(LinkError::MissingSection {
                object: object.parsed.id,
                section: section_id,
            })?;
        // The entry point is a root, so its bytes are always kept; a `None`
        // here would mean the root set and the strip disagree.
        let Some(offset_in_section) = strip.remap(
            object.parsed.id,
            section_id,
            symbol.value.saturating_sub(input.vm_address),
        ) else {
            continue;
        };

        for section in &image.layout.sections {
            if let Some(address) = section.address_of(object.parsed.id, section_id) {
                let Some(file_offset) = section.file_offset else {
                    continue;
                };
                let chunk_offset = address - section.vm_address;
                return Ok(file_offset + chunk_offset + offset_in_section);
            }
        }
    }
    Err(LinkError::NoEntryPoint {
        symbol: request.entry_symbol.clone(),
    })
}

/// Convenience: link and write the result.
pub fn link_to_file(request: &LinkRequest, output: &Path) -> Result<Image, LinkError> {
    let (image, timings) = link_timed(request)?;
    write_output(&image.bytes, output)?;
    let _ = timings;
    Ok(image)
}

/// Link and write, reusing a finished binary outright when nothing changed.
///
/// The fast path is the one case where the cache can be certain without doing
/// any of the work: every input file unchanged and the request identical means
/// the output is the one already on disk. Proving that costs 0.18 ms on a
/// 56-input Rust link — 0.16 to hash rustc's own objects, 0.024 to stat the
/// toolchain rlibs — against 22.6 ms to link (finding 67).
///
/// It returns timings rather than an [`Image`], because an `Image` carries the
/// layout and symbol table, and reconstructing those is most of the work being
/// skipped. Callers that need them should use [`link_to_file`], which always
/// performs a full link.
pub fn link_to_file_timed(request: &LinkRequest, output: &Path) -> Result<LinkTimings, LinkError> {
    link_to_file_in(request, output, &mut Session::default())
}

/// Link and write, keeping parsed inputs in `session` for the next call.
///
/// The one-shot entry points create a `Session` and drop it, which is exactly
/// the previous behaviour. A resident linker keeps one and hands it to every
/// link, which is the whole of what "resident" buys: an input whose bytes have
/// not changed is not read, not parsed, and not turned into symbols again.
pub fn link_to_file_in(
    request: &LinkRequest,
    output: &Path,
    session: &mut Session,
) -> Result<LinkTimings, LinkError> {
    if let Some(timings) = reuse_finished_image(request, output, session)? {
        return Ok(timings);
    }
    let mut timings = LinkTimings::default();
    let overall = std::time::Instant::now();
    let image = link_inner(request, &mut timings, session)?;
    timings.total_ms = elapsed_ms(overall);
    let (held, read) = session.counts();
    timings.inputs_held = held as u64;
    timings.inputs_read = read as u64;
    let (replayed, resolution) = session.reused();
    timings.replayed_extraction = replayed;
    timings.held_resolution = resolution;
    let (changes, first) = session.interface_changes();
    timings.interface_changes = changes;
    timings.first_interface_change = first.map(Path::to_path_buf);
    write_output(&image.bytes, output)?;
    Ok(timings)
}

/// Everything about a request that is not an input file.
///
/// Identical objects linked with a different entry point are a different
/// binary, and the input keys alone would not say so.
fn request_hash(request: &LinkRequest) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(request.entry_symbol.as_bytes());
    hasher.update(&[0]);
    hasher.update(request.identifier.as_bytes());
    hasher.update(&[request.dead_strip as u8, request.stable_layout as u8]);
    // The linker itself is an input to its own output.
    //
    // Without this, changing blinker and relinking replays the binary the
    // *previous* build produced: the inputs are unchanged and the request is
    // unchanged, so the whole-image fast path fires and hands back a stale
    // image. It cost an hour here — a fix was measured as broken because the
    // cache was serving output from the build before it — and in a release it
    // would mean upgrading the linker silently changes nothing.
    if let Ok(exe) = std::env::current_exe() {
        hasher.update(exe.to_string_lossy().as_bytes());
        if let Ok(meta) = std::fs::metadata(&exe) {
            hasher.update(&meta.len().to_le_bytes());
            if let Ok(modified) = meta.modified() {
                if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                    hasher.update(&since.as_nanos().to_le_bytes());
                }
            }
        }
    }
    for dylib in &request.dylibs {
        hasher.update(&[1]);
        hasher.update(dylib.install_name.as_bytes());
    }
    for stub in &request.stub_libraries {
        hasher.update(&[2]);
        hasher.update(stub.to_string_lossy().as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// The input files and the keys that prove them unchanged.
fn input_keys(request: &LinkRequest) -> Option<Vec<(PathBuf, blinker_cache::InputKey)>> {
    request
        .objects
        .iter()
        .map(|path| blinker_cache::InputKey::probe(path).map(|key| (path.clone(), key)))
        .collect()
}

/// Write the cached binary if it is provably the one this link would produce.
fn reuse_finished_image(
    request: &LinkRequest,
    output: &Path,
    session: &Session,
) -> Result<Option<LinkTimings>, LinkError> {
    let overall = std::time::Instant::now();
    let Some(path) = request.cache_path.as_deref() else {
        return Ok(None);
    };
    // The session's copy first, and not only to save a read. Once a session
    // holds a cache, the file on disk is the one *its first* link wrote, and a
    // link that reuses a previous layout does not produce the same bytes as one
    // that had none (D5). Replaying the file would hand back an image two links
    // old and then the next link would produce the current one again — an
    // output that alternates between two valid binaries.
    let held = session.cache_for(path).map(|(_, cache)| cache);
    let loaded;
    let cache = match held {
        Some(cache) => cache,
        None => {
            loaded = blinker_cache::load(path);
            match &loaded {
                Some(cache) => cache,
                None => return Ok(None),
            }
        }
    };
    let Some(inputs) = input_keys(request) else {
        // An input that cannot be examined is one that may have moved; the
        // full link will produce the real error.
        return Ok(None);
    };
    if !cache.matches(&inputs, &request_hash(request)) {
        return Ok(None);
    }

    write_output(&cache.image, output)?;
    let relocations = cache.entries.iter().map(|e| e.deps.len() as u64).sum();
    Ok(Some(LinkTimings {
        total_ms: elapsed_ms(overall),
        reused_objects: cache.entries.len() as u64,
        total_objects: cache.entries.len() as u64,
        // Nothing was relocated because nothing was linked. Reported as a
        // complete hit so the number means the same thing on both paths.
        reused_relocations: relocations,
        total_relocations: relocations,
        reused_finished_image: true,
        ..LinkTimings::default()
    }))
}

/// Write the finished image, replacing the previous one only once it is whole.
///
/// Writing straight to the output path truncates it before the first byte
/// lands, so a link killed partway — `^C`, a full disk, a panic — replaces a
/// working executable with a fragment. The spec calls for the opposite: a
/// failed or cancelled link must leave the previous output intact.
///
/// So the bytes go to a temporary file beside the target and are renamed over
/// it, which POSIX guarantees is atomic within a filesystem. Beside it, rather
/// than in `/tmp`, because a rename across filesystems is not a rename — it is
/// a copy, and the guarantee is lost exactly where the output is large.
fn write_output(bytes: &[u8], output: &Path) -> Result<(), LinkError> {
    let failed = |source: std::io::Error| LinkError::Write {
        path: output.to_path_buf(),
        source,
    };
    // The pid keeps concurrent links to different outputs in one directory
    // from colliding, and the file name keeps two links to the *same*
    // directory from sharing a temporary.
    let name = output.file_name().unwrap_or_else(|| OsStr::new("a.out"));
    let mut temporary = name.to_os_string();
    temporary.push(format!(".blinker-{}.tmp", std::process::id()));
    let temporary = output.with_file_name(temporary);

    let write = || -> std::io::Result<()> {
        std::fs::write(&temporary, bytes)?;
        // Set on the temporary, so the file is never visible at the output
        // path without its permissions.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&temporary, output)
    };

    write().map_err(|source| {
        // A failure leaves the previous output untouched; the temporary is the
        // only casualty and must not be left behind.
        let _ = std::fs::remove_file(&temporary);
        failed(source)
    })
}

#[cfg(test)]
mod defining_index_tests {
    use super::{DefiningIndex, Definition};
    use blinker_archive::MemberId;

    fn definition(name: &str, archive: usize, member: u32) -> Definition<'_> {
        Definition {
            name,
            archive,
            member: MemberId(member),
        }
    }

    /// The ordinary case: the earliest archive defining a name wins, and a
    /// name nothing defines is absent.
    #[test]
    fn the_first_archive_defining_a_name_wins() {
        let entries = [
            (1, definition("_malloc", 0, 7)),
            (1, definition("_malloc", 3, 2)),
            (2, definition("_free", 1, 4)),
        ];
        let index = DefiningIndex::from_hashed(entries.into_iter(), 3);
        // Looked up through the real `get`, which hashes the text — so these
        // go through the collision path, since the synthetic hashes above are
        // not what `hash_of` produces. Checked against the fields instead.
        assert_eq!(index.first.len(), 2);
        assert_eq!(index.collided.len(), 0);
        assert_eq!(index.first[&1].archive, 0);
        assert_eq!(index.first[&1].member, MemberId(7));
        assert_eq!(index.first[&2].archive, 1);
    }

    /// Two distinct names hashing to one `u64`, which is the case a table
    /// keyed by a hash exists to survive.
    ///
    /// Unreachable through `build`: producing a 64-bit collision by choosing
    /// symbol names is not something a test can do. So the hashes are supplied
    /// directly, and what is checked is that each name still resolves to its
    /// own definition — the failure being an archive member extracted for a
    /// symbol it does not define.
    #[test]
    fn two_names_that_hash_alike_keep_their_own_definitions() {
        const COLLIDING: u64 = 0x5ca1_ab1e;
        let entries = [
            (COLLIDING, definition("_first", 0, 1)),
            (COLLIDING, definition("_second", 2, 5)),
            // The earlier name again, from a later archive: still first.
            (COLLIDING, definition("_first", 4, 9)),
            // And the later one again, which must not displace its own first.
            (COLLIDING, definition("_second", 6, 6)),
        ];
        let index = DefiningIndex::from_hashed(entries.into_iter(), 4);
        assert_eq!(index.first[&COLLIDING].name, "_first");
        assert_eq!(index.collided.len(), 2, "both sightings of the later name");

        assert_eq!(index.at(COLLIDING, "_first"), Some((0, MemberId(1))));
        assert_eq!(index.at(COLLIDING, "_second"), Some((2, MemberId(5))));
        assert_eq!(index.at(COLLIDING, "_absent"), None);
    }
}
