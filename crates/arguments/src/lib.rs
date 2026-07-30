//! Parsing and classification of the linker invocation `rustc` generates.
//!
//! # What rustc actually passes
//!
//! `rustc` does not invoke `ld` directly. It invokes the *C compiler driver*
//! (`cc`) and lets the driver call `ld64`. The `linker = ...` key in
//! `.cargo/config.toml` therefore names a program that occupies the **cc
//! driver** position, and the arguments it receives are driver-style, not
//! `ld64`-style. A minimal `cargo build` for `aarch64-apple-darwin` produces
//! roughly:
//!
//! ```text
//! <symbols.o> <N × *.rcgu.o> <M × *.rlib>
//! -lSystem -lc -lm
//! -arch arm64
//! -mmacosx-version-min=11.0.0
//! -o <output>
//! -Wl,-dead_strip
//! -nodefaultlibs
//! ```
//!
//! Two consequences shape this module:
//!
//! 1. Flags like `-arch` and `-mmacosx-version-min=` are driver spellings, and
//!    real `ld64` options arrive tunnelled inside `-Wl,`. We therefore parse
//!    the driver surface and split `-Wl,` payloads into their own arguments.
//! 2. Driver flags collide freely with anything we might want for ourselves, so
//!    every blinker-specific option lives behind a `--blinker-` prefix
//!    (see the `cli` crate).
//!
//! Nothing here is discarded silently: an argument we do not recognize becomes
//! [`LinkerArg::Unrecognized`] and is reported in the invocation inventory,
//! satisfying the "never silently ignore unknown input" rule.

use std::path::{Path, PathBuf};

mod response_file;
pub use response_file::{expand_response_files, ResponseFileError};

/// One classified argument from the invocation.
///
/// Variants carry the parsed payload; the original spelling is always
/// recoverable from [`ParsedInvocation::argv`] via the recorded index, so
/// classification never loses information needed to replay the invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkerArg {
    /// `-o <path>`
    Output(PathBuf),
    /// `-arch <name>` — e.g. `arm64`.
    Arch(String),
    /// `-m<platform>-version-min=<version>`, e.g. `-mmacosx-version-min=11.0.0`.
    DeploymentTarget { platform: String, version: String },
    /// `-L <dir>` / `-L<dir>`
    LibrarySearchPath(PathBuf),
    /// `-F <dir>` / `-F<dir>`
    FrameworkSearchPath(PathBuf),
    /// `-l<name>` — a library request resolved through the search paths.
    Library(String),
    /// `-framework <name>`
    Framework(String),
    /// A positional Mach-O object file (`.o`).
    ObjectFile(PathBuf),
    /// A positional static archive (`.a`).
    Archive(PathBuf),
    /// A positional Rust library archive (`.rlib`).
    Rlib(PathBuf),
    /// A positional dynamic library (`.dylib` / `.tbd`).
    DynamicLibrary(PathBuf),
    /// One `ld64` option tunnelled through `-Wl,`. A single `-Wl,a,b` argument
    /// expands to one `LinkerFlag` per comma-separated element, because that is
    /// how the driver forwards them to `ld64`.
    LinkerFlag(String),
    /// `-nodefaultlibs`, `-nostdlib`, and friends: suppresses driver-injected
    /// default libraries.
    NoDefaultLibs(String),
    /// A driver flag we recognize as harmless-to-forward but do not yet model
    /// semantically. Distinct from `Unrecognized`: this is a deliberate
    /// "known, not yet interpreted" state.
    KnownUnmodelled(String),
    /// Not recognized at all. Reported in the inventory; never dropped.
    Unrecognized(String),
}

impl LinkerArg {
    /// Stable category slug, used for inventory grouping and JSON output.
    pub fn category(&self) -> &'static str {
        match self {
            LinkerArg::Output(_) => "output",
            LinkerArg::Arch(_) => "arch",
            LinkerArg::DeploymentTarget { .. } => "deployment_target",
            LinkerArg::LibrarySearchPath(_) => "library_search_path",
            LinkerArg::FrameworkSearchPath(_) => "framework_search_path",
            LinkerArg::Library(_) => "library",
            LinkerArg::Framework(_) => "framework",
            LinkerArg::ObjectFile(_) => "object_file",
            LinkerArg::Archive(_) => "archive",
            LinkerArg::Rlib(_) => "rlib",
            LinkerArg::DynamicLibrary(_) => "dynamic_library",
            LinkerArg::LinkerFlag(_) => "linker_flag",
            LinkerArg::NoDefaultLibs(_) => "no_default_libs",
            LinkerArg::KnownUnmodelled(_) => "known_unmodelled",
            LinkerArg::Unrecognized(_) => "unrecognized",
        }
    }

    /// True when this argument names an input file that the linker must read.
    pub fn input_path(&self) -> Option<&Path> {
        match self {
            LinkerArg::ObjectFile(p)
            | LinkerArg::Archive(p)
            | LinkerArg::Rlib(p)
            | LinkerArg::DynamicLibrary(p) => Some(p),
            _ => None,
        }
    }
}

