//! Linking a real Rust program with blinker.
//!
//! The C tests establish that the pipeline is correct; this establishes that it
//! is *sufficient*. Rust brings everything C did not: `.rlib` archives that
//! must be extracted selectively, paired `SUBTRACTOR` relocations, thread-local
//! variables with their descriptors and pointer table, and thousands of
//! absolute pointers in data that dyld has to slide.
//!
//! Every one of those was found by running exactly this, and each failed in a
//! way no unit test would have produced.

use blinker_test_support::{workspace_binary, Scratch};
use std::process::Command;

/// Build a single-file Rust binary with blinker as the linker, and run it.
fn build_and_run(tag: &str, main_rs: &str) -> (Option<i32>, String, String) {
    let scratch = Scratch::dir(tag).expect("scratch");
    scratch
        .write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    scratch.write("src/main.rs", main_rs).expect("source");

    let blinker = workspace_binary("blinker");
    let output = Command::new("cargo")
        .arg("build")
        .current_dir(scratch.path())
        // Offline and target-local: the fixture has no dependencies, and a
        // test that reaches the network is a test that fails on a train.
        .arg("--offline")
        .env(
            "RUSTFLAGS",
            format!(
                "-C linker={} -C link-arg=--blinker-internal",
                blinker.display()
            ),
        )
        .env("CARGO_TARGET_DIR", scratch.join("target"))
        .output()
        .expect("cargo runs");

    assert!(
        output.status.success(),
        "cargo build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let binary = scratch.join("target/debug/fixture");
    let run = Command::new(&binary).output().expect("the program runs");
    (
        run.status.code(),
        String::from_utf8_lossy(&run.stdout).into_owned(),
        String::from_utf8_lossy(&run.stderr).into_owned(),
    )
}

/// The smallest Rust program: no output, just an exit status.
///
/// Even this pulls in the whole of `libstd`'s runtime start-up, thread-local
/// setup and panic machinery.
#[test]
fn a_minimal_rust_program_links_and_runs() {
    let (status, _, stderr) = build_and_run("rust-min", "fn main() { std::process::exit(11); }\n");
    assert_eq!(status, Some(11), "stderr: {stderr}");
}

/// `println!` brings in formatting, stdout locking and thread-locals.
#[test]
fn a_rust_program_that_prints_produces_the_right_output() {
    let (status, stdout, stderr) = build_and_run(
        "rust-print",
        "fn main() { println!(\"rust via blinker\"); std::process::exit(7); }\n",
    );
    assert_eq!(status, Some(7), "stderr: {stderr}");
    assert_eq!(stdout, "rust via blinker\n");
}

/// Collections, iterators, closures and allocation.
///
/// Exercises the parts of `libstd` with the densest vtable and static data —
/// which is where an unrebased absolute pointer shows up as a segfault rather
/// than a wrong answer.
#[test]
fn a_rust_program_using_collections_produces_the_right_answer() {
    let source = r#"
use std::collections::HashMap;
fn main() {
    let mut counts = HashMap::new();
    for word in "the quick brown fox jumps over the lazy dog the fox".split_whitespace() {
        *counts.entry(word).or_insert(0) += 1;
    }
    let mut pairs: Vec<_> = counts.into_iter().collect();
    pairs.sort();
    println!("{} distinct", pairs.len());
    let joined: String = (1..=5).map(|n| n.to_string()).collect::<Vec<_>>().join("-");
    println!("joined: {joined}");
    std::process::exit(pairs.len() as i32);
}
"#;
    let (status, stdout, stderr) = build_and_run("rust-collections", source);
    assert_eq!(status, Some(8), "stderr: {stderr}");
    assert!(stdout.contains("8 distinct"), "stdout: {stdout}");
    assert!(stdout.contains("joined: 1-2-3-4-5"), "stdout: {stdout}");
}
