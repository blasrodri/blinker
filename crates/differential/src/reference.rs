//! Producing ground truth: linking a case with the system toolchain.
//!
//! # Why `cc` and not `ld` directly
//!
//! The spec's position (DECISIONS D4) is that blinker emulates the *compiler
//! driver*, not `ld64`. Invoking `ld` by hand would mean reconstructing the
//! driver's argument synthesis — `-syslibroot`, `-platform_version`,
//! `-lSystem`, the library search path — and any mistake there would show up
//! as a difference blamed on blinker.
//!
//! So the reference link goes through `cc`, exactly as a real build does, and
//! the argument vector `cc` would have handed to `ld` is captured separately
//! with `-###`. That vector is what blinker is then given, so both linkers see
//! the same request.

use blinker_test_support::Scratch;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum ReferenceError {
    /// A toolchain program could not be run at all.
    ToolMissing {
        program: String,
        source: std::io::Error,
    },
    /// A toolchain program ran and failed.
    ToolFailed {
        program: String,
        status: Option<i32>,
        stderr: String,
    },
    /// `cc -###` produced output we could not find a link line in.
    NoLinkCommand {
        output: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for ReferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReferenceError::ToolMissing { program, source } => {
                write!(f, "cannot run {program}: {source}")
            }
            ReferenceError::ToolFailed {
                program,
                status,
                stderr,
            } => write!(
                f,
                "{program} failed with status {status:?}:\n{}",
                stderr.trim()
            ),
            ReferenceError::NoLinkCommand { output } => write!(
                f,
                "no link command found in `cc -###` output:\n{}",
                output.trim()
            ),
            ReferenceError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReferenceError {}

impl From<std::io::Error> for ReferenceError {
    fn from(e: std::io::Error) -> Self {
        ReferenceError::Io(e)
    }
}

/// The deployment target every reference link is pinned to.
///
/// **This is the single most important setting in the harness.** The macOS
/// deployment target selects which dyld metadata strategy the linker emits:
///
/// | target | strategy |
/// |--------|----------|
/// | ≤ 11.x | `LC_DYLD_INFO_ONLY` — classic rebase/bind opcode streams |
/// | ≥ 12.0 | `LC_DYLD_CHAINED_FIXUPS` + `LC_DYLD_EXPORTS_TRIE` |
///
/// `cc`'s default on this machine is the running OS version (26.0), which
/// lands on chained fixups. `rustc`'s default for `aarch64-apple-darwin` is
/// 11.0, which lands on opcode streams — and blinker links *rustc's* output.
///
/// Leaving this unpinned made the harness's first run report that blinker
/// emitted the wrong dyld commands. blinker was right; the reference was built
/// against a different strategy than the one it will actually face.
/// [`deployment_target_matches_rustc`] pins this against `rustc` itself so the
/// two cannot drift apart silently.
///
/// [`deployment_target_matches_rustc`]: #
pub const DEPLOYMENT_TARGET: &str = "11.0";

/// A program to link, described by its sources.
///
/// C rather than Rust for the small cases: `cc` gives one object file per
/// source with no crate metadata, no `rlib` archives and no panic runtime, so
/// a failure is unambiguously about linking. Rust fixtures already exist in
/// `test-support` for the cases where the Rust-specific shapes matter.
#[derive(Debug, Clone)]
pub struct LinkCase {
    pub name: String,
    /// `(filename, contents)` pairs.
    pub sources: Vec<(String, String)>,
    /// Extra arguments passed to `cc` at link time.
    pub link_args: Vec<String>,
    /// The macOS deployment target. See [`DEPLOYMENT_TARGET`].
    pub deployment_target: String,
}

impl LinkCase {
    pub fn new(name: &str) -> Self {
        LinkCase {
            name: name.to_string(),
            sources: Vec::new(),
            link_args: Vec::new(),
            deployment_target: DEPLOYMENT_TARGET.to_string(),
        }
    }

    /// Override the deployment target — for tests that deliberately exercise
    /// the other dyld strategy.
    pub fn deployment_target(mut self, version: &str) -> Self {
        self.deployment_target = version.to_string();
        self
    }

    pub fn source(mut self, filename: &str, contents: &str) -> Self {
        self.sources
            .push((filename.to_string(), contents.to_string()));
        self
    }

    pub fn link_arg(mut self, arg: &str) -> Self {
        self.link_args.push(arg.to_string());
        self
    }
}

/// A case compiled to object files, plus the reference link of those objects.
///
/// The scratch directory is held so the object files outlive this struct's
/// creation — dropping it removes them.
#[derive(Debug)]
pub struct ReferenceLink {
    #[allow(dead_code)]
    directory: Scratch,
    pub objects: Vec<PathBuf>,
    /// The image `cc` produced.
    pub image: PathBuf,
    /// The argument vector `cc` would hand to `ld`, as reported by `-###`.
    pub link_argv: Vec<String>,
}

impl ReferenceLink {
    pub fn image_bytes(&self) -> std::io::Result<Vec<u8>> {
        std::fs::read(&self.image)
    }
}

/// Compile and link a case with the system toolchain.
pub fn build(case: &LinkCase) -> Result<ReferenceLink, ReferenceError> {
    let directory = Scratch::dir(&format!("diff-{}", case.name))?;

    // Pinned on both the compile and the link: the deployment target reaches
    // the linker through the object files' LC_BUILD_VERSION as well as the
    // driver's -platform_version, and a mismatch between the two is its own
    // class of confusing failure.
    let min_version = format!("-mmacosx-version-min={}", case.deployment_target);

    let mut objects = Vec::new();
    for (filename, contents) in &case.sources {
        let source = directory.write(filename, contents)?;
        let object = directory.join(format!("{filename}.o"));
        run(
            "cc",
            &[
                "-arch".as_ref(),
                "arm64".as_ref(),
                min_version.as_ref(),
                "-c".as_ref(),
                source.as_os_str(),
                "-o".as_ref(),
                object.as_os_str(),
            ],
        )?;
        objects.push(object);
    }

    let image = directory.join("reference");
    let mut link: Vec<std::ffi::OsString> = vec![
        "-arch".into(),
        "arm64".into(),
        min_version.clone().into(),
        "-o".into(),
        image.clone().into(),
    ];
    link.extend(objects.iter().map(|o| o.clone().into_os_string()));
    link.extend(case.link_args.iter().map(std::ffi::OsString::from));

    let link_refs: Vec<&std::ffi::OsStr> = link.iter().map(|s| s.as_os_str()).collect();
    run("cc", &link_refs)?;

    let link_argv = capture_link_argv(&link_refs)?;

    Ok(ReferenceLink {
        directory,
        objects,
        image,
        link_argv,
    })
}

/// The argument vector `cc` hands to `ld` for this link.
///
/// `-###` prints the commands the driver *would* run, quoted, without running
/// them. The link line is the one naming a linker binary.
fn capture_link_argv(args: &[&std::ffi::OsStr]) -> Result<Vec<String>, ReferenceError> {
    let mut with_flag: Vec<&std::ffi::OsStr> = vec!["-###".as_ref()];
    with_flag.extend_from_slice(args);

    let output = Command::new("cc")
        .args(&with_flag)
        .output()
        .map_err(|source| ReferenceError::ToolMissing {
            program: "cc".into(),
            source,
        })?;

    // `-###` writes to stderr, and exits successfully without linking.
    let text = String::from_utf8_lossy(&output.stderr).into_owned();

    for line in text.lines() {
        let trimmed = line.trim();
        // The driver prints one command per line; the link line invokes a
        // program whose name ends in `ld`, `ld64`, or similar. Matching on
        // "-dynamic" instead would miss a static link.
        let tokens = split_driver_line(trimmed);
        let Some(program) = tokens.first() else {
            continue;
        };
        let base = Path::new(program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if base == "ld" || base.starts_with("ld-") || base.ends_with("-ld") || base == "ld64" {
            return Ok(tokens);
        }
    }

    Err(ReferenceError::NoLinkCommand { output: text })
}

/// Split one `-###` line into tokens.
///
/// The driver quotes every argument with double quotes and escapes embedded
/// ones with a backslash. Splitting on whitespace alone would break any path
/// containing a space — which the scratch directories deliberately never
/// contain, but a user's SDK path very well might.
fn split_driver_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' if in_quotes => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                has_token = true;
                current.push(c);
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    tokens
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<String, ReferenceError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| ReferenceError::ToolMissing {
            program: program.into(),
            source,
        })?;

    if !output.status.success() {
        return Err(ReferenceError::ToolFailed {
            program: program.into(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_lines_are_split_on_quotes_not_whitespace() {
        // A path containing a space must survive: SDK paths on a real machine
        // are not guaranteed to be space-free.
        let line = r#" "/usr/bin/ld" "-o" "/tmp/a b/out" "-arch" "arm64""#;
        assert_eq!(
            split_driver_line(line),
            vec!["/usr/bin/ld", "-o", "/tmp/a b/out", "-arch", "arm64"]
        );
    }

    #[test]
    fn escaped_quotes_inside_arguments_survive() {
        let line = r#""/usr/bin/ld" "-DMSG=\"hi\"""#;
        assert_eq!(
            split_driver_line(line),
            vec!["/usr/bin/ld", r#"-DMSG="hi""#]
        );
    }

    #[test]
    fn an_empty_line_yields_no_tokens() {
        assert!(split_driver_line("").is_empty());
        assert!(split_driver_line("   ").is_empty());
    }
}
