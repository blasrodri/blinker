//! Locating and invoking the linker we delegate to.
//!
//! Fallback is a permanent, first-class feature (spec §30), not scaffolding for
//! M0 — every later milestone keeps it as the path taken whenever an internal
//! link cannot be proven safe. Getting the discovery and exit-status semantics
//! right now means later milestones inherit a delegation path that is already
//! correct.
//!
//! # What we delegate to
//!
//! Because rustc invokes the *C compiler driver* rather than `ld` (see the
//! `arguments` crate), the argument vector we receive is driver-shaped: it
//! contains `-arch`, `-mmacosx-version-min=`, `-Wl,…`, `-nodefaultlibs`.
//! Forwarding that vector to `/usr/bin/ld` would fail — `ld` does not accept
//! those spellings. The default fallback is therefore `cc`, the program rustc
//! would have run if blinker were not installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Candidate fallback programs, in priority order, used when neither the
/// command line nor the environment names one.
const DEFAULT_CANDIDATES: &[&str] = &["/usr/bin/cc", "/usr/bin/clang"];

/// Environment variable naming the fallback linker.
pub const FALLBACK_ENV: &str = "BLINKER_FALLBACK_LINKER";

#[derive(Debug)]
pub enum FallbackError {
    /// No usable fallback program was found.
    NotFound { tried: Vec<PathBuf> },
    /// An explicitly configured fallback does not exist or is not executable.
    NotExecutable(PathBuf),
    /// The fallback could not be spawned.
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The fallback was terminated by a signal rather than exiting normally.
    Signalled { path: PathBuf },
}

impl std::fmt::Display for FallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FallbackError::NotFound { tried } => {
                let list: Vec<String> = tried.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "no fallback linker found (tried: {}); set {FALLBACK_ENV} or pass --blinker-fallback-linker",
                    list.join(", ")
                )
            }
            FallbackError::NotExecutable(p) => {
                write!(
                    f,
                    "configured fallback linker is not executable: {}",
                    p.display()
                )
            }
            FallbackError::Spawn { path, source } => {
                write!(
                    f,
                    "cannot execute fallback linker {}: {source}",
                    path.display()
                )
            }
            FallbackError::Signalled { path } => {
                write!(f, "fallback linker {} terminated by signal", path.display())
            }
        }
    }
}

impl std::error::Error for FallbackError {}

/// Resolve which program to delegate to.
///
/// Precedence, highest first:
/// 1. `explicit` (from `--blinker-fallback-linker`)
/// 2. the `BLINKER_FALLBACK_LINKER` environment variable
/// 3. the first existing entry in [`DEFAULT_CANDIDATES`]
///
/// An explicitly configured path that does not exist is an error rather than a
/// silent fall-through to a default: the user asked for a specific linker, and
/// quietly substituting another would change link semantics without saying so.
pub fn discover(explicit: Option<&Path>) -> Result<PathBuf, FallbackError> {
    if let Some(path) = explicit {
        return validate(path);
    }
    if let Some(from_env) = std::env::var_os(FALLBACK_ENV) {
        return validate(Path::new(&from_env));
    }

    let mut tried = Vec::new();
    for candidate in DEFAULT_CANDIDATES {
        let path = PathBuf::from(candidate);
        if is_executable(&path) {
            return Ok(path);
        }
        tried.push(path);
    }
    Err(FallbackError::NotFound { tried })
}

fn validate(path: &Path) -> Result<PathBuf, FallbackError> {
    if is_executable(path) {
        Ok(path.to_path_buf())
    } else {
        Err(FallbackError::NotExecutable(path.to_path_buf()))
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Run the fallback linker with `argv`, inheriting stdio.
///
/// stdout and stderr are inherited rather than captured so the child's
/// diagnostics reach the user exactly as they would without blinker in the
/// path — interleaving, colour detection, and all.
///
/// Returns the child's exit code. A process killed by a signal has no exit code
/// to propagate, so that becomes an explicit error rather than a fabricated
/// status.
pub fn execute(linker: &Path, argv: &[String]) -> Result<i32, FallbackError> {
    let status = Command::new(linker)
        .args(argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|source| FallbackError::Spawn {
            path: linker.to_path_buf(),
            source,
        })?;

    status.code().ok_or_else(|| FallbackError::Signalled {
        path: linker.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempExe(PathBuf);

    impl TempExe {
        /// Write an executable shell script to a unique temp path.
        fn new(tag: &str, script: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "blinker-fb-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(script.as_bytes()).unwrap();
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            TempExe(path)
        }
    }

    impl Drop for TempExe {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn explicit_path_takes_precedence() {
        let exe = TempExe::new("explicit", "#!/bin/sh\nexit 0\n");
        assert_eq!(discover(Some(&exe.0)).unwrap(), exe.0);
    }

    #[test]
    fn explicit_missing_path_errors_rather_than_falling_through() {
        // Silently substituting a different linker would change link semantics
        // without telling the user.
        let err = discover(Some(Path::new("/nonexistent/blinker/ld"))).unwrap_err();
        assert!(matches!(err, FallbackError::NotExecutable(_)));
    }

    #[test]
    fn non_executable_file_is_rejected() {
        let path = std::env::temp_dir().join(format!("blinker-noexec-{}", std::process::id()));
        std::fs::write(&path, b"not executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let err = discover(Some(&path)).unwrap_err();
        assert!(matches!(err, FallbackError::NotExecutable(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_directory_is_not_accepted_as_a_linker() {
        let err = discover(Some(&std::env::temp_dir())).unwrap_err();
        assert!(matches!(err, FallbackError::NotExecutable(_)));
    }

    /// On this platform the default candidate must exist — rustc relies on it.
    #[test]
    fn discovers_a_default_on_this_machine() {
        let found = discover(None).unwrap();
        assert!(is_executable(&found), "{} not executable", found.display());
    }

    #[test]
    fn propagates_a_successful_exit_code() {
        let exe = TempExe::new("ok", "#!/bin/sh\nexit 0\n");
        assert_eq!(execute(&exe.0, &[]).unwrap(), 0);
    }

    #[test]
    fn propagates_a_failing_exit_code_verbatim() {
        // ld64 exit codes carry meaning; collapsing them to a generic 1 would
        // hide why a link failed.
        let exe = TempExe::new("fail", "#!/bin/sh\nexit 42\n");
        assert_eq!(execute(&exe.0, &[]).unwrap(), 42);
    }

    #[test]
    fn forwards_arguments_to_the_child() {
        let exe = TempExe::new(
            "args",
            "#!/bin/sh\n[ \"$1\" = \"-o\" ] && [ \"$2\" = \"out\" ] && exit 7\nexit 1\n",
        );
        let argv = vec!["-o".to_string(), "out".to_string()];
        assert_eq!(execute(&exe.0, &argv).unwrap(), 7);
    }

    #[test]
    fn spawning_a_missing_program_is_an_error() {
        let err = execute(Path::new("/nonexistent/blinker/ld"), &[]).unwrap_err();
        assert!(matches!(err, FallbackError::Spawn { .. }));
    }
}
