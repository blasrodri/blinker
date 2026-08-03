//! Test scaffolding shared across blinker's crates.
//!
//! The pieces here exist so integration tests can drive a *real* `cargo build`
//! through the real blinker binary and assert on what came out — the M0
//! acceptance bar is "representative Rust projects build through the wrapper",
//! and that cannot be checked with unit tests over synthetic argv.

use std::path::PathBuf;
use std::process::Command;

pub mod fixture;
pub mod scratch;

pub use fixture::{
    catalog, BuildCommand, FixtureBuild, FixtureKind, Network, RustFixture, HEAVY_GENERICS_MAIN,
    MINIMAL_MAIN, MULTI_MODULE_MAIN,
};
pub use scratch::{unique_path, Scratch};

/// Absolute path to a binary built by this workspace.
///
/// Derived from the test executable's own location rather than assuming
/// `target/debug`, so it stays correct under `--release`, custom
/// `CARGO_TARGET_DIR`, and cross-target builds.
pub fn workspace_binary(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().expect("test executable has a path");
    dir.pop(); // the test binary itself
    if dir.ends_with("deps") {
        dir.pop();
    }
    let candidate = dir.join(name);
    assert!(
        candidate.exists(),
        "binary `{name}` not found at {} — build the workspace first",
        candidate.display()
    );
    candidate
}

/// A blinker invocation that links in this process and starts no daemon.
///
/// blinker engages a resident linker by default, which is right for a build and
/// wrong for a test: a test that asserts on a *direct* link would have it
/// served by a daemon holding another test's session, tests that run in
/// parallel would serialise through one socket, and the daemon changes the
/// working directory of a process the test does not control.
///
/// Tests that want a daemon spawn one and ask for it explicitly; everything
/// else goes through here. See [`no_daemon`] for the cargo builds that reach
/// blinker indirectly.
pub fn blinker() -> Command {
    let mut command = Command::new(workspace_binary("blinker"));
    no_daemon(&mut command);
    command
}

/// Keep a command — and anything it spawns — from engaging a resident linker.
///
/// The variable travels through `cargo` and `rustc` to the blinker they invoke,
/// which is the only way to reach a linker a test does not construct itself.
pub fn no_daemon(command: &mut Command) -> &mut Command {
    command.env("BLINKER_NO_DAEMON", "1")
}
