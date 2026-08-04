//! blinker's driver: one invocation, start to finish.
//!
//! At M0 the pipeline is deliberately short — record, then delegate:
//!
//! ```text
//! split project options
//!   → expand response files
//!   → classify arguments
//!   → fingerprint inputs
//!   → record JSON
//!   → delegate to the fallback linker
//!   → propagate its exit status
//! ```
//!
//! Later milestones insert an internal link between "classify" and "delegate";
//! the surrounding structure — argument handling, recording, and the guarantee
//! that a delegated link behaves exactly as if blinker were not installed — is
//! established here and does not change.

use std::path::{Path, PathBuf};
use std::time::Instant;

use blinker_arguments::{expand_response_files, LinkerArg, ParsedInvocation};
use blinker_diagnostics::{fingerprint_input, LinkRecord};

pub mod archive;
pub mod daemon;
pub mod fallback;
pub mod options;

pub use options::{split_args, ProjectOptions, Verbosity, HELP};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub enum DriverError {
    Options(options::OptionError),
    ResponseFile(blinker_arguments::ResponseFileError),
    Fallback(fallback::FallbackError),
    Io(std::io::Error),
    /// The internal link failed.
    Link {
        detail: String,
    },
    /// A recorded invocation could not be read back.
    Replay {
        path: PathBuf,
        detail: String,
    },
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::Options(e) => write!(f, "{e}"),
            DriverError::ResponseFile(e) => write!(f, "{e}"),
            DriverError::Fallback(e) => write!(f, "{e}"),
            DriverError::Io(e) => write!(f, "{e}"),
            DriverError::Link { detail } => write!(f, "{detail}"),
            DriverError::Replay { path, detail } => {
                write!(f, "cannot replay {}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for DriverError {}

impl From<options::OptionError> for DriverError {
    fn from(e: options::OptionError) -> Self {
        DriverError::Options(e)
    }
}
impl From<blinker_arguments::ResponseFileError> for DriverError {
    fn from(e: blinker_arguments::ResponseFileError) -> Self {
        DriverError::ResponseFile(e)
    }
}
impl From<fallback::FallbackError> for DriverError {
    fn from(e: fallback::FallbackError) -> Self {
        DriverError::Fallback(e)
    }
}
impl From<std::io::Error> for DriverError {
    fn from(e: std::io::Error) -> Self {
        DriverError::Io(e)
    }
}

/// What one invocation produced.
pub struct Outcome {
    /// Exit code to propagate. This is the fallback linker's own code, never a
    /// substitute.
    pub exit_code: i32,
    pub record: LinkRecord,
}

/// Run one invocation to completion.
///
/// `argv` excludes the program name.
pub fn run(argv: &[String]) -> Result<Outcome, DriverError> {
    run_in(argv, &mut blinker_link::Session::default())
}

/// Run one invocation, keeping parsed inputs in `session` for the next.
///
/// The daemon holds one session and hands it to every request; a one-shot
/// invocation creates one and drops it, which is exactly what happened before
/// this existed. Everything between here and the link is identical either way,
/// which is the property that keeps a daemon from being a second linker: the
/// argument vector, the classification and the fallback decision are all the
/// same code.
pub fn run_in(
    argv: &[String],
    session: &mut blinker_link::Session,
) -> Result<Outcome, DriverError> {
    let started = Instant::now();

    let parse_started = Instant::now();
    let split = split_args(argv)?;
    let options = split.options;

    // Replaying substitutes a recorded argument vector for the live one. The
    // rest of the pipeline is identical, which is what makes a replay a
    // faithful reproduction rather than an approximation.
    //
    // `replay_output` keeps the redirected artifact alive for the duration of
    // the run; dropping it early would delete the directory we just linked into.
    let mut replay_output = None;
    let linker_argv = match &options.replay_invocation {
        Some(path) => {
            let (argv, scratch) = prepare_replay(path)?;
            replay_output = Some(scratch);
            argv
        }
        None => split.linker_argv,
    };

    let expanded = expand_response_files(&linker_argv)?;
    let parsed = ParsedInvocation::parse(expanded);
    let parse_elapsed = parse_started.elapsed();

    let mut record = LinkRecord::delegated();
    record.set_timing_argument_parsing(parse_elapsed);
    record.argv = parsed.argv.clone();
    record.output_path = parsed.output_path().map(Path::to_path_buf);
    record.arch = parsed.arch().map(str::to_string);
    record.deployment_target = parsed
        .deployment_target()
        .map(|(platform, version)| format!("{platform}{version}"));
    record.unrecognized_arguments = parsed
        .unrecognized()
        .into_iter()
        .map(str::to_string)
        .collect();

    let fingerprint_started = Instant::now();
    let mut inputs = fingerprint_input(&parsed.input_paths(), options.strict_fingerprints);
    record.set_timing_fingerprinting(fingerprint_started.elapsed());

    // Archive before delegating: rustc deletes the temporary directory holding
    // the object files as soon as the link returns, so afterwards is too late.
    if let Some(dir) = &options.record_invocation {
        let archive_dir = recording_path(dir, &record).with_extension("inputs");
        let mut mapping = archive::archive_inputs(&mut inputs, &archive_dir)?;
        // The objects are not everything the link reads; see `FILE_VALUED_FLAGS`.
        mapping.extend(archive::archive_side_files(&parsed.argv, &archive_dir)?);
        record.replay_argv = Some(archive::rewrite_argv(&parsed.argv, &mapping));
    }
    record.set_inputs(inputs);

    // Unknown arguments are always surfaced — they are the M0 deliverable that
    // tells us what the corpus contains that we have not modelled.
    if !record.unrecognized_arguments.is_empty() && options.verbosity != Verbosity::Quiet {
        eprintln!(
            "blinker: {} unrecognized argument(s): {}",
            record.unrecognized_arguments.len(),
            record.unrecognized_arguments.join(" ")
        );
    }

    // The internal link, when asked for. A failure is reported rather than
    // silently delegated: an internal path that quietly falls back looks
    // identical to one that works, and the difference is the whole project.
    let exec_started = Instant::now();
    // `--blinker-internal` asks for an internal link, not for one at any cost.
    // rustc hands the same linker every crate in a workspace, and a proc-macro
    // crate is a `-dynamiclib` — so refusing outright makes blinker unusable on
    // any project that has one, which is most of them.
    let unsupported = unsupported_output_kind(&parsed);
    if let Some(flag) = unsupported {
        if options.internal_link && options.verbosity == Verbosity::Verbose {
            eprintln!("blinker: {flag} is not an output kind blinker produces; delegating");
        }
    }
    let exit_code = if options.internal_link && unsupported.is_none() {
        if options.verbosity == Verbosity::Verbose {
            eprintln!("blinker: linking internally");
        }
        let phases = internal_link(&parsed, &options, session).map_err(|e| DriverError::Link {
            detail: e.to_string(),
        })?;
        record.set_timing_internal_link(
            exec_started.elapsed(),
            blinker_diagnostics::LinkStages {
                read_and_parse: phases.read_and_parse_ms,
                resolve: phases.resolve_ms,
                layout: phases.layout_probe_ms,
                dead_strip: phases.dead_strip_ms,
                prepare: phases.prepare_ms,
                accounting: phases.accounting_ms,
                address_table: phases.address_table_ms,
                liveness_breakdown: [phases.group_ms, phases.traverse_ms],
                digest: phases.digest_ms,
                reach_moved: phases.reach_moved,
                reach_total: phases.reach_total,
                symbols_reused: phases.symbols_reused,
                symbols_total: phases.symbols_total,
                prepare_breakdown: [
                    phases.placements_ms,
                    phases.personality_ms,
                    phases.unwind_size_ms,
                    phases.commons_ms,
                ],
                synthetic_breakdown: [phases.eh_frame_ms, phases.tables_ms, phases.unwind_ms],
                address_diff: phases.address_diff_ms,
                changed_addresses: phases.changed_addresses,
                total_addresses: phases.total_addresses,
                strip_breakdown: [phases.atoms_ms, phases.liveness_ms, phases.strip_build_ms],
                relocate: phases.relocate_ms,
                emit: phases.emit_ms,
                write: phases.write_ms,
                stub_parse: phases.stub_parse_ms,
                emit_breakdown: [
                    phases.emit_breakdown.layout_ms,
                    phases.emit_breakdown.contents_ms,
                    phases.emit_breakdown.linkedit_ms,
                    phases.emit_breakdown.assemble_ms,
                    phases.emit_breakdown.uuid_ms,
                    phases.emit_breakdown.sign_ms,
                ],
                relocate_breakdown: [
                    phases.address_map_ms,
                    phases.contents_ms,
                    phases.synthetic_ms,
                    phases.apply_ms,
                ],
                symbols: phases.symbols_ms,
                survey: phases.survey_ms,
                cache: blinker_diagnostics::CacheStages {
                    load: phases.cache_load_ms,
                    plan: phases.cache_plan_ms,
                    build: phases.cache_build_ms,
                    store: phases.cache_store_ms,
                },
            },
        );
        record.set_session(
            phases.inputs_held,
            phases.inputs_read,
            phases.replayed_extraction,
            phases.held_resolution,
            phases.interface_changes,
            phases.first_interface_change.as_deref(),
        );
        if phases.contributions_retained > 0 || phases.contributions_moved > 0 {
            record.set_placement(
                phases.contributions_retained,
                phases.contributions_moved,
                phases.contributions_moved_unchanged,
            );
        }
        if phases.cache_bytes_read > 0 || phases.cache_bytes_written > 0 {
            record.set_cache_bytes(phases.cache_bytes_read, phases.cache_bytes_written);
        }
        if wants_dead_strip(&parsed) {
            record.set_dead_strip(phases.stripped_bytes, phases.revived_atoms);
        }
        if options.incremental_cache {
            record.set_reuse(
                phases.reused_objects,
                phases.total_objects,
                (phases.reused_relocations, phases.total_relocations),
            );
        } else {
            record.mode = blinker_diagnostics::LinkMode::Cold;
        }
        record.fallback_reason = None;
        0
    } else {
        let linker = fallback::discover(options.fallback_linker.as_deref())?;
        if options.verbosity == Verbosity::Verbose {
            eprintln!("blinker: delegating to {}", linker.display());
        }
        if unsupported.is_some() {
            record.fallback_reason = Some(blinker_diagnostics::FallbackReason::UnsupportedArgument);
        }
        fallback::execute(&linker, &parsed.argv)?
    };
    record.set_timing_fallback_exec(exec_started.elapsed());

    // The replay scratch directory must outlive the link that writes into it;
    // dropping it here (rather than at an implicit end-of-scope) makes that
    // ordering explicit instead of incidental.
    drop(replay_output);
    record.exit_code = exit_code;
    record.set_timing_total(started.elapsed());

    // Records are written even for a failed link: a failing invocation is
    // exactly the one worth having a fixture for.
    if let Some(dir) = &options.record_invocation {
        record.write_json(&recording_path(dir, &record))?;
    }
    if let Some(path) = &options.json_diagnostics {
        record.write_json(path)?;
    }
    if options.print_stats && options.verbosity != Verbosity::Quiet {
        eprintln!("{}", record.to_summary());
    }

    Ok(Outcome { exit_code, record })
}

/// An output kind blinker does not produce, if the invocation asks for one.
///
/// blinker emits a `MH_EXECUTE` image with an `LC_MAIN` entry point and nothing
/// else. Everything here is a different Mach-O file type, and each is named
/// rather than lumped together so the diagnostic says which one arrived.
///
/// Delegating is the correct answer rather than a stopgap: a shim linker that
/// refuses what it cannot do, loudly and by name, is usable today on projects
/// whose every crate it cannot yet link. One that fails is not.
fn unsupported_output_kind(parsed: &ParsedInvocation) -> Option<&'static str> {
    const KINDS: &[&str] = &[
        // A dynamic library — what every proc-macro crate is built as.
        "-dynamiclib",
        "-shared",
        // A loadable bundle, e.g. a plugin.
        "-bundle",
        // A relocatable partial link: object in, object out.
        "-r",
        // A static executable, which has no dyld and therefore no LC_MAIN
        // bootstrap of the kind blinker emits.
        "-static",
    ];
    parsed.args.iter().find_map(|(_, arg)| {
        let text = match arg {
            LinkerArg::KnownUnmodelled(flag)
            | LinkerArg::Unrecognized(flag)
            | LinkerArg::LinkerFlag(flag) => flag.as_str(),
            _ => return None,
        };
        KINDS.iter().find(|kind| **kind == text).copied()
    })
}

/// Whether the command line asked for unreachable input to be discarded.
///
/// `-dead_strip` is a linker flag rustc already passes on every macOS link;
/// honouring it is what makes blinker's output comparable to the system
/// linker's at all.
fn wants_dead_strip(parsed: &ParsedInvocation) -> bool {
    parsed
        .args
        .iter()
        .any(|(_, arg)| matches!(arg, LinkerArg::LinkerFlag(flag) if flag == "-dead_strip"))
}

/// The `.tbd` stubs the command line's `-l` and `-framework` requests name.
///
/// `-L` and `-F` accumulate first because a search path applies to every
/// request, wherever it appeared: `ld` collects the paths and then resolves.
/// The SDK's own directories come last, inside the resolver.
///
/// A request that resolves to something other than a `.tbd` is dropped rather
/// than guessed at — a `.dylib` needs its export trie read and a `.a` is an
/// archive, and neither is supported yet. The symbols it would have provided
/// then arrive as a named undefined-symbol error, which is the correct
/// outcome for "this link needs something blinker cannot read".
fn stub_libraries(parsed: &ParsedInvocation) -> Vec<PathBuf> {
    let mut library_paths = Vec::new();
    let mut framework_paths = Vec::new();
    for (_, arg) in &parsed.args {
        match arg {
            LinkerArg::LibrarySearchPath(path) => library_paths.push(path.clone()),
            LinkerArg::FrameworkSearchPath(path) => framework_paths.push(path.clone()),
            _ => {}
        }
    }

    let sdk = blinker_link::sdk_root();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_, arg) in &parsed.args {
        let path = match arg {
            LinkerArg::Library(name) => {
                blinker_link::libraries::find_library(name, &library_paths, sdk.as_deref())
            }
            LinkerArg::Framework(name) => {
                blinker_link::libraries::find_framework(name, &framework_paths, sdk.as_deref())
            }
            LinkerArg::DynamicLibrary(path) => Some(path.clone()),
            _ => continue,
        };
        let Some(path) = path else { continue };
        if path.extension().is_some_and(|e| e == "tbd") && seen.insert(path.clone()) {
            found.push(path);
        }
    }
    found
}

/// Every input the link reads, in the order the command line gave them.
///
/// Named inputs and `-l` requests interleave, and the order is not decoration:
/// an archive supplies a member only for a symbol that is undefined when the
/// linker reaches it, so moving `-ladder` past the objects that need it changes
/// what comes out.
///
/// `-l` that resolves to a `.tbd` is left to [`stub_libraries`]; a `.dylib` is
/// dropped, as it was before. What is new here is the static library. A build
/// script that compiles C and hands rustc `-ladder -L …/out` is the standard
/// shape of `cc`/`cmake` crates, and until the internal path became the default
/// nothing linked one: the request resolved to a `libadder.a` that was found
/// and then discarded, and the link failed on `_adder_add` undefined.
fn link_inputs(parsed: &ParsedInvocation) -> Vec<PathBuf> {
    let mut library_paths = Vec::new();
    for (_, arg) in &parsed.args {
        if let LinkerArg::LibrarySearchPath(path) = arg {
            library_paths.push(path.clone());
        }
    }
    let sdk = blinker_link::sdk_root();

    let mut inputs = Vec::new();
    for (_, arg) in &parsed.args {
        if let Some(path) = arg.input_path() {
            inputs.push(path.to_path_buf());
            continue;
        }
        let LinkerArg::Library(name) = arg else {
            continue;
        };
        // Only the static case. A `.tbd` describes a dylib to link against, not
        // an input to read, and it travels the other path.
        if let Some(found) =
            blinker_link::libraries::find_library(name, &library_paths, sdk.as_deref())
        {
            if found.extension().is_some_and(|kind| kind == "a") {
                inputs.push(found);
            }
        }
    }
    inputs
}

/// Perform the link with blinker's own linker.
///
/// Only object files are accepted so far. An archive or a dylib on the command
/// line is refused with a message naming it, rather than dropped — a link that
/// silently ignores an input produces a binary missing whatever was in it.
fn internal_link(
    parsed: &ParsedInvocation,
    options: &ProjectOptions,
    session: &mut blinker_link::Session,
) -> Result<blinker_link::LinkTimings, blinker_link::LinkError> {
    let output = parsed
        .output_path()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("a.out"));

    let objects: Vec<PathBuf> = link_inputs(parsed);

    let identifier = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a.out".to_string());

    let mut request = blinker_link::LinkRequest::new(objects)
        .identifier(&identifier)
        .dead_stripped(wants_dead_strip(parsed))
        .stub_libraries(stub_libraries(parsed))
        // Reserved slack pays for itself only across links, so it is turned on
        // with the cache — but it is applied to the cold link too, so that the
        // cached output is the one a cold link with the same options produces.
        .with_stable_layout(options.incremental_cache);
    // Opt-in for now: the reuse path is new, and a linker that silently
    // depends on state from a previous run is one whose output cannot be
    // reproduced from its inputs alone. `--blinker-cache` turns it on.
    if options.incremental_cache {
        request = request
            .cached_at(blinker_cache::cache_path(&output))
            .reusing_relocations(options.reuse_relocations)
            // Only when something will read it: the counter costs 0.46 ms.
            .counting_placement(
                options.json_diagnostics.is_some() || options.verbosity == Verbosity::Verbose,
            );
    }
    let timings = blinker_link::link_to_file_in(&request, &output, session)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o755)).map_err(
            |source| blinker_link::LinkError::Write {
                path: output.clone(),
                source,
            },
        )?;
    }
    Ok(timings)
}

