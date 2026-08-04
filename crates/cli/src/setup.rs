//! Turning blinker on for a project, and off again.
//!
//! Setting a linker means one key in one file:
//!
//! ```toml
//! [target.aarch64-apple-darwin]
//! linker = "/absolute/path/to/blinker"
//! ```
//!
//! That is small enough to write by hand and is what the README has always
//! said to do. It is also the whole of the gate between "I have built blinker"
//! and "my build is using it", and a gate that asks for an absolute path is one
//! people get wrong — a relative path, a path that was right before the binary
//! moved, or a `.cargo/config.toml` in the wrong directory. `--blinker-install`
//! writes it from the running binary's own location, which cannot be any of
//! those things.
//!
//! # Editing without a TOML parser
//!
//! This edits the file as text, and deliberately so: a parse-and-reprint
//! rewrites a file the user owns — losing comments, ordering and formatting —
//! to change one line of it. The cases it handles are exactly the shapes cargo
//! reads for this key, and **anything else is refused rather than guessed at**:
//! a `linker` already pointing somewhere that is not blinker is reported and
//! left alone, because a build that silently stops using the linker the user
//! configured is worse than a setup command that failed.

use std::path::{Path, PathBuf};

/// The only target blinker links for.
pub const TARGET: &str = "aarch64-apple-darwin";

#[derive(Debug)]
pub enum SetupError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The config already sets a linker, and it is not this one.
    ForeignLinker { path: PathBuf, linker: String },
    /// The running binary's own path could not be determined, so there is no
    /// absolute path to write.
    NoSelfPath(std::io::Error),
    /// `cargo` could not be run for `--blinker-try`.
    NoCargo(std::io::Error),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            SetupError::ForeignLinker { path, linker } => write!(
                f,
                "{} already sets linker = \"{linker}\"; \
                 remove that line first if you meant to replace it",
                path.display()
            ),
            SetupError::NoSelfPath(source) => {
                write!(f, "cannot find blinker's own path: {source}")
            }
            SetupError::NoCargo(source) => write!(f, "cannot run cargo: {source}"),
        }
    }
}

impl std::error::Error for SetupError {}

/// What an install or uninstall did.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// The file was written.
    Wrote(PathBuf),
    /// It already said what it needed to say.
    AlreadyDone(PathBuf),
    /// The file held nothing but blinker's key, so it is gone — along with the
    /// `.cargo` directory, if that held nothing but the file. Uninstalling
    /// should leave a project as it was found, and one that had no
    /// `.cargo/config.toml` before should not be left with an empty one.
    Removed(PathBuf),
}

/// Where cargo reads per-project configuration.
pub fn config_path(project: &Path) -> PathBuf {
    project.join(".cargo").join("config.toml")
}

