//! The incremental cache, driven by a real Rust link.
//!
//! `crates/link/tests/writes_a_cache.rs` covers the same machinery with C
//! fixtures, and it is not sufficient: the bug in finding 64 — every one of 47
//! objects failing the byte copy because of a zero-filled section, so that
//! reuse was silently zero — reproduces here and **not** there. Reverting the
//! fix leaves every C test passing.
//!
//! What makes the difference is what Rust links against. `libstd` brings
//! `__bss` and the thread-local block into almost every object, and the C
//! fixture reaches neither in the same shape.
//!
//! These tests are slow — each builds a Rust crate — and they are worth it,
//! because the property they check has no other witness. A cache that stops
//! working produces a correct binary and a slower link, which no correctness
//! test can see.

use blinker_test_support::{workspace_binary, Scratch};
use std::path::Path;
use std::process::Command;

const SOURCE: &str = r#"
use std::collections::HashMap;

thread_local! {
    static COUNTER: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

static TABLE: [u32; 512] = [3; 512];

fn main() {
    COUNTER.with(|c| c.set(TABLE[7]));
    let mut counts = HashMap::new();
    for word in "a b a c b a".split_whitespace() {
        *counts.entry(word).or_insert(0u32) += 1;
    }
    let total: u32 = counts.values().sum();
    println!("{} {}", COUNTER.with(|c| c.get()), total);
}
"#;

/// Build the fixture once, and capture the argument list rustc handed the
/// linker.
///
/// The tests below relink from this list rather than driving `cargo` again,
/// because cargo will not relink inputs it considers unchanged — and forcing
/// it to (by touching the source) regenerates every object, since a debug
/// build embeds paths into DWARF. That changes the inputs, which is the one
/// thing an "unchanged relink" test must not do.
fn capture_link_arguments(scratch: &Scratch) -> Vec<String> {
    let blinker = workspace_binary("blinker");
    let records = scratch.join("records");
    let output = Command::new("cargo")
        .arg("build")
        .arg("--offline")
        .current_dir(scratch.path())
        .env(
            "RUSTFLAGS",
            format!(
                "-C linker={} -C link-arg=--blinker-internal \
                 -C link-arg=--blinker-record-invocation -C link-arg={}",
                blinker.display(),
                records.display()
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

    let record = std::fs::read_dir(&records)
        .expect("a records directory")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .expect("an invocation was recorded");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(record).expect("readable"))
            .expect("the record is JSON");
    // `replay_argv`, not `argv`: rustc passes a `symbols.o` from a temporary
    // directory it deletes as soon as the link returns, so the original
    // argument list stops working the moment the build finishes. Recording
    // archives every input aside and rewrites the arguments to point at the
    // copies, which is what makes a captured link replayable at all.
    json["replay_argv"]
        .as_array()
        .expect("replay_argv")
        .iter()
        .map(|v| v.as_str().expect("a string").to_string())
        .collect()
}

/// Link the captured arguments with the cache on, writing to `output`.
fn link(arguments: &[String], output: &Path, record: &Path) -> serde_json::Value {
    let mut arguments = arguments.to_vec();
    let at = arguments.iter().position(|a| a == "-o").expect("an -o");
    arguments[at + 1] = output.display().to_string();

    let status = Command::new(workspace_binary("blinker"))
        .args([
            "--blinker-internal",
            "--blinker-cache",
            "--blinker-json-diagnostics",
        ])
        .arg(record)
        .args(&arguments)
        .output()
        .expect("blinker runs");
    assert!(
        status.status.success(),
        "blinker failed:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    serde_json::from_str(&std::fs::read_to_string(record).expect("a record was written"))
        .expect("the record is JSON")
}

fn fixture(tag: &str) -> Scratch {
    let scratch = Scratch::dir(tag).expect("scratch");
    scratch
        .write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    scratch.write("src/main.rs", SOURCE).expect("source");
    scratch
}

/// Relink byte-identical inputs, and check the cache was actually used.
///
/// The inputs are deliberately unchanged, which is the case a silently-dead
/// cache passes and a working one does not. It reproduces finding 64: with the
/// zero-filled-section fix reverted, every object fails the byte copy and this
/// reports zero, while every C fixture keeps passing.
#[test]
fn a_rust_relink_reuses_every_object() {
    let scratch = fixture("rust-cache-reuse");
    let arguments = capture_link_arguments(&scratch);

    // Both links write to the same path, because the cache is keyed by output
    // binary — linking to a different name is a different program, and gets a
    // different cache. The first result is copied aside to compare against.
    let out = scratch.join("program");
    let cold = link(&arguments, &out, &scratch.join("cold.json"));
    assert_eq!(
        cold["counters"]["reused_inputs"], 0,
        "the first link is cold"
    );
    let first = std::fs::read(&out).expect("a binary");

    let warm = link(&arguments, &out, &scratch.join("warm.json"));
    let reused = warm["counters"]["reused_inputs"].as_u64().expect("a count");
    let objects = reused
        + warm["counters"]["changed_inputs"]
            .as_u64()
            .expect("a count");
    assert!(objects > 20, "the fixture should pull in libstd: {objects}");

    // Every object, not merely some. `reused > 0` is what a partly-broken
    // cache passes: with the zero-filled-section fix reverted, the objects
    // that happen not to touch `__bss` still reuse, and a weaker assertion
    // reports success while the cache does nothing useful.
    assert_eq!(
        reused, objects,
        "reused {reused} of {objects} objects on an unchanged relink"
    );
    assert_eq!(warm["mode"], "incremental");
    assert_eq!(
        first,
        std::fs::read(&out).expect("a binary"),
        "the reusing link produced a different binary"
    );
}

/// And the binary it produced must still run — including the thread-local and
/// the static table, which bring in the sections that broke the copy.
#[test]
fn the_reused_rust_binary_runs_correctly() {
    let scratch = fixture("rust-cache-run");
    let arguments = capture_link_arguments(&scratch);

    let out = scratch.join("program");
    link(&arguments, &out, &scratch.join("cold.json"));
    let warm = link(&arguments, &out, &scratch.join("warm.json"));
    assert!(
        warm["counters"]["reused_inputs"].as_u64().unwrap_or(0) > 0,
        "nothing was reused, so this proves nothing about reuse"
    );

    let run = Command::new(&out).output().expect("the program runs");
    assert!(run.status.success(), "exit {:?}", run.status.code());
    assert_eq!(String::from_utf8_lossy(&run.stdout), "3 6\n");
}

/// The reuse count must be reported even when it is zero.
///
/// Finding 64's actual lesson: a cache that reused nothing looked exactly like
/// one that worked, because the number existed and nothing printed it.
#[test]
fn the_reuse_count_is_reported_on_every_cached_link() {
    let scratch = fixture("rust-cache-report");
    let arguments = capture_link_arguments(&scratch);
    let cold = link(
        &arguments,
        &scratch.join("cold"),
        &scratch.join("cold.json"),
    );
    assert!(
        cold["counters"]["reused_inputs"].is_number(),
        "a cached link must report its hit rate, including zero"
    );
    assert!(cold["counters"]["changed_inputs"].is_number());
}
