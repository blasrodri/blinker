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

use blinker_arguments::{expand_response_files, ParsedInvocation};
use blinker_diagnostics::{fingerprint_input, LinkRecord};

pub mod archive;
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
        let mapping = archive::archive_inputs(&mut inputs, &archive_dir)?;
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
    let exit_code = if options.internal_link {
        if options.verbosity == Verbosity::Verbose {
            eprintln!("blinker: linking internally");
        }
        let phases = internal_link(&parsed, &options).map_err(|e| DriverError::Link {
            detail: e.to_string(),
        })?;
        record.set_timing_internal_link(
            exec_started.elapsed(),
            phases.read_and_parse_ms,
            phases.resolve_ms,
            phases.layout_probe_ms,
            phases.relocate_ms,
            phases.emit_ms,
        );
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

/// Whether the command line asked for unreachable input to be discarded.
///
/// `-dead_strip` is a linker flag rustc already passes on every macOS link;
/// honouring it is what makes blinker's output comparable to the system
/// linker's at all.
fn wants_dead_strip(parsed: &ParsedInvocation) -> bool {
    parsed.args.iter().any(|(_, arg)| {
        matches!(arg, blinker_arguments::LinkerArg::LinkerFlag(flag) if flag == "-dead_strip")
    })
}

/// Perform the link with blinker's own linker.
///
/// Only object files are accepted so far. An archive or a dylib on the command
/// line is refused with a message naming it, rather than dropped — a link that
/// silently ignores an input produces a binary missing whatever was in it.
fn internal_link(
    parsed: &ParsedInvocation,
    options: &ProjectOptions,
) -> Result<blinker_link::LinkTimings, blinker_link::LinkError> {
    let output = parsed
        .output_path()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("a.out"));

    let objects: Vec<PathBuf> = parsed
        .input_paths()
        .into_iter()
        .map(Path::to_path_buf)
        .collect();

    let identifier = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a.out".to_string());

    let mut request = blinker_link::LinkRequest::new(objects)
        .identifier(&identifier)
        .dead_stripped(wants_dead_strip(parsed));
    // Opt-in for now: the reuse path is new, and a linker that silently
    // depends on state from a previous run is one whose output cannot be
    // reproduced from its inputs alone. `--blinker-cache` turns it on.
    if options.incremental_cache {
        request = request.cached_at(blinker_cache::cache_path(&output));
    }
    let timings = blinker_link::link_to_file_timed(&request, &output)?;
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
