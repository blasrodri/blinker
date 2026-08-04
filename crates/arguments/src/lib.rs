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

pub mod reference;
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
    /// `-install_name <path>` — the name a dylib records for itself, which is
    /// what anything linking against it will later look for.
    InstallName(String),
    /// `-exported_symbols_list <file>` — the only names the image exports.
    /// Everything else defined in it becomes invisible from outside.
    ExportedSymbolsList(PathBuf),
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
            LinkerArg::InstallName(_) => "install_name",
            LinkerArg::ExportedSymbolsList(_) => "exported_symbols_list",
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

    /// The name a dylib will record for itself, if `-install_name` was given.
    pub fn install_name(&self) -> Option<&str> {
        self.find_map(|a| match a {
            LinkerArg::InstallName(s) => Some(s.as_str()),
            _ => None,
        })
    }

    /// The file listing what the image may export, if one was named.
    pub fn exported_symbols_list(&self) -> Option<&Path> {
        self.find_map(|a| match a {
            LinkerArg::ExportedSymbolsList(p) => Some(p.as_path()),
            _ => None,
        })
    }

    /// Whether the invocation asks for a dynamic library.
    pub fn wants_dylib(&self) -> bool {
        self.args.iter().any(|(_, a)| {
            matches!(
                a,
                LinkerArg::KnownUnmodelled(flag) | LinkerArg::LinkerFlag(flag)
                    if flag == "-dynamiclib"
            )
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

/// Driver flags we see from rustc, recognize, and forward without modelling.
///
/// These are *compiler driver* spellings, not `ld64` options — the `ld64` set
/// lives in [`reference::LD64_OPTIONS`] with its arity.
const KNOWN_UNMODELLED: &[&str] = &[
    "-dynamiclib",
    "-shared",
    "-static",
    "-g",
    "-fPIC",
    "-pie",
    "-no-pie",
];

/// Classify one option and the `values` it consumes.
///
/// Options blinker models semantically get a typed variant; the rest are
/// recorded as recognized-but-unmodelled along with the arguments they ate, so
/// nothing is silently dropped and the arity is visible in the inventory.
fn classify_option(name: &str, values: &[String]) -> LinkerArg {
    match (name, values) {
        ("-o", [v]) => LinkerArg::Output(PathBuf::from(v)),
        ("-arch", [v]) => LinkerArg::Arch(v.clone()),
        ("-framework", [v]) => LinkerArg::Framework(v.clone()),
        ("-install_name", [v]) => LinkerArg::InstallName(v.clone()),
        ("-exported_symbols_list", [v]) => LinkerArg::ExportedSymbolsList(PathBuf::from(v)),
        ("-L", [v]) => LinkerArg::LibrarySearchPath(PathBuf::from(v)),
        ("-F", [v]) => LinkerArg::FrameworkSearchPath(PathBuf::from(v)),
        (_, []) => LinkerArg::KnownUnmodelled(name.to_string()),
        _ => LinkerArg::KnownUnmodelled(format!("{name} {}", values.join(" "))),
    }
}

/// Classify an attached-value flag such as `-L/usr/lib` or `-lSystem`.
fn classify_joined(arg: &str) -> Option<LinkerArg> {
    for prefix in reference::JOINED_PREFIXES {
        // Longest-match order is guaranteed by JOINED_PREFIXES' ordering, so
        // `-weak-lfoo` is not mistaken for `-l` with value `weak-lfoo`.
        let Some(rest) = arg.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            // Bare flag: the separate-argument spelling, handled by the table.
            return None;
        }
        return Some(match *prefix {
            "-L" => LinkerArg::LibrarySearchPath(PathBuf::from(rest)),
            "-F" => LinkerArg::FrameworkSearchPath(PathBuf::from(rest)),
            "-l" => LinkerArg::Library(rest.to_string()),
            // `-weak-l…`, `-O…` and friends: recognized, not yet modelled.
            _ => LinkerArg::KnownUnmodelled(arg.to_string()),
        });
    }
    None
}

fn classify(argv: &[String]) -> Vec<(usize, LinkerArg)> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];

        // `-Wl,a,b,c` — the driver's tunnel for ld64 options.
        //
        // Arity applies *within* the sequence: `-Wl,-exported_symbol,_main` is
        // one option consuming one value, not two independent flags. Splitting
        // blindly on commas would classify `_main` as its own option.
        //
        // And the sequence spans arguments. rustc writes an option and its
        // value as *two* `-Wl,` arguments — `-Wl,-exported_symbols_list`
        // followed by `-Wl,/path/to/list` — which is the only form it ever
        // emits for that flag. Reading each argument on its own leaves the
        // option with nothing and the path looking like an option, so a whole
        // run of consecutive `-Wl,` arguments is flattened and read as one.
        if arg.starts_with("-Wl,") {
            let mut elements: Vec<String> = Vec::new();
            // Which argv index each element came from, so a classified option
            // still points at the argument that introduced it.
            let mut origins: Vec<usize> = Vec::new();
            let mut at = i;
            while let Some(payload) = argv.get(at).and_then(|a| a.strip_prefix("-Wl,")) {
                for element in payload.split(',').filter(|e| !e.is_empty()) {
                    elements.push(element.to_string());
                    origins.push(at);
                }
                at += 1;
            }
            for (element, arg) in classify_ld64_sequence(&elements) {
                out.push((origins[element], arg));
            }
            i = at;
            continue;
        }

        // `-m<platform>-version-min=<version>` — a driver spelling with no
        // ld64 equivalent, so it is checked before the table.
        if let Some(parsed) = parse_version_min(arg) {
            out.push((i, parsed));
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

        // Table lookup drives arity. This is what stops a value-taking option
        // from letting its values be misread as input files.
        if let Some(arity) = reference::arity_of(arg) {
            let arity = arity as usize;
            let available = argv.len() - i - 1;
            if available < arity {
                // Declared arity cannot be satisfied: malformed, not unknown.
                out.push((i, LinkerArg::Unrecognized(arg.clone())));
                i += 1;
                continue;
            }
            let values = &argv[i + 1..=i + arity];
            out.push((i, classify_option(arg, values)));
            i += 1 + arity;
            continue;
        }

        // Attached-value flags, after the table so bare `-L` takes the
        // separate-argument path above.
        if let Some(parsed) = classify_joined(arg) {
            out.push((i, parsed));
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

/// Classify a sequence of `ld64` options, honouring arity.
///
/// Used for `-Wl,` payloads, where the elements form their own little argument
/// vector with the same value-consuming rules as the top level.
/// Classify a flattened `-Wl,` sequence, pairing each result with the index of
/// the element it started at.
fn classify_ld64_sequence(elements: &[String]) -> Vec<(usize, LinkerArg)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < elements.len() {
        let start = i;
        let element = &elements[i];
        let arity = reference::arity_of(element).unwrap_or(0) as usize;

        if arity > 0 && i + arity < elements.len() {
            let values = &elements[i + 1..=i + arity];
            // Modelled options are modelled wherever they arrive. An
            // `-exported_symbols_list` tunnelled through `-Wl,` is the same
            // instruction as one spelled out, and rustc only ever sends the
            // tunnelled form — so classifying it as an opaque `LinkerFlag`
            // here means the linker never sees the flag it does honour.
            let classified = match classify_option(element, values) {
                LinkerArg::KnownUnmodelled(_) => {
                    LinkerArg::LinkerFlag(format!("{element} {}", values.join(" ")))
                }
                modelled => modelled,
            };
            out.push((start, classified));
            i += 1 + arity;
        } else {
            out.push((start, LinkerArg::LinkerFlag(element.clone())));
            i += 1;
        }
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
        // Lowercase `-l` takes no separate value, so a bare one is malformed.
        let parsed = ParsedInvocation::parse(argv(&["-l"]));
        assert_eq!(parsed.unrecognized(), vec!["-l"]);
    }

    /// Observed from a real build: a `cargo:rustc-link-search=` directive in a
    /// build script reaches the linker as `-L` followed by the path as a
    /// separate argument, not as an attached `-L<dir>`. Handling only the
    /// attached form silently drops the search path, and the native library
    /// the build script just compiled fails to resolve.
    #[test]
    fn search_path_flags_accept_both_attached_and_separate_spellings() {
        for (input, expected) in [
            (
                vec!["-L", "/build/out"],
                LinkerArg::LibrarySearchPath(PathBuf::from("/build/out")),
            ),
            (
                vec!["-L/build/out"],
                LinkerArg::LibrarySearchPath(PathBuf::from("/build/out")),
            ),
            (
                vec!["-F", "/Frameworks"],
                LinkerArg::FrameworkSearchPath(PathBuf::from("/Frameworks")),
            ),
            (
                vec!["-F/Frameworks"],
                LinkerArg::FrameworkSearchPath(PathBuf::from("/Frameworks")),
            ),
        ] {
            let parsed = ParsedInvocation::parse(argv(&input));
            assert_eq!(
                parsed.args.iter().map(|(_, a)| a).collect::<Vec<_>>(),
                vec![&expected],
                "failed for {input:?}"
            );
            assert!(parsed.unrecognized().is_empty(), "failed for {input:?}");
        }
    }

    #[test]
    fn separate_form_search_path_does_not_leak_its_value_as_a_positional() {
        // The regression this guards: consuming `-L` but not its value leaves
        // the path to be misclassified as an input file.
        let parsed = ParsedInvocation::parse(argv(&["-L", "/build/out", "a.o"]));
        assert_eq!(parsed.input_paths(), vec![Path::new("a.o")]);
    }

    /// The bug class the option table exists to prevent. `-sectcreate` takes
    /// three arguments; without arity knowledge its three values are read as
    /// input files and the link silently gains three phantom inputs.
    ///
    /// No fixture we could plausibly write would have surfaced this — it comes
    /// from the table, which is the point.
    #[test]
    fn multi_argument_options_consume_all_their_values() {
        let parsed = ParsedInvocation::parse(argv(&[
            "-sectcreate",
            "__TEXT",
            "__info_plist",
            "plist.xml",
            "real.o",
        ]));
        assert_eq!(
            parsed.input_paths(),
            vec![Path::new("real.o")],
            "option values were misread as inputs"
        );
        assert!(parsed.unrecognized().is_empty());
    }

    #[test]
    fn four_argument_options_consume_all_their_values() {
        let parsed = ParsedInvocation::parse(argv(&[
            "-rename_section",
            "__OLD",
            "__old",
            "__NEW",
            "__new",
            "real.o",
        ]));
        assert_eq!(parsed.input_paths(), vec![Path::new("real.o")]);
    }

    /// `-platform_version macos 11.0 14.0` is the modern replacement for
    /// `-macosx_version_min`, and takes three arguments.
    #[test]
    fn platform_version_consumes_its_three_values() {
        let parsed =
            ParsedInvocation::parse(argv(&["-platform_version", "macos", "11.0", "14.0", "a.o"]));
        assert_eq!(parsed.input_paths(), vec![Path::new("a.o")]);
    }

    #[test]
    fn an_option_whose_declared_arity_cannot_be_satisfied_is_reported() {
        // Truncated input is malformed, not unknown. Both the option and its
        // orphaned value are surfaced, and neither is silently consumed or
        // read past the end of the vector.
        let parsed = ParsedInvocation::parse(argv(&["-sectcreate", "__TEXT"]));
        assert!(parsed.unrecognized().contains(&"-sectcreate"));
        assert!(parsed.input_paths().is_empty());
    }

    /// Inside a `-Wl,` payload the same arity rules apply, but the values are
    /// the following *comma elements*. Splitting blindly on commas would make
    /// `_main` look like its own ld64 option.
    #[test]
    fn wl_payload_honours_option_arity() {
        let parsed = ParsedInvocation::parse(argv(&["-Wl,-exported_symbol,_main"]));
        let flags: Vec<&str> = parsed
            .args
            .iter()
            .filter_map(|(_, a)| match a {
                LinkerArg::LinkerFlag(f) => Some(f.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(flags, vec!["-exported_symbol _main"]);
    }

    #[test]
    fn wl_payload_still_splits_independent_flags() {
        let parsed = ParsedInvocation::parse(argv(&["-Wl,-dead_strip,-no_pie"]));
        assert_eq!(parsed.args.len(), 2);
    }

    #[test]
    fn longest_joined_prefix_wins() {
        // `-weak-lfoo` is a weak library request, not `-l` with the value
        // `weak-lfoo`.
        let parsed = ParsedInvocation::parse(argv(&["-weak-lfoo"]));
        assert!(matches!(
            parsed.args[0].1,
            LinkerArg::KnownUnmodelled(ref s) if s == "-weak-lfoo"
        ));
    }

    #[test]
    fn argv_is_preserved_verbatim_for_fallback_replay() {
        let original = real_rustc_invocation();
        let parsed = ParsedInvocation::parse(original.clone());
        assert_eq!(parsed.argv, original);
    }
}
