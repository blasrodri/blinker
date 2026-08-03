//! Rust fixture projects, built through blinker.
//!
//! This is the harness behind the M0 acceptance criterion: real Rust projects
//! must build when blinker occupies the linker position. Everything here drives
//! a genuine `cargo build`, because the whole point of M0 is to observe what
//! rustc *actually* does rather than what we assume it does.
//!
//! [`catalog`] is the single source of truth for which project shapes we cover.
//! Both the integration tests and the corpus tool iterate it, so a shape added
//! here is immediately both tested and recorded.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Scratch;

/// How a fixture is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildCommand {
    /// `cargo build` — links one executable.
    Build,
    /// `cargo test --no-run` — links a test harness binary, which has a
    /// different symbol and dependency profile from a plain binary.
    TestNoRun,
}

impl BuildCommand {
    fn args(self) -> &'static [&'static str] {
        match self {
            BuildCommand::Build => &["build"],
            BuildCommand::TestNoRun => &["test", "--no-run"],
        }
    }
}

/// Whether a fixture needs to reach crates.io.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    NotNeeded,
    Required,
}

/// A generated Rust project on disk.
pub struct RustFixture {
    dir: Scratch,
    tag: String,
    build_command: BuildCommand,
    network: Network,
}

/// The result of building a fixture.
pub struct FixtureBuild {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    /// Recorded invocation JSON files, if recording was requested.
    pub recordings: Vec<PathBuf>,
    /// Wall-clock duration of the whole `cargo` invocation.
    pub elapsed: std::time::Duration,
}

impl FixtureBuild {
    /// Parse the single recorded invocation, asserting exactly one exists.
    pub fn single_recording(&self) -> serde_json::Value {
        assert_eq!(
            self.recordings.len(),
            1,
            "expected exactly 1 recorded invocation, found {}",
            self.recordings.len()
        );
        self.recording(0)
    }

    /// Parse the nth recorded invocation.
    pub fn recording(&self, index: usize) -> serde_json::Value {
        let text = std::fs::read_to_string(&self.recordings[index]).expect("recording is readable");
        serde_json::from_str(&text).expect("recording is valid JSON")
    }

    /// Parse every recorded invocation.
    pub fn all_recordings(&self) -> Vec<serde_json::Value> {
        (0..self.recordings.len())
            .map(|i| self.recording(i))
            .collect()
    }
}

impl RustFixture {
    /// Start an empty fixture. Files are added with [`RustFixture::file`].
    pub fn new(tag: &str) -> std::io::Result<Self> {
        Ok(RustFixture {
            dir: Scratch::dir(tag)?,
            tag: tag.to_string(),
            build_command: BuildCommand::Build,
            network: Network::NotNeeded,
        })
    }

    /// Write a file into the fixture, creating parent directories.
    pub fn file(self, rel: &str, contents: &str) -> std::io::Result<Self> {
        let path = self.dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(self)
    }

    pub fn build_command(mut self, cmd: BuildCommand) -> Self {
        self.build_command = cmd;
        self
    }

