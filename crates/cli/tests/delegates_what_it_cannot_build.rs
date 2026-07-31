//! Output kinds blinker does not produce must delegate, not fail.
//!
//! blinker emits a `MH_EXECUTE` image with an `LC_MAIN` entry point and
//! nothing else. `rustc` hands the same linker every crate in a workspace, and
//! a proc-macro crate is built as a `-dynamiclib` — so a linker that refuses
//! what it cannot do is unusable on any project that has one, which is most of
//! them. It was unusable on *this* one: building blinker with blinker stopped
//! at `serde_derive` with "entry symbol _main is not defined in any input".
//!
//! `--blinker-internal` therefore means "link internally where you can", and
//! the fallback carries a structured reason so the delegation is visible
//! rather than silent.

use blinker_test_support::{workspace_binary, Scratch};
use std::path::PathBuf;
use std::process::Command;

const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

fn compile(scratch: &Scratch, name: &str, code: &str) -> PathBuf {
    let source = scratch.write(name, code).expect("writable");
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
}

/// Run blinker, returning its exit status and the diagnostics record.
fn link(scratch: &Scratch, tag: &str, extra: &[&str], object: &PathBuf, out: &PathBuf) -> String {
    let record = scratch.join(format!("{tag}.json"));
    let status = Command::new(workspace_binary("blinker"))
        .arg("--blinker-internal")
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .args(extra)
        .arg("-o")
        .arg(out)
        .arg(object)
        .status()
        .expect("blinker runs");
    assert!(
        status.success(),
        "blinker failed on {tag}; it should have delegated"
    );
    std::fs::read_to_string(&record).expect("record written")
}

/// A dynamic library is what every proc-macro crate is, and blinker cannot
/// make one. It must produce the library anyway, by handing the job on.
#[test]
fn a_dynamic_library_is_delegated_and_still_produced() {
    let scratch = Scratch::dir("delegate-dylib").expect("scratch");
    let object = compile(&scratch, "lib.c", "int answer(void) { return 42; }\n");
    let out = scratch.join("libanswer.dylib");
    let json = link(&scratch, "dylib", &["-dynamiclib"], &object, &out);

    assert!(
        json.contains("\"delegated\""),
        "the record does not say it delegated:\n{json}"
    );
    assert!(
        json.contains("UnsupportedArgument") || json.contains("unsupported_argument"),
        "the delegation carries no structured reason:\n{json}"
    );
    // And the actual deliverable: the library exists and is a dylib.
    assert!(out.exists(), "no library was produced");
    let kind = Command::new("file").arg(&out).output().expect("file runs");
    let kind = String::from_utf8_lossy(&kind.stdout);
    assert!(
        kind.contains("dynamically linked shared library"),
        "not a dylib: {kind}"
    );
}

/// The other output kinds blinker does not emit, each named separately so the
/// diagnostic says which one arrived.
#[test]
fn every_unsupported_output_kind_delegates() {
    let scratch = Scratch::dir("delegate-kinds").expect("scratch");
    let object = compile(&scratch, "u.c", "int used(void) { return 1; }\n");

    for (tag, flag) in [("bundle", "-bundle"), ("partial", "-r")] {
        let out = scratch.join(format!("out-{tag}"));
        let json = link(&scratch, tag, &[flag], &object, &out);
        assert!(
            json.contains("\"delegated\""),
            "{flag} was not delegated:\n{json}"
        );
    }
}

/// And the control: an ordinary executable must **not** delegate, or the rule
/// above would be satisfied by a linker that delegates everything.
#[test]
fn an_ordinary_executable_is_still_linked_internally() {
    let scratch = Scratch::dir("delegate-control").expect("scratch");
    let object = compile(&scratch, "m.c", "int main(void) { return 3; }\n");
    let out = scratch.join("program");
    let json = link(&scratch, "exe", &[], &object, &out);

    assert!(
        !json.contains("\"delegated\""),
        "an ordinary executable was delegated:\n{json}"
    );
    let status = Command::new(&out).status().expect("the program runs");
    assert_eq!(status.code(), Some(3));
}
