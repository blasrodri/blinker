//! Dead-stripping a real Rust link.
//!
//! `crates/link/tests/dead_strips.rs` covers the same feature with C fixtures,
//! and it is not sufficient. macOS clang describes a C function with a compact
//! unwind record and nothing else, so a C fixture never produces the section
//! that broke: `__eh_frame`.
//!
//! What broke there is worth stating, because it is the one thing in a Mach-O
//! link that stripping cannot treat as opaque bytes. An FDE's second word is
//! the distance *backwards* to the CIE describing it — computed by the
//! assembler, covered by no relocation. Compacting the section moves records
//! apart, and every one of those distances is then the distance they used to
//! be. The binary links, runs, prints, and segfaults the moment it unwinds.
//!
//! These tests are slow, because each builds a Rust crate. They are worth it:
//! the failure they catch is invisible to every other test in the workspace.

use blinker_test_support::{workspace_binary, Scratch};
use std::process::Command;

/// A built fixture: where the binary is, and what it did when run.
struct Built {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    _scratch: Scratch,
}

fn build(tag: &str, main_rs: &str) -> Built {
    let scratch = Scratch::dir(tag).expect("scratch");
    scratch
        .write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    scratch.write("src/main.rs", main_rs).expect("source");

    let blinker = workspace_binary("blinker");
    // Passed explicitly even though rustc already does, so the fixture states
    // what it is testing rather than depending on a default.
    let strip_flag = " -C link-arg=-Wl,-dead_strip";
    let output = Command::new("cargo")
        .arg("build")
        .arg("--offline")
        .current_dir(scratch.path())
        .env(
            "RUSTFLAGS",
            format!(
                "-C linker={} -C link-arg=--blinker-internal{strip_flag}",
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
    Built {
        code: run.status.code(),
        stdout: String::from_utf8_lossy(&run.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
        _scratch: scratch,
    }
}

const PANICS: &str = "fn main() { println!(\"before\"); panic!(\"boom\"); }\n";

/// The test the `__eh_frame` bug fails.
///
/// An uncaught panic under the default `panic=unwind` exits **101**. A binary
/// whose FDEs point at the wrong CIE gets as far as printing the panic
/// message and then dies of SIGSEGV (139) inside the unwinder — so the exit
/// status is the whole assertion, and `stderr` alone would not notice.
#[test]
fn an_uncaught_panic_still_exits_101_after_stripping() {
    let built = build("strip-rust-panic", PANICS);
    assert!(
        built.stderr.contains("panicked at") && built.stderr.contains("boom"),
        "the panic message did not reach stderr: {}",
        built.stderr
    );
    assert_eq!(
        built.code,
        Some(101),
        "unwinding a stripped binary did not reach the runtime's exit\nstderr: {}",
        built.stderr
    );
}

/// A caught panic runs destructors on the way out, which needs the exception
/// tables as well as the frame descriptions.
#[test]
fn a_caught_panic_still_runs_destructors_after_stripping() {
    let source = r#"
struct Noisy(u32);
impl Drop for Noisy {
    fn drop(&mut self) { println!("drop {}", self.0); }
}
fn deep(n: u32) {
    let _guard = Noisy(n);
    if n == 0 { panic!("boom"); }
    deep(n - 1);
}
fn main() {
    let caught = std::panic::catch_unwind(|| deep(3));
    println!("caught {}", caught.is_err());
}
"#;
    let built = build("strip-rust-catch", source);
    assert_eq!(built.code, Some(0), "stderr: {}", built.stderr);
    assert_eq!(
        built.stdout, "drop 0\ndrop 1\ndrop 2\ndrop 3\ncaught true\n",
        "the unwinder did not walk every frame"
    );
}

/// Stripping must not change what the program does.
#[test]
fn a_stripped_rust_program_behaves_identically() {
    let source = r#"
use std::collections::HashMap;
fn main() {
    let mut counts = HashMap::new();
    for word in "a b a c b a".split_whitespace() {
        *counts.entry(word).or_insert(0u32) += 1;
    }
    let mut keys: Vec<_> = counts.keys().copied().collect();
    keys.sort();
    for key in keys { println!("{key} {}", counts[key]); }
}
"#;
    let stripped = build("strip-rust-same", source);
    assert_eq!(stripped.code, Some(0), "stderr: {}", stripped.stderr);
    assert_eq!(stripped.stdout, "a 3\nb 2\nc 1\n");
}

/// Nothing may have to be revived.
///
/// The propagation is supposed to guarantee that no live atom refers to a dead
/// one, and a verification pass at the end checks it and *repairs* what it
/// finds. That repair is what makes an incomplete model produce a correct
/// binary — and therefore what would hide it. The count is the only evidence,
/// so it is asserted rather than watched.
///
/// A Rust link is where this can say anything: it is the only fixture with
/// `__eh_frame`, exception tables, thread-locals and 47 objects' worth of
/// cross-references.
#[test]
fn the_reachability_model_needs_no_repairs_on_a_rust_link() {
    let scratch = Scratch::dir("strip-rust-revived").expect("scratch");
    scratch
        .write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    scratch
        .write(
            "src/main.rs",
            "use std::collections::HashMap;\n\
             fn main() {\n\
             \x20   let mut m = HashMap::new();\n\
             \x20   m.insert(\"k\", vec![1u8, 2, 3]);\n\
             \x20   println!(\"{:?}\", m.get(\"k\"));\n\
             }\n",
        )
        .expect("source");

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

    // The counter has to exist, or asserting it is zero proves nothing: an
    // absent field and a zero one both read as "no repairs" to a careless
    // comparison.
    assert!(
        json["counters"]["revived_atoms"].is_number(),
        "the link did not report a dead-strip at all: {}",
        json["counters"]
    );
    assert_eq!(
        json["counters"]["revived_atoms"], 0,
        "the reachability model missed edges the verification pass had to repair"
    );
    assert!(
        json["counters"]["stripped_bytes"].as_u64().unwrap_or(0) > 100_000,
        "barely anything was stripped: {}",
        json["counters"]
    );
}

/// And it must actually remove something, or the tests above are checking a
/// linker that quietly did nothing.
///
/// The control cannot come from building without the flag: **rustc passes
/// `-Wl,-dead_strip` on every macOS link**, so "build it twice, once with the
/// flag" produces two stripped binaries and an assertion that cannot fail. A
/// first version of this test did exactly that and reported 224492 against
/// 224492.
///
/// The honest control is the same argument list replayed with `-dead_strip`
/// taken out, which is the only way to reach a Rust link that was not
/// stripped.
///
/// The threshold is deliberately coarse — half of `__text` — because the exact
/// figure moves with the toolchain. What it rules out is a strip that trimmed
/// only the edges: a Rust binary carries the whole of `libstd`'s reachable
/// closure, and the fixture uses almost none of it.
#[test]
fn stripping_removes_most_of_a_rust_binarys_text() {
    let scratch = Scratch::dir("strip-rust-size").expect("scratch");
    scratch
        .write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    scratch
        .write("src/main.rs", "fn main() { println!(\"hello\"); }\n")
        .expect("source");

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
    // `replay_argv`, not `argv`: rustc hands the linker a `symbols.o` from a
    // temporary directory it deletes as soon as the link returns.
    let arguments: Vec<String> = json["replay_argv"]
        .as_array()
        .expect("replay_argv")
        .iter()
        .map(|v| v.as_str().expect("a string").to_string())
        .collect();
    assert!(
        arguments.iter().any(|a| a.contains("-dead_strip")),
        "rustc no longer passes -dead_strip, so this test has no control"
    );

    let relink = |name: &str, strip: bool| -> u64 {
        let out = scratch.join(name);
        let mut arguments = arguments.clone();
        if !strip {
            arguments.retain(|a| !a.contains("-dead_strip"));
        }
        let at = arguments.iter().position(|a| a == "-o").expect("an -o");
        arguments[at + 1] = out.display().to_string();
        let status = Command::new(&blinker)
            .arg("--blinker-internal")
            .args(&arguments)
            .output()
            .expect("blinker runs");
        assert!(
            status.status.success(),
            "blinker failed:\n{}",
            String::from_utf8_lossy(&status.stderr)
        );
        let size = Command::new("size")
            .arg("-m")
            .arg(&out)
            .output()
            .expect("size runs");
        String::from_utf8_lossy(&size.stdout)
            .lines()
            .find(|line| line.contains("Section __text:"))
            .and_then(|line| line.rsplit(' ').next()?.parse().ok())
            .expect("a __text section")
    };

    let small = relink("stripped", true);
    let large = relink("whole", false);
    assert!(
        small * 2 < large,
        "__text barely moved: {small} stripped against {large} whole"
    );
}
