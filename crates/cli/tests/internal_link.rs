//! The driver linking for itself rather than delegating.
//!
//! `--blinker-internal` is the switch that makes blinker a linker instead of a
//! recording wrapper. These check that the switch is real: the output must
//! run, and the record must say `cold` rather than `delegated`.

use blinker_test_support::{workspace_binary, Scratch};
use std::process::Command;

fn compile(scratch: &Scratch, name: &str, code: &str) -> std::path::PathBuf {
    let source = scratch.write(name, code).expect("writable");
    let object = scratch.join(format!("{name}.o"));
    let status = Command::new("cc")
        .args(["-arch", "arm64", "-mmacosx-version-min=11.0", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    assert!(status.success(), "cc failed");
    object
}

#[test]
fn the_driver_links_internally_and_the_program_runs() {
    let scratch = Scratch::dir("driver-internal").expect("scratch");
    let object = compile(
        &scratch,
        "main.c",
        "#include <stdio.h>\nint main(void){ printf(\"driven\\n\"); return 3; }\n",
    );
    let output = scratch.join("program");

    let status = Command::new(workspace_binary("blinker"))
        .arg("--blinker-internal")
        .arg("-o")
        .arg(&output)
        .arg(&object)
        .status()
        .expect("blinker runs");
    assert!(status.success(), "blinker failed to link");

    let run = Command::new(&output).output().expect("program runs");
    assert_eq!(run.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "driven\n");
}

/// The record must distinguish an internal link from a delegated one.
#[test]
fn an_internal_link_is_recorded_as_cold_not_delegated() {
    let scratch = Scratch::dir("driver-record").expect("scratch");
    let object = compile(&scratch, "m.c", "int main(void){ return 0; }\n");
    let output = scratch.join("program");
    let record = scratch.join("record.json");

    let status = Command::new(workspace_binary("blinker"))
        .arg("--blinker-internal")
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .arg("-o")
        .arg(&output)
        .arg(&object)
        .status()
        .expect("blinker runs");
    assert!(status.success());

    let json = std::fs::read_to_string(&record).expect("record written");
    assert!(
        json.contains("\"cold\""),
        "record does not say cold:\n{json}"
    );
    assert!(
        !json.contains("\"delegated\""),
        "record still says delegated:\n{json}"
    );
}

/// Without the flag, behaviour is unchanged: delegate to the system linker.
#[test]
fn delegation_remains_the_default() {
    let scratch = Scratch::dir("driver-default").expect("scratch");
    let object = compile(&scratch, "m.c", "int main(void){ return 5; }\n");
    let output = scratch.join("program");
    let record = scratch.join("record.json");

    let status = Command::new(workspace_binary("blinker"))
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .arg("-o")
        .arg(&output)
        .arg(&object)
        .status()
        .expect("blinker runs");
    assert!(status.success());

    let json = std::fs::read_to_string(&record).expect("record written");
    assert!(json.contains("\"delegated\""), "default changed:\n{json}");
}
