//! blinker's own options, and how they are separated from the linker's.
//!
//! # Why a prefix namespace
//!
//! blinker sits in the *C compiler driver* position (see the `arguments`
//! crate), so the argument vector it receives is full of driver flags: `-o`,
//! `-L`, `-l`, `-arch`, `-F`, `-m…`, `-W…`. Any short or conventional option
//! name we might want is already taken or plausibly will be. Every blinker
//! option therefore carries a `--blinker-` prefix, which cannot collide with a
//! driver or `ld64` flag.
//!
//! Project options are **removed** from the vector before it is forwarded, so
//! the fallback linker never sees them.

use std::path::PathBuf;

/// Diagnostic verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

/// blinker's own configuration for one invocation.
#[derive(Debug, Clone, Default)]
pub struct ProjectOptions {
    /// Explicit fallback linker path.
    pub fallback_linker: Option<PathBuf>,
    /// Directory to write a recorded invocation JSON into.
    pub record_invocation: Option<PathBuf>,
    /// A previously recorded invocation to replay instead of using argv.
    pub replay_invocation: Option<PathBuf>,
    /// Path to write the machine-readable record to.
    pub json_diagnostics: Option<PathBuf>,
    /// Perform the link internally instead of delegating.
    ///
    /// Opt-in rather than default: delegation is the behaviour that is known
    /// to work for every input, and the internal path is still narrower than
    /// ld64's. Flipping the default is a decision to make from evidence, not
    /// a side effect of the path existing.
    pub internal_link: bool,
    /// Reuse relocated output from a previous link, and record this one.
    ///
    /// Opt-in for the same reason `internal_link` is: a linker that silently
    /// depends on state from an earlier run produces output that cannot be
    /// reproduced from its inputs alone. Turning this on is a statement that
    /// the caller wants that trade.
    pub incremental_cache: bool,
    /// Also record and reuse per-object relocation state.
    ///
    /// Separate from `incremental_cache` because the two mechanisms have
    /// opposite economics. Replaying an unchanged image skips the whole
    /// linker; reusing individual objects' relocated bytes skips one stage of
    /// six and makes every link record what each object read, which measured
    /// 10.2 ms of cost against at most 5.6 ms of saving (finding 94). It stays
    /// reachable because it is the scaffolding retained placement will use,
    /// and it is off because it does not pay.
    pub reuse_relocations: bool,
    /// Print the human-readable summary.
    pub print_stats: bool,
    /// Compute BLAKE3 hashes for every input (spec §13 verification path).
    pub strict_fingerprints: bool,
    pub verbosity: Verbosity,
    /// Print version and exit.
    pub version: bool,
    /// Print help and exit.
    pub help: bool,
}

#[derive(Debug)]
pub enum OptionError {
    /// A `--blinker-…` option that takes a value was given none.
    MissingValue(String),
    /// An option starting with `--blinker-` that we do not define. Rejected
    /// rather than forwarded: forwarding it would surface as a confusing error
    /// from the system linker instead of from us.
    UnknownProjectOption(String),
    /// A value that is not one of the accepted alternatives.
    InvalidValue { option: String, value: String },
}

impl std::fmt::Display for OptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OptionError::MissingValue(o) => write!(f, "{o} requires a value"),
            OptionError::UnknownProjectOption(o) => write!(f, "unknown blinker option: {o}"),
            OptionError::InvalidValue { option, value } => {
                write!(f, "invalid value for {option}: {value}")
            }
        }
    }
}

impl std::error::Error for OptionError {}

/// The result of splitting an argument vector.
#[derive(Debug)]
pub struct SplitArgs {
    pub options: ProjectOptions,
    /// Everything that was not a blinker option, in original order — this is
    /// what gets forwarded.
    pub linker_argv: Vec<String>,
}

const PREFIX: &str = "--blinker-";

