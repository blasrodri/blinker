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

    // Serving is a mode, not a link: the process becomes the resident linker
    // and does not return until it has been idle long enough to be pointless.
    if argv.iter().any(|a| a == "--blinker-daemon-serve") {
        return match blinker_cli::daemon::serve_links() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("blinker: daemon: {error}");
                ExitCode::from(1)
            }
        };
    }

    // Using one is not. `--blinker-daemon` asks for the link to be performed by
    // a resident process if there is one, and performs it here if there is not
    // — so a build configured to use a daemon still links when none is running.
    if argv.iter().any(|a| a == "--blinker-daemon") {
        match blinker_cli::daemon::link_via_daemon(&argv) {
            Ok(Some(code)) => return exit_code(code),
            Ok(None) => {}
            Err(error) => eprintln!("blinker: daemon unavailable ({error}); linking in process"),
        }
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