/// Build a unique filename for a recorded invocation.
///
/// The output's file name is included so a corpus gathered from a real build is
/// browsable by target, and the pid keeps concurrent rustc jobs from colliding.
fn recording_path(dir: &Path, record: &LinkRecord) -> PathBuf {
    let stem = record
        .output_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    dir.join(format!("{stem}-{}.json", std::process::id()))
}

/// A directory that deletes itself when dropped.
///
/// Used to hold a replayed link's output so replaying never leaves artifacts
/// behind and never touches the path the original link wrote to.
#[derive(Debug)]
pub struct ScratchOutput(PathBuf);

impl ScratchOutput {
    fn new() -> std::io::Result<Self> {
        // pid alone is not unique enough: several replays can be in flight
        // within one process, and two sharing a directory would delete each
        // other's output when the first one dropped.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("blinker-replay-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(ScratchOutput(dir))
    }

    fn output_path(&self) -> PathBuf {
        self.0.join("replayed-output")
    }
}

impl Drop for ScratchOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build the argument vector for replaying a recorded invocation.
///
/// Prefers `replay_argv` (pointing at archived input copies) and falls back to
/// the verbatim `argv`, which only resolves while the original inputs still
/// exist. The output is redirected into a scratch directory so a replay cannot
/// overwrite a real build artifact.
fn prepare_replay(path: &Path) -> Result<(Vec<String>, ScratchOutput), DriverError> {
    let json = read_record(path)?;

    let argv_value = json
        .get("replay_argv")
        .filter(|v| !v.is_null())
        .or_else(|| json.get("argv"))
        .ok_or_else(|| DriverError::Replay {
            path: path.to_path_buf(),
            detail: "record contains neither `replay_argv` nor `argv`".to_string(),
        })?;

    let array = argv_value.as_array().ok_or_else(|| DriverError::Replay {
        path: path.to_path_buf(),
        detail: "argument vector is not an array".to_string(),
    })?;

    let argv: Vec<String> = array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| DriverError::Replay {
                    path: path.to_path_buf(),
                    detail: "argument vector contains a non-string element".to_string(),
                })
        })
        .collect::<Result<_, _>>()?;

    let scratch = ScratchOutput::new()?;
    let redirected = archive::redirect_output(&argv, &scratch.output_path());
    Ok((redirected, scratch))
}

