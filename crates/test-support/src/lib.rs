//! Test scaffolding shared across blinker's crates.
//!
//! The pieces here exist so integration tests can drive a *real* `cargo build`
//! through the real blinker binary and assert on what came out — the M0
//! acceptance bar is "representative Rust projects build through the wrapper",
//! and that cannot be checked with unit tests over synthetic argv.

use std::path::PathBuf;

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
