//! blinker's entry point.
//!
//! Kept deliberately thin: all logic lives in the library so it can be driven
//! directly by tests without spawning a process. This binary only handles
//! argv acquisition, the two early-exit flags, and turning a `DriverError`
//! into an exit status.

use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Handled before the driver runs: neither should require a valid link
    // configuration, and both must work when blinker is invoked by hand.
    if argv.iter().any(|a| a == "--blinker-help") {
        print!("{}", blinker_cli::HELP);
        return ExitCode::SUCCESS;
    }
    if argv.iter().any(|a| a == "--blinker-version") {
        println!("blinker {}", blinker_cli::VERSION);
        return ExitCode::SUCCESS;
    }

    // Setup, which is the one thing a user does before ever using blinker as a
    // linker — and the only reason to run it by hand other than stopping the
    // daemon. Handled here, ahead of everything, because none of it is a link
    // and none of it should need a valid link configuration.
    if let Some(code) = setup_mode(&argv) {
        return code;
    }

    // Serving is a mode, not a link: the process becomes the resident linker
    // and does not return until it has been idle long enough to be pointless.
    if let Some(worker) = blinker_cli::daemon::serving(&argv) {
        return match blinker_cli::daemon::serve_links(worker) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("blinker: daemon: {error}");
                ExitCode::from(1)
            }
        };
    }

    // Stopping one is a mode too, and the only one a user can reach: a daemon
    // nobody started by hand is a process nobody knows the name of.
    if argv.iter().any(|a| a == "--blinker-daemon-stop") {
        return match blinker_cli::daemon::stop_resident() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("blinker: daemon: {error}");
                ExitCode::from(1)
            }
        };
    }

    // Using one is not, and is what happens unless it is refused. A resident
    // linker performs this link if one is running, and one is started for the
    // next link if not — see `daemon::engage`. `--blinker-daemon` is still
    // accepted and now says nothing new.
    if let Some(code) = blinker_cli::daemon::engage(&argv) {
        return exit_code(code);
    }

    match blinker_cli::run(&argv) {
        Ok(outcome) => exit_code(outcome.exit_code),
        Err(err) => {
            eprintln!("blinker: {err}");
            // 1 is blinker's own failure. A delegated link's own non-zero code
            // travels through the Ok path above, so the two never blur.
            ExitCode::from(1)
        }
    }
}

/// Convert a child exit code into an `ExitCode`.
///
/// `ExitCode` is a `u8`; a code outside that range (or negative) cannot be
/// represented, so it collapses to 1 rather than wrapping into a value that
/// might read as success.
fn exit_code(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(c) => ExitCode::from(c),
        Err(_) => ExitCode::from(1),
    }
}

/// `--blinker-install`, `--blinker-uninstall` and `--blinker-try`.
///
/// `None` when the argument vector asks for none of them, which is every
/// invocation rustc ever makes.
fn setup_mode(argv: &[String]) -> Option<ExitCode> {
    use blinker_cli::setup::{self, Change};

    let project = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("blinker: cannot find the current directory: {error}");
            return Some(ExitCode::from(1));
        }
    };

    let report = |result: Result<Change, setup::SetupError>, verb: &str| match result {
        Ok(Change::Wrote(path)) => {
            eprintln!("blinker: {verb} {}", path.display());
            ExitCode::SUCCESS
        }
        Ok(Change::Removed(path)) => {
            eprintln!("blinker: removed {}", path.display());
            ExitCode::SUCCESS
        }
        Ok(Change::AlreadyDone(path)) => {
            eprintln!("blinker: {} already says so", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("blinker: {error}");
            ExitCode::from(1)
        }
    };

    if argv.iter().any(|a| a == "--blinker-install") {
        let linker = match setup::self_path() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("blinker: {error}");
                return Some(ExitCode::from(1));
            }
        };
        return Some(report(setup::install(&project, &linker), "wrote"));
    }
    if argv.iter().any(|a| a == "--blinker-uninstall") {
        return Some(report(setup::uninstall(&project), "updated"));
    }

    // Everything after the flag is cargo's, so `--blinker-try test --release`
    // means what it looks like. Blinker's own options are not mixed in: this
    // process runs no link, it runs cargo.
    let at = argv.iter().position(|a| a == "--blinker-try")?;
    let linker = match setup::self_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("blinker: {error}");
            return Some(ExitCode::from(1));
        }
    };
    match setup::try_build(&project, &linker, &argv[at + 1..]) {
        Ok(code) => Some(exit_code(code)),
        Err(error) => {
            eprintln!("blinker: {error}");
            Some(ExitCode::from(1))
        }
    }
}
