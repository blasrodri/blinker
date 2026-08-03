//! A backtrace must say which source line each frame is on.
//!
//! Names alone are half the answer. `fixture::deep` three times over says
//! nothing about *where* in `deep` the program was, and a debugger cannot
//! break on a line it has no address for. The missing piece is the **debug
//! map**: a Mach-O executable does not carry its debug information, it carries
//! a table of stabs saying which `.o` holds the DWARF for each function and
//! what address that function ended up at. `lldb` reads it directly and
//! `dsymutil` reads it to build a `.dSYM`.
//!
//! blinker emitted no stabs at all, so every consumer of the debug map got an
//! empty one:
//!
//! ```text
//!   ld-prime                       blinker
//!   1: hello::deep                 1: hello::deep
//!        at ./hello.rs:1:38                          <- nothing
//! ```
//!
//! These build a real crate and are slow. The property is about what the Rust
//! runtime prints, which needs the Rust runtime; and it is end-to-end on
//! purpose, because every intermediate check passed while the feature did not
//! exist. `crates/link/tests/emits_a_debug_map.rs` covers the structure.

use blinker_test_support::{workspace_binary, Scratch};
use std::process::Command;

struct Built {
    stderr: String,
    _scratch: Scratch,
}

/// Two private functions and a recursion, so there are several distinct lines
/// to get right rather than one.
const RECURSES_AND_PANICS: &str = r#"
fn deep(n: u32) -> u32 {
    if n == 0 {
        panic!("bottom")
    } else {
        deep(n - 1)
    }
}

#[inline(never)]
fn middle() -> u32 {
    deep(3)
}

fn main() {
    println!("{}", middle());
}
"#;

fn build(tag: &str, internal: bool) -> Built {
    let scratch = Scratch::dir(tag).expect("scratch");
    scratch
        .write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    scratch
        .write("src/main.rs", RECURSES_AND_PANICS)
        .expect("source");

    let mut build = Command::new("cargo");
    build.env("BLINKER_NO_DAEMON", "1");
    build
        .arg("build")
        .arg("--offline")
        .current_dir(scratch.path())
        .env("CARGO_TARGET_DIR", scratch.join("target"));
    if internal {
        build.env(
            "RUSTFLAGS",
            format!(
                "-C linker={} -C link-arg=--blinker-internal",
                workspace_binary("blinker").display()
            ),
        );
    }
    let output = build.output().expect("cargo runs");
    assert!(
        output.status.success(),
        "cargo build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let run = Command::new(scratch.join("target/debug/fixture"))
        .env("RUST_BACKTRACE", "1")
        .output()
        .expect("the program runs");
    Built {
        stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
        _scratch: scratch,
    }
}

/// Source locations attributed to the fixture's own file, as `line:column`.
///
/// Only `main.rs` is counted: a backtrace through `std` carries locations from
/// the Rust distribution's own debug information, which is present regardless
/// of what this linker emits, so counting those would pass on a linker that
/// emitted no debug map at all.
fn fixture_locations(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("at ") && line.contains("main.rs:"))
        .map(|line| {
            line.rsplit("main.rs:")
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// The property: frames in the fixture's own code carry its source lines.
#[test]
fn a_panic_backtrace_carries_the_source_lines_of_the_crate_being_linked() {
    let built = build("debugmap-internal", true);
    let locations = fixture_locations(&built.stderr);
    assert!(
        !locations.is_empty(),
        "no frame carries a location in the fixture's own source:\n{}",
        built.stderr
    );
    // `deep` recurses, `middle` calls it, `main` calls `middle`: at least
    // three distinct lines, not one repeated. A debug map that resolved every
    // address to the same line would satisfy a bare "has a location" check.
    let distinct: std::collections::BTreeSet<_> = locations.iter().collect();
    assert!(
        distinct.len() >= 3,
        "every frame resolved to the same place ({distinct:?}), which is not \
         what this call stack looks like:\n{}",
        built.stderr
    );
}

/// The control: the same source built with the system linker.
///
/// Both the count and the distinctness above are properties of the fixture and
/// the toolchain as much as of blinker — an optimiser that inlined `deep`
/// away, or a runtime that stopped resolving lines, would fail them for
/// reasons that have nothing to do with this linker. This says the expectation
/// is reachable here at all.
///
/// It must pass both before and after the fix, and it did pass while the test
/// above failed.
#[test]
fn the_system_linker_produces_the_source_lines_this_expects() {
    let built = build("debugmap-control", false);
    let locations = fixture_locations(&built.stderr);
    let distinct: std::collections::BTreeSet<_> = locations.iter().collect();
    assert!(
        distinct.len() >= 3,
        "the expectation is not reachable on this machine even with the \
         system linker, so these tests are measuring the fixture rather than \
         blinker; got {distinct:?}:\n{}",
        built.stderr
    );
}