/// The path of the running blinker, resolved so it survives a `cd`.
pub fn self_path() -> Result<PathBuf, SetupError> {
    let exe = std::env::current_exe().map_err(SetupError::NoSelfPath)?;
    // Canonicalized, because `current_exe` can return a path through a symlink
    // — including `target/debug/blinker` in a workspace that hardlinks it — and
    // the value written here has to keep meaning the same file tomorrow.
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// Point `project`'s cargo config at `linker`.
pub fn install(project: &Path, linker: &Path) -> Result<Change, SetupError> {
    let path = config_path(project);
    let existing = read(&path)?;
    let updated = with_linker(&existing, &linker.display().to_string(), &path)?;
    write_if_changed(&path, &existing, updated)
}

/// Remove the linker key blinker set, and the table if nothing else used it.
pub fn uninstall(project: &Path) -> Result<Change, SetupError> {
    let path = config_path(project);
    let existing = read(&path)?;
    let updated = without_linker(&existing);
    if updated.is_empty() && !existing.is_empty() {
        std::fs::remove_file(&path).map_err(|source| SetupError::Io {
            path: path.clone(),
            source,
        })?;
        // Only if it is empty, and never reported as a failure: a `.cargo`
        // directory holding anything else is not this command's business.
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        return Ok(Change::Removed(path));
    }
    write_if_changed(&path, &existing, updated)
}

fn read(path: &Path) -> Result<String, SetupError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(SetupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_if_changed(path: &Path, before: &str, after: String) -> Result<Change, SetupError> {
    if after == before {
        return Ok(Change::AlreadyDone(path.to_path_buf()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, after).map_err(|source| SetupError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Change::Wrote(path.to_path_buf()))
}

/// Whether a line opens the target table this key belongs in.
///
/// Both spellings cargo accepts: bare and quoted. A `[target.'cfg(…)']` table
/// is *not* one of them — it can also carry a `linker`, but it is a different
/// table with different scope, and treating it as this one would move a key
/// between them.
fn opens_target_table(line: &str) -> bool {
    let line = line.trim();
    line == format!("[target.{TARGET}]")
        || line == format!("[target.\"{TARGET}\"]")
        || line == format!("[target.'{TARGET}']")
}

fn opens_any_table(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('[') && line.ends_with(']')
}

/// The value of a `linker = "…"` assignment, if the line is one.
fn linker_value(line: &str) -> Option<&str> {
    let (key, value) = line.split_once('=')?;
    if key.trim() != "linker" {
        return None;
    }
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
}

/// A path that names blinker rather than some other linker.
///
/// Deliberately loose: a debug build, a release build, an installed copy and a
/// renamed one all count, because the question this answers is "is it safe to
/// overwrite" and overwriting one blinker with another is what the command is
/// for. Anything else is refused.
fn is_blinker(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("blinker"))
}

/// The config text with our linker key set.
fn with_linker(text: &str, linker: &str, path: &Path) -> Result<String, SetupError> {
    let assignment = format!("linker = \"{linker}\"");
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    let mut wrote = false;

    for line in text.lines() {
        if opens_any_table(line) {
            // Leaving the table without having found a key to replace: add one
            // at the end of it, where a reader looking for it would.
            if inside && !wrote {
                out.push(assignment.clone());
                wrote = true;
            }
            inside = opens_target_table(line);
            out.push(line.to_string());
            continue;
        }
        if inside {
            if let Some(existing) = linker_value(line) {
                if !is_blinker(existing) {
                    return Err(SetupError::ForeignLinker {
                        path: path.to_path_buf(),
                        linker: existing.to_string(),
                    });
                }
                out.push(assignment.clone());
                wrote = true;
                continue;
            }
        }
        out.push(line.to_string());
    }

    if inside && !wrote {
        out.push(assignment.clone());
        wrote = true;
    }
    if !wrote {
        if !out.is_empty() && out.last().is_some_and(|line| !line.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(format!("[target.{TARGET}]"));
        out.push(assignment);
    }
    let mut text = out.join("\n");
    text.push('\n');
    Ok(text)
}

/// The config text with our linker key — and the table, if it held nothing
/// else — removed.
fn without_linker(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    // Where the table header went in `out`, and whether anything but our key
    // followed it.
    let mut header: Option<usize> = None;
    let mut occupied = false;

    let close = |out: &mut Vec<String>, header: &mut Option<usize>, occupied: &mut bool| {
        if let Some(at) = header.take() {
            if !*occupied {
                out.remove(at);
            }
        }
        *occupied = false;
    };

    for line in text.lines() {
        if opens_any_table(line) {
            close(&mut out, &mut header, &mut occupied);
            inside = opens_target_table(line);
            out.push(line.to_string());
            if inside {
                header = Some(out.len() - 1);
            }
            continue;
        }
        if inside {
            if linker_value(line).is_some_and(is_blinker) {
                continue;
            }
            if !line.trim().is_empty() {
                occupied = true;
            }
        }
        out.push(line.to_string());
    }
    close(&mut out, &mut header, &mut occupied);

    // Trailing blank lines a removed table left behind.
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        return String::new();
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Build a project through blinker without configuring anything.
///
/// The point is that it leaves nothing behind: no `.cargo/config.toml`, and a
/// target directory of its own, so a build that used blinker and one that did
/// not can both exist and neither invalidates the other. `RUSTFLAGS` alone
/// would rebuild the project's own target directory from scratch and then make
/// the next ordinary `cargo build` do it again.
pub fn try_build(project: &Path, linker: &Path, args: &[String]) -> Result<i32, SetupError> {
    let mut flags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if !flags.is_empty() {
        flags.push(' ');
    }
    flags.push_str(&format!("-C linker={}", linker.display()));

    let target_dir = project.join("target").join("blinker-try");
    let default = ["build".to_string()];
    let args = if args.is_empty() { &default[..] } else { args };

    eprintln!(
        "blinker: cargo {} with linker={}, into {}",
        args.join(" "),
        linker.display(),
        target_dir.display()
    );
    let status = std::process::Command::new("cargo")
        .args(args)
        .current_dir(project)
        .env("RUSTFLAGS", flags)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .map_err(SetupError::NoCargo)?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(text: &str) -> String {
        with_linker(text, "/opt/blinker", Path::new("config.toml")).expect("editable")
    }

    #[test]
    fn an_empty_config_gains_the_table_and_the_key() {
        assert_eq!(
            set(""),
            "[target.aarch64-apple-darwin]\nlinker = \"/opt/blinker\"\n"
        );
    }

    #[test]
    fn an_existing_table_gains_only_the_key() {
        let before = "[target.aarch64-apple-darwin]\nrustflags = [\"-C\", \"debuginfo=1\"]\n";
        assert_eq!(
            set(before),
            "[target.aarch64-apple-darwin]\nrustflags = [\"-C\", \"debuginfo=1\"]\nlinker = \"/opt/blinker\"\n"
        );
    }

    /// Everything the user wrote survives — comments, other tables, ordering.
    /// This is the property that makes editing text rather than reprinting a
    /// parse the right choice.
    #[test]
    fn nothing_else_in_the_file_moves() {
        let before = "\
# my settings
[build]
jobs = 4

[alias]
b = \"build\"
";
        let after = set(before);
        assert!(after.starts_with(before.trim_end()), "{after}");
        assert!(after.ends_with("[target.aarch64-apple-darwin]\nlinker = \"/opt/blinker\"\n"));
    }

    #[test]
    fn an_existing_blinker_is_replaced_in_place() {
        let before = "[target.aarch64-apple-darwin]\nlinker = \"/old/blinker\"\nrustflags = []\n";
        assert_eq!(
            set(before),
            "[target.aarch64-apple-darwin]\nlinker = \"/opt/blinker\"\nrustflags = []\n"
        );
    }

    /// Somebody else's linker is a decision, not a leftover.
    #[test]
    fn another_linker_is_refused_rather_than_overwritten() {
        let before = "[target.aarch64-apple-darwin]\nlinker = \"/usr/bin/ld64.lld\"\n";
        let error = with_linker(before, "/opt/blinker", Path::new("c.toml"));
        assert!(matches!(error, Err(SetupError::ForeignLinker { .. })));
    }

    /// A `cfg` table is a different table. Writing our key into it would apply
    /// it to whatever that cfg matches instead of to this target.
    #[test]
    fn a_cfg_table_is_not_this_target() {
        let before = "[target.'cfg(unix)']\nlinker = \"/usr/bin/cc\"\n";
        let after = set(before);
        assert!(after.contains("[target.'cfg(unix)']\nlinker = \"/usr/bin/cc\""));
        assert!(after.ends_with("[target.aarch64-apple-darwin]\nlinker = \"/opt/blinker\"\n"));
    }

    #[test]
    fn setting_twice_changes_nothing_the_second_time() {
        let once = set("");
        assert_eq!(set(&once), once);
    }

    #[test]
    fn removing_takes_the_table_it_emptied() {
        let text = set("");
        assert_eq!(without_linker(&text), "");
    }

    #[test]
    fn removing_keeps_a_table_that_holds_anything_else() {
        let text = "[target.aarch64-apple-darwin]\nlinker = \"/opt/blinker\"\nrustflags = []\n";
        assert_eq!(
            without_linker(text),
            "[target.aarch64-apple-darwin]\nrustflags = []\n"
        );
    }

    /// Uninstall removes what blinker set and nothing else — a config pointing
    /// at another linker is left exactly as it was.
    #[test]
    fn removing_leaves_another_linker_alone() {
        let text = "[target.aarch64-apple-darwin]\nlinker = \"/usr/bin/ld\"\n";
        assert_eq!(without_linker(text), text);
    }

    #[test]
    fn install_then_uninstall_returns_the_file_to_what_it_was() {
        let before = "[build]\njobs = 4\n";
        assert_eq!(without_linker(&set(before)), before);
    }
}