    pub fn needs_network(mut self) -> Self {
        self.network = Network::Required;
        self
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Crate name derived from the tag; also the built binary's name.
    pub fn name(&self) -> String {
        format!("fixture_{}", self.tag)
    }

    /// Path where `build_with_linker` asks blinker to write recordings.
    pub fn recording_dir(&self) -> PathBuf {
        self.dir.join("recordings")
    }

    /// Path of the built executable, for fixtures that produce one.
    pub fn built_binary(&self) -> PathBuf {
        self.dir
            .join("target/aarch64-apple-darwin/debug")
            .join(self.name())
    }

    /// Build the fixture with `linker` installed in the linker position.
    ///
    /// `linker_args` are appended via `-C link-arg`, which is how blinker's own
    /// `--blinker-…` options reach it through rustc.
    pub fn build_with_linker(
        &self,
        linker: &Path,
        linker_args: &[String],
    ) -> std::io::Result<FixtureBuild> {
        let mut rustflags = format!("-C linker={}", linker.display());
        for arg in linker_args {
            rustflags.push_str(&format!(" -C link-arg={arg}"));
        }
        self.run_cargo(Some(rustflags))
    }

    /// Build with the default system linker, for baseline timing.
    pub fn build_with_system_linker(&self) -> std::io::Result<FixtureBuild> {
        self.run_cargo(None)
    }

    /// Remove build artifacts so the next build links from scratch.
    ///
    /// Timing a link requires that the link actually happens; cargo will skip
    /// it entirely if the target is up to date.
    pub fn clean(&self) -> std::io::Result<()> {
        let target = self.dir.join("target");
        if target.exists() {
            std::fs::remove_dir_all(target)?;
        }
        Ok(())
    }

    fn run_cargo(&self, rustflags: Option<String>) -> std::io::Result<FixtureBuild> {
        let mut cmd = Command::new("cargo");
        cmd.args(self.build_command.args())
            .arg("--target")
            .arg("aarch64-apple-darwin")
            .current_dir(self.dir.path())
            // Isolate from ambient configuration that could override the
            // linker we are trying to exercise.
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .env_remove("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER");
        crate::no_daemon(&mut cmd);

        match rustflags {
            Some(flags) => {
                cmd.env("RUSTFLAGS", flags);
            }
            None => {
                cmd.env_remove("RUSTFLAGS");
            }
        }

        if self.network == Network::NotNeeded {
            cmd.arg("--offline");
        }

        let started = std::time::Instant::now();
        let output = cmd.output()?;
        let elapsed = started.elapsed();

        Ok(FixtureBuild {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            recordings: read_dir_sorted(&self.recording_dir()),
            elapsed,
        })
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

/// Standard `Cargo.toml` for a single-binary fixture.
///
/// `[workspace]` is empty on purpose: it stops the fixture from being adopted
/// into any surrounding workspace, keeping its target directory self-contained
/// so parallel builds never share state.
fn manifest(name: &str, extra: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [workspace]\n\n{extra}"
    )
}

// ---------------------------------------------------------------------------
// Fixture catalog
// ---------------------------------------------------------------------------

/// One entry in the fixture catalog.
pub struct FixtureKind {
    /// Short stable identifier, used in reports and test names.
    pub tag: &'static str,
    /// What linker-visible feature this shape is here to exercise.
    pub exercises: &'static str,
    pub network: Network,
    build: fn(&str) -> std::io::Result<RustFixture>,
}

impl FixtureKind {
    pub fn build(&self) -> std::io::Result<RustFixture> {
        (self.build)(self.tag)
    }
}

/// Every project shape the corpus covers.
///
/// Both the integration tests and the corpus tool iterate this list, so adding
/// a shape here immediately gets it both tested and recorded.
pub fn catalog() -> Vec<FixtureKind> {
    vec![
        FixtureKind {
            tag: "minimal",
            exercises: "smallest linkable binary",
            network: Network::NotNeeded,
            build: minimal,
        },
        FixtureKind {
            tag: "multimod",
            exercises: "several modules, TLS, statics, panic path",
            network: Network::NotNeeded,
            build: multi_module,
        },
        FixtureKind {
            tag: "workspace",
            exercises: "multi-crate workspace, cross-crate rlib linkage",
            network: Network::NotNeeded,
            build: workspace,
        },
        FixtureKind {
            tag: "buildscript",
            exercises: "build script emitting link arguments",
            network: Network::NotNeeded,
            build: build_script,
        },
        FixtureKind {
            tag: "cdep",
            exercises: "C static library built and linked via build script",
            network: Network::NotNeeded,
            build: c_dependency,
        },
        FixtureKind {
            tag: "procmacro",
            exercises: "proc macro at compile time, normal link of the user",
            network: Network::NotNeeded,
            build: proc_macro,
        },
        FixtureKind {
            tag: "testharness",
            exercises: "cargo test --no-run harness binary",
            network: Network::NotNeeded,
            build: test_harness,
        },
        FixtureKind {
            tag: "generics",
            exercises: "heavy generic instantiation, many codegen units",
            network: Network::NotNeeded,
            build: heavy_generics,
        },
        FixtureKind {
            tag: "deps",
            exercises: "real crates.io dependency graph",
            network: Network::Required,
            build: dependency_heavy,
        },
    ]
}

// --- individual fixtures ---------------------------------------------------

fn minimal(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    f.file("Cargo.toml", &manifest(&name, ""))?
        .file("src/main.rs", MINIMAL_MAIN)
}

fn multi_module(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    f.file("Cargo.toml", &manifest(&name, ""))?
        .file("src/main.rs", MULTI_MODULE_MAIN)
}

/// A workspace whose binary depends on two local library crates, so the link
/// pulls in rlibs produced by this build rather than only from the sysroot.
fn workspace(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    f.file(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"core_lib\", \"util_lib\"]\nresolver = \"2\"\n\n\
         [workspace.package]\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?
    .file(
        "app/Cargo.toml",
        &format!(
            "[package]\nname = \"{name}\"\nversion.workspace = true\nedition.workspace = true\n\n\
             [[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n\n\
             [dependencies]\ncore_lib = {{ path = \"../core_lib\" }}\n\
             util_lib = {{ path = \"../util_lib\" }}\n"
        ),
    )?
    .file(
        "app/src/main.rs",
        "fn main() {\n    let v = core_lib::seed(7);\n    \
         println!(\"{} {}\", v, util_lib::render(v));\n}\n",
    )?
    .file(
        "core_lib/Cargo.toml",
        "[package]\nname = \"core_lib\"\nversion.workspace = true\nedition.workspace = true\n",
    )?
    .file(
        "core_lib/src/lib.rs",
        "pub fn seed(n: u64) -> u64 { n.wrapping_mul(6364136223846793005).wrapping_add(1) }\n",
    )?
    .file(
        "util_lib/Cargo.toml",
        "[package]\nname = \"util_lib\"\nversion.workspace = true\nedition.workspace = true\n",
    )?
    .file(
        "util_lib/src/lib.rs",
        "pub fn render(n: u64) -> String { format!(\"{n:#x}\") }\n",
    )
}

/// A build script that emits linker arguments. This is the shape most likely to
/// introduce argument spellings rustc alone never produces.
fn build_script(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    f.file("Cargo.toml", &manifest(&name, "[build-dependencies]\n"))?
        .file(
            "build.rs",
            // `-dead_strip` is already present in a default link; re-requesting
            // it is harmless and keeps the fixture from depending on a flag
            // whose semantics we would otherwise have to reason about.
            "fn main() {\n    \
             println!(\"cargo:rustc-link-arg=-Wl,-dead_strip\");\n    \
             println!(\"cargo:rustc-link-lib=framework=CoreFoundation\");\n\
             }\n",
        )?
        .file(
            "src/main.rs",
            "fn main() { println!(\"buildscript fixture ok\"); }\n",
        )
}

/// A C static library compiled by a build script and linked in. The build
/// script shells out to `cc` directly rather than using the `cc` crate, so the
/// fixture needs no network and the link arguments are explicit.
fn c_dependency(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    f.file("Cargo.toml", &manifest(&name, ""))?
        .file(
            "csrc/adder.c",
            "#include <stdint.h>\nint64_t adder_add(int64_t a, int64_t b) { return a + b; }\n",
        )?
        .file(
            "build.rs",
            r#"use std::process::Command;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let obj = format!("{out_dir}/adder.o");
    let lib = format!("{out_dir}/libadder.a");

    let cc = Command::new("cc")
        .args(["-c", "csrc/adder.c", "-o", &obj])
        .status()
        .expect("cc runs");
    assert!(cc.success(), "compiling adder.c failed");

    let ar = Command::new("ar")
        .args(["crs", &lib, &obj])
        .status()
        .expect("ar runs");
    assert!(ar.success(), "archiving libadder.a failed");

    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=adder");
    println!("cargo:rerun-if-changed=csrc/adder.c");
}
"#,
        )?
        .file(
            "src/main.rs",
            "extern \"C\" {\n    fn adder_add(a: i64, b: i64) -> i64;\n}\n\n\
             fn main() {\n    let sum = unsafe { adder_add(40, 2) };\n    \
             println!(\"c dependency sum {sum}\");\n    assert_eq!(sum, 42);\n}\n",
        )
}

/// A proc macro expanded at compile time, with the consumer linked normally.
/// Written against raw `TokenStream` so it needs neither syn nor quote.
fn proc_macro(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    f.file(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"macros\"]\nresolver = \"2\"\n\n\
         [workspace.package]\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?
    .file(
        "app/Cargo.toml",
        &format!(
            "[package]\nname = \"{name}\"\nversion.workspace = true\nedition.workspace = true\n\n\
             [[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n\n\
             [dependencies]\nmacros = {{ path = \"../macros\" }}\n"
        ),
    )?
    .file(
        "app/src/main.rs",
        "macros::make_answer!();\n\nfn main() { println!(\"answer {}\", answer()); }\n",
    )?
    .file(
        "macros/Cargo.toml",
        "[package]\nname = \"macros\"\nversion.workspace = true\nedition.workspace = true\n\n\
         [lib]\nproc-macro = true\n",
    )?
    .file(
        "macros/src/lib.rs",
        "use proc_macro::TokenStream;\n\n\
         #[proc_macro]\npub fn make_answer(_item: TokenStream) -> TokenStream {\n    \
         \"fn answer() -> u32 { 42 }\".parse().unwrap()\n}\n",
    )
}

/// A `cargo test --no-run` harness binary — a different link shape from a plain
/// binary, pulling in the test harness and its dependencies.
fn test_harness(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    Ok(f.file("Cargo.toml", &manifest(&name, ""))?
        .file(
            "src/main.rs",
            "fn double(n: u64) -> u64 { n * 2 }\n\n\
             fn main() { println!(\"{}\", double(21)); }\n\n\
             #[cfg(test)]\nmod tests {\n    use super::*;\n\n    \
             #[test]\n    fn doubles() { assert_eq!(double(21), 42); }\n\n    \
             #[test]\n    fn doubles_zero() { assert_eq!(double(0), 0); }\n}\n",
        )?
        .build_command(BuildCommand::TestNoRun))
}

/// Heavy generic instantiation, which multiplies codegen units and symbol
/// count — the shape that stresses symbol resolution rather than argument
/// parsing.
fn heavy_generics(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    f.file("Cargo.toml", &manifest(&name, ""))?
        .file("src/main.rs", HEAVY_GENERICS_MAIN)
}

/// A real crates.io dependency graph. Requires network.
fn dependency_heavy(tag: &str) -> std::io::Result<RustFixture> {
    let f = RustFixture::new(tag)?;
    let name = f.name();
    Ok(f.file(
        "Cargo.toml",
        &manifest(
            &name,
            "[dependencies]\n\
             serde = { version = \"1\", features = [\"derive\"] }\n\
             serde_json = \"1\"\n\
             regex = \"1\"\n",
        ),
    )?
    .file(
        "src/main.rs",
        "use serde::Serialize;\n\n\
         #[derive(Serialize)]\nstruct Record { name: String, hits: usize }\n\n\
         fn main() {\n    \
         let re = regex::Regex::new(r\"\\b\\w{5}\\b\").unwrap();\n    \
         let hits = re.find_iter(\"there where these three words\").count();\n    \
         let record = Record { name: \"deps\".into(), hits };\n    \
         println!(\"{}\", serde_json::to_string(&record).unwrap());\n\
         }\n",
    )?
    .needs_network())
}

// --- fixture sources -------------------------------------------------------

/// Source for the simplest possible linkable Rust program.
pub const MINIMAL_MAIN: &str = "fn main() { println!(\"fixture ok\"); }\n";

/// Source exercising several modules, a static, TLS, and a panic path.
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

/// Source that instantiates a generic pipeline across many concrete types.
pub const HEAVY_GENERICS_MAIN: &str = r#"
use std::fmt::Debug;

trait Stage {
    type Out: Debug;
    fn run(&self) -> Self::Out;
}

#[derive(Debug)]
struct Wrap<T>(T);

impl<T: Copy + Debug> Stage for Wrap<T> {
    type Out = (T, T);
    fn run(&self) -> Self::Out { (self.0, self.0) }
}

fn pipeline<S: Stage>(s: S) -> String { format!("{:?}", s.run()) }

macro_rules! instantiate {
    ($($t:ty => $v:expr),* $(,)?) => {
        vec![$(pipeline(Wrap::<$t>($v))),*]
    };
}

fn main() {
    let results = instantiate! {
        u8 => 1, u16 => 2, u32 => 3, u64 => 4, u128 => 5,
        i8 => -1, i16 => -2, i32 => -3, i64 => -4, i128 => -5,
        f32 => 1.5, f64 => 2.5, char => 'x', bool => true,
        usize => 6, isize => -6,
    };
    println!("{} instantiations", results.len());
}
"#;
