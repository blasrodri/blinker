//! A C++ `throw` must reach its `catch`.
//!
//! Two independent faults kept it from doing so, and either alone is enough to
//! terminate the program (finding 239):
//!
//! 1. the personality routine was dropped from `__unwind_info`, because it was
//!    read only after resolving the record's address and an *imported*
//!    personality has none. `___gxx_personality_v0` lives in libc++abi;
//!    `rust_eh_personality` is linked into the program, which is the whole
//!    reason Rust panics worked and C++ exceptions did not;
//! 2. the first-level index sentinel was the last function's *start* rather
//!    than its end, so the last function in the image had an empty range and no
//!    unwind info at all. In a one-function program that is the whole table.
//!
//! These tests are behavioural on purpose. The unwind tables can be inspected,
//! and every field can look right while the unwinder still finds nothing —
//! which is exactly what happened. Only running the program says otherwise.

use blinker_test_support::{blinker, Scratch};
use std::path::PathBuf;
use std::process::Command;

fn compile(scratch: &Scratch, name: &str, code: &str) -> Option<PathBuf> {
    let source = scratch.write(name, code).expect("writable");
    let object = scratch.join(format!("{name}.o"));
    let status = Command::new("c++")
        .args(["-arch", "arm64", "-mmacosx-version-min=11.0", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("c++ runs");
    status.success().then_some(object)
}

/// Link internally — a delegated link would prove nothing — and run.
fn link_and_run(scratch: &Scratch, tag: &str, object: &PathBuf) -> i32 {
    let out = scratch.join(format!("program-{tag}"));
    let record = scratch.join(format!("{tag}.json"));
    let status = blinker()
        .arg("--blinker-internal")
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .arg("-o")
        .arg(&out)
        .arg(object)
        .args(["-lc++", "-lSystem"])
        .status()
        .expect("blinker runs");
    assert!(status.success(), "blinker failed to link {tag}");
    let json = std::fs::read_to_string(&record).expect("record written");
    assert!(
        !json.contains("\"delegated\""),
        "the link was delegated, so it says nothing about blinker:\n{json}"
    );
    Command::new(&out)
        .status()
        .expect("the program runs")
        .code()
        .unwrap_or(-1)
}

/// The smallest possible case, and the one that failed hardest: a single
/// function, so the sentinel bug leaves the table covering nothing.
#[test]
fn a_throw_reaches_its_catch() {
    let scratch = Scratch::dir("unwind-throw").expect("scratch");
    let Some(object) = compile(
        &scratch,
        "tiny.cpp",
        "int main() { try { throw 42; } catch (int v) { return v == 42 ? 0 : 1; } return 2; }\n",
    ) else {
        return; // no C++ toolchain: nothing to say
    };
    assert_eq!(
        link_and_run(&scratch, "throw", &object),
        0,
        "the exception was not caught — the unwinder found no personality, or no entry"
    );
}

/// Through a deep stack, so the unwinder walks frames rather than one, and
/// across a destructor, which is what makes unwinding observable.
#[test]
fn an_exception_unwinds_through_frames_and_runs_destructors() {
    let scratch = Scratch::dir("unwind-frames").expect("scratch");
    let Some(object) = compile(
        &scratch,
        "frames.cpp",
        r#"
#include <stdexcept>
static int unwound = 0;
struct Marker { ~Marker() { ++unwound; } };
static int descend(int n) {
  Marker marker;
  if (n == 0) { throw std::runtime_error("bottom"); }
  return descend(n - 1) + 1;
}
int main() {
  try { descend(20); } catch (const std::exception &) {
    /* 21 frames each held a Marker. */
    return unwound == 21 ? 0 : 3;
  }
  return 1;
}
"#,
    ) else {
        return;
    };
    assert_eq!(
        link_and_run(&scratch, "frames", &object),
        0,
        "unwinding did not run every destructor on the way out"
    );
}

/// An exception thrown past a function that has *no* landing pad of its own,
/// so the unwinder has to keep walking rather than stop at the first frame.
#[test]
fn an_exception_passes_through_a_frame_that_does_not_catch() {
    let scratch = Scratch::dir("unwind-through").expect("scratch");
    let Some(object) = compile(
        &scratch,
        "through.cpp",
        r#"
static void innermost() { throw 7; }
static void middle() { innermost(); }
int main() { try { middle(); } catch (int v) { return v == 7 ? 0 : 1; } return 2; }
"#,
    ) else {
        return;
    };
    assert_eq!(
        link_and_run(&scratch, "through", &object),
        0,
        "an exception did not pass through an uninterested frame"
    );
}
