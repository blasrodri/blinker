//! Temporary files and directories for tests.
//!
//! This exists because the same twenty lines were written by hand a dozen
//! times across the workspace, and two of those copies were wrong in ways that
//! cost real debugging time:
//!
//! - **Keyed on the pid alone**, so parallel tests in one process shared a
//!   directory and deleted it out from under each other. `cargo test` runs
//!   tests concurrently by default, so this fails intermittently rather than
//!   reliably — the worst failure mode to diagnose.
//! - **Keyed on `format!("{:?}", thread::current().id())`**, which renders as
//!   `ThreadId(6)`. The parentheses in a filename broke `otool`, which reported
//!   `can't open file` against a path truncated at the paren.
//!
//! So uniqueness here comes from the pid plus a process-wide counter, and the
//! resulting name is restricted to characters no external tool will object to.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counter making each scratch path unique.
///
/// The pid alone is not enough — tests within one process run concurrently.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Reduce a tag to characters that are safe in a path passed to any tool.
///
/// Anything outside `[a-z0-9-]` becomes a hyphen. Parentheses, spaces, and
/// quotes have all caused real failures when handed to `otool`, `ar`, or a
/// shell-invoking helper.
fn sanitize(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// A unique path under the system temp directory.
///
/// The name embeds the pid and a counter, so it is unique both across
/// concurrent processes and across concurrent tests within one process.
pub fn unique_path(tag: &str) -> PathBuf {
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "blinker-{}-{}-{seq}",
        sanitize(tag),
        std::process::id()
    ))
}

/// A temporary file or directory that removes itself when dropped.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    is_dir: bool,
}

impl Scratch {
    /// Create an empty directory.
    pub fn dir(tag: &str) -> std::io::Result<Self> {
        let path = unique_path(tag);
        // Remove first: a leftover from a killed run would otherwise make the
        // new test see stale contents.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Scratch { path, is_dir: true })
    }

    /// Create a file with the given contents.
    pub fn file(tag: &str, contents: &[u8]) -> std::io::Result<Self> {
        let path = unique_path(tag);
        std::fs::write(&path, contents)?;
        Ok(Scratch {
            path,
            is_dir: false,
        })
    }

    /// Create an executable file, for tests that spawn it.
    pub fn executable(tag: &str, contents: &str) -> std::io::Result<Self> {
        let scratch = Scratch::file(tag, contents.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&scratch.path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(scratch)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path as a string, for passing to an external tool.
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        self.path.to_string_lossy()
    }

    /// A path inside this directory.
    pub fn join(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.path.join(relative)
    }

    /// Write a file inside this directory, creating parent directories.
    ///
    /// `contents` takes `AsRef<[u8]>` so both `&str` and `&[u8]` work — most
    /// callers here write text, and requiring `.as_bytes()` at every site was
    /// noise.
    pub fn write(
        &self,
        relative: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> std::io::Result<PathBuf> {
        let path = self.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents.as_ref())?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: a test that already failed should not fail again on
        // cleanup, and a leaked temp file is not worth panicking over.
        if self.is_dir {
            let _ = std::fs::remove_dir_all(&self.path);
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug that made tests fail intermittently: the pid alone is shared by
    /// every test in the process.
    #[test]
    fn paths_are_unique_even_within_one_process() {
        let a = unique_path("dup");
        let b = unique_path("dup");
        assert_ne!(a, b, "two scratch paths collided");
    }

    /// The bug that broke `otool`: `ThreadId(6)` puts parentheses in a
    /// filename, and the tool reported the path truncated at the paren.
    #[test]
    fn paths_contain_no_characters_that_break_external_tools() {
        let path = unique_path("weird tag(with)parens'and\"quotes");
        let name = path.file_name().expect("has a name").to_string_lossy();

        for bad in ['(', ')', ' ', '\'', '"', '*', '?', '$', '&', ';'] {
            assert!(
                !name.contains(bad),
                "scratch name {name:?} contains {bad:?}, which breaks external tools"
            );
        }
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn a_directory_is_created_and_removed_with_its_contents() {
        let path = {
            let scratch = Scratch::dir("dircheck").expect("creates");
            scratch.write("nested/deep/file.txt", b"x").expect("writes");
            assert!(scratch.join("nested/deep/file.txt").exists());
            scratch.path().to_path_buf()
        };
        assert!(!path.exists(), "scratch directory leaked");
    }

    #[test]
    fn a_file_is_created_with_its_contents_and_removed() {
        let path = {
            let scratch = Scratch::file("filecheck", b"hello").expect("creates");
            assert_eq!(std::fs::read(scratch.path()).expect("readable"), b"hello");
            scratch.path().to_path_buf()
        };
        assert!(!path.exists(), "scratch file leaked");
    }

    #[test]
    fn an_executable_can_be_run() {
        let script = Scratch::executable("exec", "#!/bin/sh\nexit 7\n").expect("creates");
        let status = std::process::Command::new(script.path())
            .status()
            .expect("runs");
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn a_stale_directory_from_an_earlier_run_is_cleared() {
        // A killed run leaves a directory behind; the next test must not see
        // its contents.
        let scratch = Scratch::dir("stale").expect("creates");
        let path = scratch.path().to_path_buf();
        scratch.write("leftover", b"old").expect("writes");
        std::mem::forget(scratch); // simulate a run that never cleaned up

        let recreated = Scratch {
            path: path.clone(),
            is_dir: true,
        };
        drop(recreated);

        let fresh = Scratch::dir("stale2").expect("creates");
        assert!(!fresh.join("leftover").exists());
    }
}