/// Split blinker's options out of an argument vector.
///
/// Accepts both `--blinker-x=value` and `--blinker-x value` spellings.
pub fn split_args(argv: &[String]) -> Result<SplitArgs, OptionError> {
    let mut options = ProjectOptions::default();
    let mut linker_argv = Vec::with_capacity(argv.len());
    let mut i = 0;

    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with(PREFIX) {
            linker_argv.push(arg.clone());
            i += 1;
            continue;
        }

        // Accept `--blinker-name=value` by splitting; otherwise the value (if
        // the option needs one) is the next argument.
        let (name, inline_value) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg.as_str(), None),
        };

        // Consumes the following argument when there is no inline value.
        let take_value = |i: &mut usize| -> Result<String, OptionError> {
            match inline_value.clone() {
                Some(v) => Ok(v),
                None => {
                    let v = argv
                        .get(*i + 1)
                        .cloned()
                        .ok_or_else(|| OptionError::MissingValue(name.to_string()))?;
                    *i += 1;
                    Ok(v)
                }
            }
        };

        match name {
            "--blinker-fallback-linker" => {
                options.fallback_linker = Some(PathBuf::from(take_value(&mut i)?))
            }
            "--blinker-record-invocation" => {
                options.record_invocation = Some(PathBuf::from(take_value(&mut i)?))
            }
            "--blinker-replay-invocation" => {
                options.replay_invocation = Some(PathBuf::from(take_value(&mut i)?))
            }
            "--blinker-json-diagnostics" => {
                options.json_diagnostics = Some(PathBuf::from(take_value(&mut i)?))
            }
            "--blinker-diagnostics" => {
                let value = take_value(&mut i)?;
                options.verbosity = match value.as_str() {
                    "quiet" => Verbosity::Quiet,
                    "normal" => Verbosity::Normal,
                    "verbose" => Verbosity::Verbose,
                    _ => {
                        return Err(OptionError::InvalidValue {
                            option: name.to_string(),
                            value,
                        })
                    }
                };
            }
            "--blinker-internal" => options.internal_link = true,
            // Handled in `main` before the driver runs, and accepted here so
            // they are not reported as unrecognized and forwarded to `ld`.
            "--blinker-daemon" | "--blinker-daemon-serve" => {}
            "--blinker-cache" => options.incremental_cache = true,
            "--blinker-cache-relocations" => {
                options.incremental_cache = true;
                options.reuse_relocations = true;
            }
            "--blinker-print-stats" => options.print_stats = true,
            "--blinker-strict-fingerprints" => options.strict_fingerprints = true,
            "--blinker-version" => options.version = true,
            "--blinker-help" => options.help = true,
            other => return Err(OptionError::UnknownProjectOption(other.to_string())),
        }
        i += 1;
    }

    Ok(SplitArgs {
        options,
        linker_argv,
    })
}

pub const HELP: &str = "\
blinker — incremental Mach-O linker for Rust on Apple Silicon

USAGE:
    blinker [LINKER ARGUMENTS...] [--blinker-OPTION...]

blinker is installed in the linker position and receives the C-compiler-driver
argument vector that rustc generates. Its own options use a --blinker- prefix so
they cannot collide with driver or ld64 flags, and are removed before the
remaining arguments are forwarded.

OPTIONS:
    --blinker-fallback-linker <PATH>   Linker to delegate to (default: discovered)
    --blinker-record-invocation <DIR>  Record this invocation as JSON in DIR
    --blinker-replay-invocation <FILE> Replay a previously recorded invocation
    --blinker-json-diagnostics <PATH>  Write the machine-readable record to PATH
    --blinker-diagnostics <LEVEL>      quiet | normal | verbose
    --blinker-internal                 Link internally instead of delegating
    --blinker-cache                    Replay an unchanged image from a previous link
    --blinker-cache-relocations        Force per-object relocation reuse. Automatic
                                       under --blinker-daemon; a loss for one-shot links
    --blinker-daemon                   Link via a resident linker, if one is running
    --blinker-daemon-serve             Become the resident linker
    --blinker-print-stats              Print the human-readable summary
    --blinker-strict-fingerprints      Hash every input (slower, exact identity)
    --blinker-version                  Print version
    --blinker-help                     Print this help

