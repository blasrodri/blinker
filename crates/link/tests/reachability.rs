//! What the dead-stripping analysis can and cannot prove.
//!
//! The analysis reports what *would* be removed and removes nothing, so these
//! tests check a prediction rather than a binary. That makes the direction of
//! error the thing to pin down: reporting live code as dead would license a
//! stripper to delete something a program needs, while the reverse only leaves
//! bytes behind. Every case below is written to fail if the analysis becomes
//! optimistic.

use blinker_link::{reachability_report, LinkRequest};
use blinker_test_support::Scratch;
use std::path::PathBuf;
use std::process::Command;

const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

fn compile(scratch: &Scratch, sources: &[(&str, &str)]) -> Vec<PathBuf> {
    sources
        .iter()
        .map(|(name, code)| {
            let source = scratch.write(name, *code).expect("writable");
            let object = scratch.join(format!("{name}.o"));
            let status = Command::new("cc")
                .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-c"])
                .arg(&source)
                .arg("-o")
                .arg(&object)
                .status()
                .expect("cc runs");
            assert!(status.success(), "cc failed to compile {name}");
            object
        })
        .collect()
}

fn analyse(tag: &str, sources: &[(&str, &str)]) -> blinker_link::reachability::Report {
    let scratch = Scratch::dir(tag).expect("scratch");
    let objects = compile(&scratch, sources);
    reachability_report(&LinkRequest::new(objects)).expect("the inputs parse")
}

/// A function nothing calls is dead, and the analysis has to say so — this is
/// the entire point, and the case a too-conservative model gets wrong.
#[test]
fn an_uncalled_function_is_reported_dead() {
    let source = r#"
int reached(int n) { return n + 1; }
int never_called(int n) { return n * 7 + 3; }
int main(void) { return reached(1); }
"#;
    let report = analyse("reach-dead", &[("c.c", source)]);
    assert!(
        report.dead_bytes() > 0,
        "nothing was found dead: {report:?}"
    );
    assert!(
        report.live_atoms < report.total_atoms,
        "every atom was live: {report:?}"
    );
}

/// And a function that *is* called must never be reported dead. This is the
/// direction that would produce a broken binary.
#[test]
fn a_called_function_is_always_live() {
    let source = r#"
int deep(int n) { return n * 2; }
int middle(int n) { return deep(n) + 1; }
int main(void) { return middle(3); }
"#;
    let report = analyse("reach-live", &[("c.c", source)]);
    // main, middle and deep are all reachable; only compiler-emitted extras
    // may be dead, so the live set must cover at least those three.
    assert!(report.live_atoms >= 3, "{report:?}");
}

/// Reachability is transitive across objects, not just within one.
#[test]
fn liveness_follows_calls_between_objects() {
    let main = "int helper(int n);\nint main(void) { return helper(2); }\n";
    let other = r#"
int helper(int n) { return n + 40; }
int unrelated(int n) { return n - 1; }
"#;
    let report = analyse("reach-cross", &[("a.c", main), ("b.c", other)]);
    assert!(report.live_atoms >= 2, "helper was not reached: {report:?}");
    assert!(report.dead_bytes() > 0, "unrelated survived: {report:?}");
}

/// A function reached only through a pointer stored in data has no call site,
/// and must still be live. Nothing in `__text` refers to it.
#[test]
fn a_function_referenced_only_from_data_is_live() {
    let source = r#"
int via_pointer(int n) { return n + 5; }
int (*table[])(int) = { via_pointer };
int main(void) { return table[0](1); }
"#;
    let report = analyse("reach-pointer", &[("c.c", source)]);
    // If the data root were missed, `via_pointer` would be dead and the
    // program would jump into whatever replaced it.
    assert!(report.live_atoms >= 2, "{report:?}");
}

/// Unwind and exception tables name every function in an object, and treating
/// them as roots makes the analysis say nothing at all.
///
/// **This does not reproduce that bug.** Restoring metadata as a root leaves
/// it passing. The reason is specific: `__compact_unwind` refers to its
/// function through a *section* relocation with an inline addend, so rooting
/// from symbol targets never sees it. `__eh_frame` uses symbol-named
/// `SUBTRACTOR` pairs (finding 56) and does trigger it — and macOS clang emits
/// `__eh_frame` only where compact unwind cannot describe a frame, which no
/// small C fixture reaches.
///
/// The evidence for the bug is the real Rust link, where it moved the result
/// from 2274 of 2274 atoms live to 938 (finding 71). Kept for the weaker
/// property it does check, and labelled rather than trusted.
#[test]
fn unwind_metadata_does_not_keep_its_function_alive() {
    // Every function calls `printf`, so each needs a frame and gets its own
    // `__compact_unwind` record — the closest a C fixture gets.
    let source = r#"
#include <stdio.h>
__attribute__((noinline)) int never_called(int n) { printf("%d", n); return n * 9; }
__attribute__((noinline)) int also_dead(int n) { printf("%d", n); return n - 2; }
__attribute__((noinline)) int reached(int n) { printf("%d", n); return n + 1; }
int main(void) { return reached(1); }
"#;
    let report = analyse("reach-unwind", &[("c.c", source)]);
    assert!(
        report.dead_bytes() > 0,
        "unwind tables kept an uncalled function alive: {report:?}"
    );
    // Two of the four functions are unreachable, so a model that roots from
    // unwind data cannot get near this.
    assert!(
        report.live_atoms * 2 <= report.total_atoms + 1,
        "too much survived; metadata is probably rooting: {report:?}"
    );
}

/// The report has to be internally consistent, or the number it prints is not
/// a number about anything.
#[test]
fn the_report_totals_agree() {
    let source = "int f(int n) { return n; }\nint main(void) { return f(1); }\n";
    let report = analyse("reach-totals", &[("c.c", source)]);
    assert!(report.live_atoms <= report.total_atoms);
    assert!(report.live_bytes <= report.total_bytes);
    assert_eq!(report.dead_bytes(), report.total_bytes - report.live_bytes);
    assert!(report.total_atoms > 0, "no atoms were found at all");
}