fn read_record(path: &Path) -> Result<serde_json::Value, DriverError> {
    let contents = std::fs::read_to_string(path).map_err(|e| DriverError::Replay {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    serde_json::from_str(&contents).map_err(|e| DriverError::Replay {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_path_is_named_after_the_link_output() {
        let mut record = LinkRecord::delegated();
        record.output_path = Some(PathBuf::from("/t/deps/probe-717f"));
        let path = recording_path(Path::new("/corpus"), &record);
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("probe-717f-"), "got {name}");
        assert!(name.ends_with(".json"));
    }

    #[test]
    fn recording_path_tolerates_an_invocation_with_no_output() {
        let path = recording_path(Path::new("/corpus"), &LinkRecord::delegated());
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("unknown-"));
    }

    use blinker_test_support::Scratch;

    /// Scratch directory for record-file fixtures.
    fn scratch(tag: &str) -> Scratch {
        Scratch::dir(tag).unwrap()
    }

    #[test]
    fn replay_prefers_the_archived_argument_vector() {
        // The whole point of archiving: the verbatim `argv` points into a temp
        // directory rustc has already deleted, so `replay_argv` must win.
        let scratch = scratch("prefer");
        let mut record = LinkRecord::delegated();
        record.argv = vec!["/deleted/tmp/a.o".into(), "-o".into(), "out".into()];
        record.replay_argv = Some(vec!["/archive/0000-a.o".into(), "-o".into(), "out".into()]);
        let path = scratch.join("rec.json");
        record.write_json(&path).unwrap();

        let (argv, _scratch) = prepare_replay(&path).unwrap();
        assert_eq!(argv[0], "/archive/0000-a.o");
    }

    #[test]
    fn replay_falls_back_to_verbatim_argv_when_not_archived() {
        let scratch = scratch("fallback");
        let mut record = LinkRecord::delegated();
        record.argv = vec!["a.o".into()];
        let path = scratch.join("rec.json");
        record.write_json(&path).unwrap();

        let (argv, _scratch) = prepare_replay(&path).unwrap();
        assert_eq!(argv, vec!["a.o"]);
    }

    #[test]
    fn replay_redirects_output_away_from_the_original_artifact() {
        // Replaying is a diagnostic action; it must not be able to clobber a
        // real build tree.
        let scratch = scratch("redirect");
        let mut record = LinkRecord::delegated();
        record.argv = vec!["a.o".into(), "-o".into(), "/real/artifact".into()];
        let path = scratch.join("rec.json");
        record.write_json(&path).unwrap();

        let (argv, _scratch) = prepare_replay(&path).unwrap();
        let output = &argv[argv.iter().position(|a| a == "-o").unwrap() + 1];
        assert_ne!(output, "/real/artifact");
        assert!(output.contains("blinker-replay"));
    }

    #[test]
    fn replaying_a_malformed_record_is_an_error_not_an_empty_link() {
        // Treating a bad record as "no arguments" would silently produce a
        // meaningless link instead of reporting the problem.
        let scratch = scratch("malformed");
        for (name, contents) in [
            ("bad.json", &b"not json at all"[..]),
            ("noargv.json", br#"{"mode":"delegated"}"#),
            ("wrongtype.json", br#"{"argv":[1,2,3]}"#),
            ("notarray.json", br#"{"argv":"a.o"}"#),
        ] {
            let path = scratch.write(name, contents).unwrap();
            assert!(
                matches!(prepare_replay(&path), Err(DriverError::Replay { .. })),
                "{name} should have failed to replay"
            );
        }
    }

    #[test]
    fn replaying_a_missing_record_reports_the_path() {
        let err = prepare_replay(Path::new("/nonexistent/blinker/rec.json")).unwrap_err();
        assert!(matches!(err, DriverError::Replay { .. }));
    }

    #[test]
    fn scratch_output_is_removed_on_drop() {
        let dir = {
            let scratch = ScratchOutput::new().unwrap();
            let dir = scratch.0.clone();
            assert!(dir.exists());
            dir
        };
        assert!(!dir.exists(), "replay scratch directory leaked");
    }
}
