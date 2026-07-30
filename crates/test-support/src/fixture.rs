//! Minimal Rust fixture projects, built through blinker.
//!
//! This is the harness behind the M0 acceptance criterion: real Rust projects
//! must build when blinker occupies the linker position. Everything here drives
//! a genuine `cargo build`, because the whole point of M0 is to observe what
//! rustc *actually* does rather than what we assume it does.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::TempDir;

/// A generated Rust project on disk.
pub struct RustFixture {
    dir: TempDir,
    name: String,
}

/// The result of building a fixture.
pub struct FixtureBuild {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    /// Recorded invocation JSON files, if recording was requested.
    pub recordings: Vec<PathBuf>,
}

impl FixtureBuild {
    /// Parse the single recorded invocation, asserting exactly one exists.
    ///
    /// A link produces one record; more than one means the fixture linked
    /// several targets and the test needs to say which it meant.
    pub fn single_recording(&self) -> serde_json::Value {
        assert_eq!(
            self.recordings.len(),
            1,
            "expected exactly 1 recorded invocation, found {}",
            self.recordings.len()
        );
        let text = std::fs::read_to_string(&self.recordings[0]).expect("recording is readable");
        serde_json::from_str(&text).expect("recording is valid JSON")
    }
}

impl RustFixture {
    /// Create a single-binary crate with the given `main.rs` body.
    pub fn binary(tag: &str, main_body: &str) -> std::io::Result<Self> {
        let dir = TempDir::new(tag)?;
        let name = format!("fixture_{tag}");

        std::fs::create_dir_all(dir.join("src"))?;
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [dependencies]\n\n\
                 # Keep the fixture's own target dir self-contained so parallel\n\
                 # tests never share build state.\n\
                 [workspace]\n"
            ),
        )?;
        std::fs::write(dir.join("src/main.rs"), main_body)?;

        Ok(RustFixture { dir, name })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Build the fixture with `linker` installed in the linker position.
    ///
    /// `linker_args` are appended to the linker invocation via `-C link-arg`,
    /// which is how a caller passes blinker's own `--blinker-…` options through
    /// rustc down to the linker.
    pub fn build_with_linker(
        &self,
        linker: &Path,
        linker_args: &[String],
    ) -> std::io::Result<FixtureBuild> {
        let mut rustflags = format!("-C linker={}", linker.display());
        for arg in linker_args {
            rustflags.push_str(&format!(" -C link-arg={arg}"));
        }

        let recording_dir = self.dir.join("recordings");

        let output = Command::new("cargo")
            .arg("build")
            .arg("--target")
            .arg("aarch64-apple-darwin")
            .current_dir(self.dir.path())
            .env("RUSTFLAGS", rustflags)
            // Isolate from any ambient cargo configuration that might override
            // the linker we are trying to test.
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .env_remove("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER")
            .output()?;

        let recordings = read_dir_sorted(&recording_dir);

        Ok(FixtureBuild {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            recordings,
        })
    }

    /// Path where `build_with_linker` asks blinker to write recordings.
    pub fn recording_dir(&self) -> PathBuf {
        self.dir.join("recordings")
    }
}

/// List a directory's files in a stable order, or empty if it does not exist.
fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths
}

/// Source for the simplest possible linkable Rust program.
pub const MINIMAL_MAIN: &str = "fn main() { println!(\"fixture ok\"); }\n";

/// Source exercising several modules, a static, and a panic path — enough to
/// produce multiple codegen units and a non-trivial symbol table.
pub const MULTI_MODULE_MAIN: &str = r#"
mod alpha {
    pub static GREETING: &str = "alpha";
    pub fn compute(n: u64) -> u64 { n.wrapping_mul(2654435761) }
}

mod beta {
    pub fn describe(n: u64) -> String { format!("beta:{n}") }
}

thread_local! {
    static COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn main() {
    COUNTER.with(|c| c.set(c.get() + 1));
    let value = alpha::compute(COUNTER.with(|c| c.get()));
    println!("{} {}", alpha::GREETING, beta::describe(value));
    if std::env::var("FIXTURE_SHOULD_PANIC").is_ok() {
        panic!("requested panic");
    }
}
"#;