/// A fully classified invocation.
///
/// `argv` is the post-response-file-expansion argument vector, preserved
/// verbatim and in order. Fallback execution replays `argv` exactly rather than
/// re-rendering from `args`, so classification can never corrupt a delegated
/// link.
#[derive(Debug, Clone)]
pub struct ParsedInvocation {
    /// Verbatim arguments after response-file expansion, in original order.
    pub argv: Vec<String>,
    /// Classified arguments, paired with their index into `argv`.
    ///
    /// A `-Wl,a,b` argument yields multiple entries sharing one index; a
    /// two-token flag such as `-o out` yields one entry at the flag's index.
    pub args: Vec<(usize, LinkerArg)>,
}

impl ParsedInvocation {
    /// Classify an already-expanded argument vector.
    pub fn parse(argv: Vec<String>) -> Self {
        let args = classify(&argv);
        ParsedInvocation { argv, args }
    }

    fn find_map<'a, T>(&'a self, f: impl Fn(&'a LinkerArg) -> Option<T>) -> Option<T> {
        self.args.iter().find_map(|(_, a)| f(a))
    }

    /// The `-o` output path, if one was given.
    pub fn output_path(&self) -> Option<&Path> {
        self.find_map(|a| match a {
            LinkerArg::Output(p) => Some(p.as_path()),
            _ => None,
        })
    }

    /// The `-arch` value, if one was given.
    pub fn arch(&self) -> Option<&str> {
        self.find_map(|a| match a {
            LinkerArg::Arch(s) => Some(s.as_str()),
            _ => None,
        })
    }

    /// The deployment target as `(platform, version)`, if one was given.
    pub fn deployment_target(&self) -> Option<(&str, &str)> {
        self.find_map(|a| match a {
            LinkerArg::DeploymentTarget { platform, version } => {
                Some((platform.as_str(), version.as_str()))
            }
            _ => None,
        })
    }

    /// Every input file named positionally, in link order.
    ///
    /// Order matters: archive semantics are order-sensitive, so this preserves
    /// the sequence rather than grouping by kind.
    pub fn input_paths(&self) -> Vec<&Path> {
        self.args
            .iter()
            .filter_map(|(_, a)| a.input_path())
            .collect()
    }

