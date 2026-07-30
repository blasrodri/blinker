//! Test scaffolding shared across blinker's crates.
//!
//! The pieces here exist so integration tests can drive a *real* `cargo build`
//! through the real blinker binary and assert on what came out — the M0
//! acceptance bar is "representative Rust projects build through the wrapper",
//! and that cannot be checked with unit tests over synthetic argv.

use std::path::{Path, PathBuf};

pub mod fixture;
pub use fixture::{
    catalog, BuildCommand, FixtureBuild, FixtureKind, Network, RustFixture, HEAVY_GENERICS_MAIN,
    MINIMAL_MAIN, MULTI_MODULE_MAIN,
};

/// A scratch directory that removes itself on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Create a uniquely named scratch directory.
    ///
    /// The name combines pid and thread id so concurrent tests — including
    /// `cargo test`'s default parallel harness — cannot collide.
    pub fn new(tag: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "blinker-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(TempDir(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
