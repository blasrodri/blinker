//! A panic backtrace must name the function each frame is actually in.
//!
//! blinker dropped every local symbol from the output. The comment justifying
//! it said locals are "invisible outside their object by definition, and the
//! only consumer that would want them is a debugger" — both halves are wrong.
//! Most Rust functions are local: `deep` and `middle` below are private to the
//! crate, and so is nearly everything monomorphised out of `std`. And the
//! consumer is not a debugger, it is the panicking program itself, which
//! symbolicates its own backtrace from its own symbol table.
//!
//! The failure is not a missing name. A symbolicator maps an address to the
//! nearest symbol *at or below* it, so removing the locals does not remove the
//! answer — it moves the answer to whatever global happens to precede the
//! frame. Same fixture, both linkers:
//!
//! ```text
//!   ld-prime                          blinker
//!   1: hello::deep                    5: hello::main
//!   2: hello::deep                    6: hello::main
//!   3: hello::deep                    7: hello::main
//!   4: hello::deep                    8: std::rt::lang_start::<()>
//!   5: hello::middle                  ...
//! ```
//!
//! Four frames in `deep` and one in `middle` are reported as three in `main`.
//! Nothing is marked uncertain; the backtrace is well-formed, plausible, and
//! names functions the program was never in. That is the silent-mislink
//! failure mode this project exists to avoid, reached through the symbol table
//! rather than through a relocation.
//!
//! These tests build a real crate, so they are slow. A C fixture cannot
//! replace them: the property is about what a *Rust* backtrace prints, which
//! needs the Rust runtime's symbolicator.

use blinker_test_support::{workspace_binary, Scratch};
use std::process::Command;

/// A fixture built by one linker, run once, with backtraces enabled.
struct Built {
    stderr: String,
    _scratch: Scratch,
}

/// The program: a private recursive function that panics at the bottom,
/// reached through a second private function.
///
/// `deep` recurses so that a mis-symbolicated backtrace collapses *several*
/// distinct frames onto one wrong name, and `middle` is `#[inline(never)]` so
/// there is a second local frame that cannot be explained away as inlining.
/// Neither is `pub`, so both are local symbols — which is the whole point.
const RECURSES_AND_PANICS: &str = r#"
fn deep(n: u32) -> u32 {
    if n == 0 { panic!("bottom") } else { deep(n - 1) }
}

#[inline(never)]
fn middle() -> u32 { deep(3) }

fn main() { println!("{}", middle()); }
"#;

/// Build the fixture and run it. `internal` chooses the linker: blinker's own
/// path, or the default one cargo would have used.
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
        let blinker = workspace_binary("blinker");
        build.env(
            "RUSTFLAGS",
            format!(
                "-C linker={} -C link-arg=--blinker-internal",
                blinker.display()
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

/// How many backtrace frames name a given function.
fn frames_naming(stderr: &str, function: &str) -> usize {
    let wanted = format!("fixture::{function}");
    stderr
        .lines()
        .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        .filter(|line| line.contains(&wanted))
        .count()
}

/// The property, stated positively: the frames the program really was in are
/// the frames the backtrace names.
#[test]
fn a_panic_backtrace_names_the_private_functions_it_unwound_through() {
    let built = build("backtrace-internal", true);
    assert!(
        built.stderr.contains("stack backtrace:"),
        "no backtrace was printed at all:\n{}",
        built.stderr
    );
    assert!(
        frames_naming(&built.stderr, "deep") >= 2,
        "the recursion through `deep` is not in the backtrace:\n{}",
        built.stderr
    );
    assert!(
        frames_naming(&built.stderr, "middle") >= 1,
        "`middle` is not in the backtrace:\n{}",
        built.stderr
    );
}

/// And stated negatively, which is the half that catches the real bug: `main`
/// called `middle` once and never recursed, so `main` occupies exactly one
/// frame. Several frames naming `main` means addresses inside `deep` were
/// attributed to the nearest global below them.
///
/// Without this, dropping the locals again and emitting one bogus `deep`
/// symbol would satisfy the test above.
#[test]
fn no_frame_is_attributed_to_a_function_it_was_not_in() {
    let built = build("backtrace-blame", true);
    let blamed = frames_naming(&built.stderr, "main");
    assert!(
        blamed <= 1,
        "{blamed} frames name `main`, which was entered once — \
         frames from `deep` are being attributed to it:\n{}",
        built.stderr
    );
}

/// The control. Both assertions above are properties of the *fixture* as much
/// as of the linker: if rustc inlined `deep` away, or the runtime stopped
/// symbolicating, they would fail for reasons that have nothing to do with
/// blinker. Building the same source with the system linker says whether the
/// expectation is reachable at all on this machine.
///
/// It is a control, not a duplicate: it must pass both before and after the
/// fix, and it did pass while the two tests above failed.
#[test]
fn the_system_linker_produces_the_backtrace_this_expects() {
    let built = build("backtrace-control", false);
    assert!(
        frames_naming(&built.stderr, "deep") >= 2
            && frames_naming(&built.stderr, "middle") >= 1
            && frames_naming(&built.stderr, "main") <= 1,
        "the expectation is not reachable on this machine even with the \
         system linker; the fixture, not blinker, is what these tests are \
         measuring:\n{}",
        built.stderr
    );
}