    /// Arguments we could not classify. Non-empty means we are looking at
    /// something the corpus has not taught us about yet.
    pub fn unrecognized(&self) -> Vec<&str> {
        self.args
            .iter()
            .filter_map(|(_, a)| match a {
                LinkerArg::Unrecognized(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Flags that take their value as the *following* argument.
const SEPARATE_VALUE_FLAGS: &[&str] =
    &["-o", "-arch", "-framework", "-install_name", "-syslibroot"];

/// Driver flags we see from rustc, recognize, and forward without modelling.
const KNOWN_UNMODELLED: &[&str] = &[
    "-dynamiclib",
    "-shared",
    "-static",
    "-g",
    "-fPIC",
    "-pie",
    "-no-pie",
    "-v",
];

fn classify(argv: &[String]) -> Vec<(usize, LinkerArg)> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];

        // Two-token flags: consume the value that follows.
        if SEPARATE_VALUE_FLAGS.contains(&arg.as_str()) {
            let Some(value) = argv.get(i + 1) else {
                // A value-taking flag with nothing after it is malformed, not
                // unknown — record it verbatim so the inventory shows it.
                out.push((i, LinkerArg::Unrecognized(arg.clone())));
                i += 1;
                continue;
            };
            let parsed = match arg.as_str() {
                "-o" => LinkerArg::Output(PathBuf::from(value)),
                "-arch" => LinkerArg::Arch(value.clone()),
                "-framework" => LinkerArg::Framework(value.clone()),
                // Recognized, value-taking, not yet semantically modelled.
                _ => LinkerArg::KnownUnmodelled(format!("{arg} {value}")),
            };
            out.push((i, parsed));
            i += 2;
            continue;
        }

        // `-Wl,a,b,c` — driver tunnel for ld64 options. Split so each ld64
        // option is classified on its own, matching how the driver forwards it.
        if let Some(payload) = arg.strip_prefix("-Wl,") {
            for flag in payload.split(',') {
                if !flag.is_empty() {
                    out.push((i, LinkerArg::LinkerFlag(flag.to_string())));
                }
            }
            i += 1;
            continue;
        }

        // `-m<platform>-version-min=<version>`
        if let Some(parsed) = parse_version_min(arg) {
            out.push((i, parsed));
            i += 1;
            continue;
        }

        // Attached-value flags: `-L<dir>`, `-F<dir>`, `-l<name>`.
        if let Some(rest) = strip_attached(arg, "-L") {
            out.push((i, LinkerArg::LibrarySearchPath(PathBuf::from(rest))));
            i += 1;
            continue;
        }
        if let Some(rest) = strip_attached(arg, "-F") {
            out.push((i, LinkerArg::FrameworkSearchPath(PathBuf::from(rest))));
            i += 1;
            continue;
        }
        if let Some(rest) = strip_attached(arg, "-l") {
            out.push((i, LinkerArg::Library(rest.to_string())));
            i += 1;
            continue;
        }

        if arg == "-nodefaultlibs" || arg == "-nostdlib" || arg == "-nostartfiles" {
            out.push((i, LinkerArg::NoDefaultLibs(arg.clone())));
            i += 1;
            continue;
        }

        if KNOWN_UNMODELLED.contains(&arg.as_str()) {
            out.push((i, LinkerArg::KnownUnmodelled(arg.clone())));
            i += 1;
            continue;
        }

        // Anything else starting with `-` is a flag we do not know.
        if arg.starts_with('-') {
            out.push((i, LinkerArg::Unrecognized(arg.clone())));
            i += 1;
            continue;
        }

        // Positional: classify by extension.
        out.push((i, classify_positional(arg)));
        i += 1;
    }
    out
}

/// Classify a positional (non-flag) argument by file extension.
///
/// Extension is the only signal available without opening the file; M1 will
/// verify the actual container format when it parses these inputs.
fn classify_positional(arg: &str) -> LinkerArg {
    let path = PathBuf::from(arg);
    match path.extension().and_then(|e| e.to_str()) {
        Some("o") => LinkerArg::ObjectFile(path),
        Some("a") => LinkerArg::Archive(path),
        Some("rlib") => LinkerArg::Rlib(path),
        Some("dylib") | Some("tbd") => LinkerArg::DynamicLibrary(path),
        _ => LinkerArg::Unrecognized(arg.to_string()),
    }
}

/// Parse `-m<platform>-version-min=<version>`.
fn parse_version_min(arg: &str) -> Option<LinkerArg> {
    let rest = arg.strip_prefix("-m")?;
    let (platform, version) = rest.split_once("-version-min=")?;
    if platform.is_empty() || version.is_empty() {
        return None;
    }
    Some(LinkerArg::DeploymentTarget {
        platform: platform.to_string(),
        version: version.to_string(),
    })
}

/// Strip a flag prefix, returning the attached value if non-empty.
///
/// Returns `None` for the bare flag (e.g. `-L` alone), which is a
/// separate-value spelling we do not currently see from rustc; leaving it
/// unrecognized surfaces it in the inventory rather than guessing.
fn strip_attached<'a>(arg: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = arg.strip_prefix(prefix)?;
    (!rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The exact argument shape captured from a real `cargo build` on
    /// aarch64-apple-darwin. This is the M0 ground-truth fixture: if rustc's
    /// invocation shape changes, this test is the first thing to fail.
    fn real_rustc_invocation() -> Vec<String> {
        argv(&[
            "/t/deps/rustcXXXX/symbols.o",
            "/t/deps/probe-717f.1c9vuuc.rcgu.o",
            "/rustlib/lib/libstd-4f24f0876fd27385.rlib",
            "/rustlib/lib/libcore-df38416008f914c9.rlib",
            "-lSystem",
            "-lc",
            "-lm",
            "-arch",
            "arm64",
            "-mmacosx-version-min=11.0.0",
            "-o",
            "/t/deps/probe-717f633124e51aa4",
            "-Wl,-dead_strip",
            "-nodefaultlibs",
        ])
    }

    #[test]
    fn parses_real_rustc_invocation() {
        let parsed = ParsedInvocation::parse(real_rustc_invocation());

        assert_eq!(parsed.arch(), Some("arm64"));
        assert_eq!(parsed.deployment_target(), Some(("macosx", "11.0.0")));
        assert_eq!(
            parsed.output_path(),
            Some(Path::new("/t/deps/probe-717f633124e51aa4"))
        );
        assert_eq!(
            parsed.input_paths(),
            vec![
                Path::new("/t/deps/rustcXXXX/symbols.o"),
                Path::new("/t/deps/probe-717f.1c9vuuc.rcgu.o"),
                Path::new("/rustlib/lib/libstd-4f24f0876fd27385.rlib"),
                Path::new("/rustlib/lib/libcore-df38416008f914c9.rlib"),
            ]
        );
    }

    /// The whole point of M0: a real invocation must classify completely.
    /// Anything left over is a gap in our model, not an acceptable default.
    #[test]
    fn real_invocation_has_no_unrecognized_arguments() {
        let parsed = ParsedInvocation::parse(real_rustc_invocation());
        assert_eq!(parsed.unrecognized(), Vec::<&str>::new());
    }

    #[test]
    fn collects_library_requests_in_order() {
        let parsed = ParsedInvocation::parse(real_rustc_invocation());
        let libs: Vec<&str> = parsed
            .args
            .iter()
            .filter_map(|(_, a)| match a {
                LinkerArg::Library(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(libs, vec!["System", "c", "m"]);
    }

    #[test]
    fn splits_wl_payload_into_individual_ld64_flags() {
        let parsed = ParsedInvocation::parse(argv(&["-Wl,-dead_strip,-no_pie,-x"]));
        let flags: Vec<&str> = parsed
            .args
            .iter()
            .filter_map(|(_, a)| match a {
                LinkerArg::LinkerFlag(f) => Some(f.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(flags, vec!["-dead_strip", "-no_pie", "-x"]);
    }

    #[test]
    fn wl_split_flags_share_the_originating_argv_index() {
        // All three came from argv[0]; replay must re-emit that one argument,
        // not three separate ones.
        let parsed = ParsedInvocation::parse(argv(&["-Wl,-a,-b,-c"]));
        assert!(parsed.args.iter().all(|(i, _)| *i == 0));
        assert_eq!(parsed.args.len(), 3);
    }

    #[test]
    fn ignores_empty_elements_in_wl_payload() {
        let parsed = ParsedInvocation::parse(argv(&["-Wl,-a,,-b"]));
        assert_eq!(parsed.args.len(), 2);
    }

    #[test]
    fn classifies_positional_inputs_by_extension() {
        let parsed = ParsedInvocation::parse(argv(&[
            "a.o",
            "b.rlib",
            "c.a",
            "d.dylib",
            "e.tbd",
            "mystery.xyz",
        ]));
        let cats: Vec<&str> = parsed.args.iter().map(|(_, a)| a.category()).collect();
        assert_eq!(
            cats,
            vec![
                "object_file",
                "rlib",
                "archive",
                "dynamic_library",
                "dynamic_library",
                "unrecognized"
            ]
        );
    }

    #[test]
    fn parses_attached_and_separate_value_flags() {
        let parsed = ParsedInvocation::parse(argv(&[
            "-L/usr/lib",
            "-F/System/Frameworks",
            "-framework",
            "CoreFoundation",
        ]));
        let args: Vec<&LinkerArg> = parsed.args.iter().map(|(_, a)| a).collect();
        assert_eq!(
            args,
            vec![
                &LinkerArg::LibrarySearchPath(PathBuf::from("/usr/lib")),
                &LinkerArg::FrameworkSearchPath(PathBuf::from("/System/Frameworks")),
                &LinkerArg::Framework("CoreFoundation".to_string()),
            ]
        );
    }

    #[test]
    fn parses_deployment_targets_for_multiple_platforms() {
        for (arg, platform, version) in [
            ("-mmacosx-version-min=11.0.0", "macosx", "11.0.0"),
            ("-miphoneos-version-min=14.0", "iphoneos", "14.0"),
        ] {
            assert_eq!(
                parse_version_min(arg),
                Some(LinkerArg::DeploymentTarget {
                    platform: platform.into(),
                    version: version.into(),
                })
            );
        }
    }

    #[test]
    fn rejects_malformed_version_min_rather_than_guessing() {
        assert_eq!(parse_version_min("-mmacosx-version-min="), None);
        assert_eq!(parse_version_min("-m-version-min=11.0"), None);
        assert_eq!(parse_version_min("-mtune=native"), None);
    }

    #[test]
    fn unknown_flags_are_surfaced_never_dropped() {
        let parsed = ParsedInvocation::parse(argv(&["-fno-such-flag", "--weird"]));
        assert_eq!(parsed.unrecognized(), vec!["-fno-such-flag", "--weird"]);
        // Both must still be present in argv for verbatim fallback replay.
        assert_eq!(parsed.argv.len(), 2);
    }

    #[test]
    fn value_taking_flag_at_end_of_argv_is_reported_not_panicked_on() {
        let parsed = ParsedInvocation::parse(argv(&["-o"]));
        assert_eq!(parsed.unrecognized(), vec!["-o"]);
        assert_eq!(parsed.output_path(), None);
    }

    #[test]
    fn bare_dash_l_is_unrecognized_rather_than_an_empty_library() {
        let parsed = ParsedInvocation::parse(argv(&["-l"]));
        assert_eq!(parsed.unrecognized(), vec!["-l"]);
    }

    #[test]
    fn argv_is_preserved_verbatim_for_fallback_replay() {
        let original = real_rustc_invocation();
        let parsed = ParsedInvocation::parse(original.clone());
        assert_eq!(parsed.argv, original);
    }
}