SETUP:
    # .cargo/config.toml
    [target.aarch64-apple-darwin]
    linker = \"/absolute/path/to/blinker\"

    Remove that section to restore the default linker.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn passes_linker_arguments_through_untouched() {
        let input = argv(&["a.o", "-lSystem", "-arch", "arm64", "-o", "out"]);
        let split = split_args(&input).unwrap();
        assert_eq!(split.linker_argv, input);
    }

    #[test]
    fn strips_project_options_from_the_forwarded_vector() {
        // The fallback linker must never see a --blinker- flag.
        let split = split_args(&argv(&[
            "a.o",
            "--blinker-print-stats",
            "-o",
            "out",
            "--blinker-strict-fingerprints",
        ]))
        .unwrap();
        assert_eq!(split.linker_argv, argv(&["a.o", "-o", "out"]));
        assert!(split.options.print_stats);
        assert!(split.options.strict_fingerprints);
    }

    #[test]
    fn accepts_both_inline_and_separate_value_spellings() {
        let inline = split_args(&argv(&["--blinker-fallback-linker=/usr/bin/cc"])).unwrap();
        let separate = split_args(&argv(&["--blinker-fallback-linker", "/usr/bin/cc"])).unwrap();
        assert_eq!(
            inline.options.fallback_linker,
            Some(PathBuf::from("/usr/bin/cc"))
        );
        assert_eq!(
            separate.options.fallback_linker,
            inline.options.fallback_linker
        );
    }

    #[test]
    fn separate_value_is_consumed_not_forwarded() {
        // Regression guard: a naive implementation forwards the value as if it
        // were a linker argument, corrupting the link.
        let split =
            split_args(&argv(&["--blinker-fallback-linker", "/usr/bin/cc", "a.o"])).unwrap();
        assert_eq!(split.linker_argv, argv(&["a.o"]));
    }

    #[test]
    fn parses_every_diagnostics_level() {
        for (value, expected) in [
            ("quiet", Verbosity::Quiet),
            ("normal", Verbosity::Normal),
            ("verbose", Verbosity::Verbose),
        ] {
            let split = split_args(&argv(&["--blinker-diagnostics", value])).unwrap();
            assert_eq!(split.options.verbosity, expected);
        }
    }

    #[test]
    fn rejects_an_invalid_diagnostics_level() {
        let err = split_args(&argv(&["--blinker-diagnostics", "loud"])).unwrap_err();
        assert!(matches!(err, OptionError::InvalidValue { .. }));
    }

    #[test]
    fn rejects_unknown_project_options_rather_than_forwarding_them() {
        // Forwarding would produce a confusing error from the system linker
        // instead of a clear one from us.
        let err = split_args(&argv(&["--blinker-nonsense"])).unwrap_err();
        match err {
            OptionError::UnknownProjectOption(o) => assert_eq!(o, "--blinker-nonsense"),
            other => panic!("expected UnknownProjectOption, got {other:?}"),
        }
    }

    #[test]
    fn reports_a_missing_value_instead_of_silently_defaulting() {
        let err = split_args(&argv(&["--blinker-fallback-linker"])).unwrap_err();
        assert!(matches!(err, OptionError::MissingValue(_)));
    }

    #[test]
    fn does_not_mistake_similar_linker_flags_for_project_options() {
        // `--blink` and `-Wl,--blinker-ish` must pass straight through.
        let input = argv(&["--blink", "-Wl,--blinker-ish", "-blinker"]);
        let split = split_args(&input).unwrap();
        assert_eq!(split.linker_argv, input);
    }

    #[test]
    fn preserves_relative_order_of_forwarded_arguments() {
        // Link order is semantically load-bearing (archive resolution), so
        // stripping must never reorder what remains.
        let split = split_args(&argv(&[
            "1.o",
            "--blinker-print-stats",
            "2.o",
            "--blinker-diagnostics",
            "quiet",
            "3.o",
        ]))
        .unwrap();
        assert_eq!(split.linker_argv, argv(&["1.o", "2.o", "3.o"]));
    }

    #[test]
    fn empty_argv_is_valid_and_yields_defaults() {
        let split = split_args(&[]).unwrap();
        assert!(split.linker_argv.is_empty());
        assert_eq!(split.options.verbosity, Verbosity::Normal);
    }
}
