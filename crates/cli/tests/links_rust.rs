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
/// How a run finished.
///
/// A process killed by a signal has **no exit code** — `ExitStatus::code()`
/// returns `None`, and the familiar 134 a shell reports is its own encoding of
/// `128 + SIGABRT`. An aborting panic is a signal death, so the two have to be
/// distinguished rather than conflated.
#[derive(Debug, PartialEq, Eq)]
enum RunResult {
    Exited(i32),
    Signalled(i32),
}

fn build_and_run(tag: &str, main_rs: &str) -> (RunResult, String, String) {
    build_and_run_with(tag, main_rs, "")
}

/// As above, with extra `RUSTFLAGS`.
fn build_and_run_with(tag: &str, main_rs: &str, extra_flags: &str) -> (RunResult, String, String) {
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
        .env("BLINKER_NO_DAEMON", "1")
        .arg("build")
        .current_dir(scratch.path())
        // Offline and target-local: the fixture has no dependencies, and a
        // test that reaches the network is a test that fails on a train.
        .arg("--offline")
        .env(
            "RUSTFLAGS",
            format!(
                "-C linker={} -C link-arg=--blinker-internal {extra_flags}",
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
    let result = match run.status.code() {
        Some(code) => RunResult::Exited(code),
        None => {
            use std::os::unix::process::ExitStatusExt;
            RunResult::Signalled(run.status.signal().expect("exited or signalled"))
        }
    };
    (
        result,
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
    assert_eq!(status, RunResult::Exited(11), "stderr: {stderr}");
}

/// `println!` brings in formatting, stdout locking and thread-locals.
#[test]
fn a_rust_program_that_prints_produces_the_right_output() {
    let (status, stdout, stderr) = build_and_run(
        "rust-print",
        "fn main() { println!(\"rust via blinker\"); std::process::exit(7); }\n",
    );
    assert_eq!(status, RunResult::Exited(7), "stderr: {stderr}");
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
    assert_eq!(status, RunResult::Exited(8), "stderr: {stderr}");
    assert!(stdout.contains("8 distinct"), "stdout: {stdout}");
    assert!(stdout.contains("joined: 1-2-3-4-5"), "stdout: {stdout}");
}

/// A panic under `-C panic=abort` must report and abort, exactly as ld64's
/// output does.
///
/// The panic path is thick: it takes a thread-local panic count, formats a
/// message, and calls the panic hook. Reaching the abort means the
/// thread-local machinery — descriptors, the pointer table, its rebases, and
/// the block offsets — is all correct.
///
/// `panic=unwind` — the default, and the harder case — is covered by the tests
/// below it.
#[test]
fn a_panicking_program_reports_and_aborts_under_panic_abort() {
    let (status, _, stderr) = build_and_run_with(
        "rust-panic-abort",
        "fn main() { println!(\"before\"); panic!(\"boom\"); }\n",
        "-C panic=abort",
    );

    // SIGABRT (6), as an aborting panic should — not an exit code.
    assert_eq!(status, RunResult::Signalled(6), "stderr: {stderr}");
    assert!(
        stderr.contains("panicked at") && stderr.contains("boom"),
        "the panic message did not reach stderr: {stderr}"
    );
}

/// A caught panic under the **default** `panic=unwind`.
///
/// This is the deepest thing the linker is asked to get right, because
/// unwinding reads four separate structures the linker itself synthesises —
/// `__unwind_info`'s two-level index, the `__eh_frame` CIE/FDE chain, each
/// CIE's personality pointer, and each FDE's LSDA — and a single wrong word in
/// any of them ends the process instead of returning here.
///
/// It failed for a long time with `failed to initiate panic, error 3`
/// (`_URC_FATAL_PHASE1_ERROR`) while all four structures decoded correctly, and
/// the cause was none of them: `SUBTRACTOR` pairs dropped their addend. Mach-O
/// stores addends **in the bytes being patched**, not in the relocation, so
/// every LSDA came out measured from the start of its object's `__eh_frame`
/// contribution rather than from the field, and landed outside
/// `__gcc_except_tab`. See FINDINGS.md 58.
#[test]
fn a_panic_unwinds_and_is_caught_under_the_default_panic_strategy() {
    let source = r#"
struct Noisy(&'static str);
impl Drop for Noisy {
    fn drop(&mut self) { println!("drop {}", self.0); }
}

fn deep(n: u32) {
    let _guard = Noisy(if n == 0 { "zero" } else { "deep" });
    if n == 0 { panic!("boom"); }
    deep(n - 1);
}

fn main() {
    let caught = std::panic::catch_unwind(|| deep(3));
    println!("caught: {}", caught.is_err());
    let payload = caught.unwrap_err().downcast::<&str>().map(|b| *b).unwrap_or("?");
    println!("payload: {payload}");
    std::process::exit(21);
}
"#;
    let (status, stdout, stderr) = build_and_run("rust-panic-unwind", source);

    // Exiting at all is the assertion: a broken LSDA aborts in phase 1.
    assert_eq!(status, RunResult::Exited(21), "stderr: {stderr}");
    assert!(
        !stderr.contains("failed to initiate panic"),
        "the unwinder could not walk the tables: {stderr}"
    );
    assert!(stdout.contains("caught: true"), "stdout: {stdout}");
    assert!(stdout.contains("payload: boom"), "stdout: {stdout}");

    // Each frame's cleanup runs exactly once, innermost first. A plausible
    // near-miss — landing pads found but the wrong ones — shows up here and
    // not in the exit status.
    let drops: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("drop "))
        .collect();
    assert_eq!(drops, ["zero", "deep", "deep", "deep"], "stdout: {stdout}");
}

/// An *uncaught* panic under `panic=unwind` exits 101 rather than aborting.
///
/// The distinction matters: `panic=abort` dies by SIGABRT, and so does an
/// unwinder that cannot find its tables. Only a complete unwind reaches the
/// runtime's ordinary exit path.
#[test]
fn an_uncaught_panic_exits_101_rather_than_aborting() {
    let (status, _, stderr) = build_and_run(
        "rust-panic-unwind-uncaught",
        "fn main() { println!(\"before\"); panic!(\"boom\"); }\n",
    );
    assert_eq!(status, RunResult::Exited(101), "stderr: {stderr}");
    assert!(
        stderr.contains("panicked at") && stderr.contains("boom"),
        "the panic message did not reach stderr: {stderr}"
    );
}
