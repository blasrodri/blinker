//! End-to-end tests: real `cargo build` invocations driven through the real
//! blinker binary.
//!
//! These are the tests that actually establish the M0 acceptance criteria.
//! Unit tests over synthetic argv can only confirm we handle the arguments we
//! *imagined*; only a genuine build confirms we handle the ones rustc emits.
//!
//! They are correspondingly slow (each spawns a full `cargo build`), so the
//! set is kept small and each test earns its place by checking something no
//! unit test can.

use blinker_test_support::{workspace_binary, RustFixture, MINIMAL_MAIN, MULTI_MODULE_MAIN};
use std::path::Path;

fn blinker() -> std::path::PathBuf {
    workspace_binary("blinker")
}

/// Ask blinker to record into the fixture's recording directory.
fn record_into(dir: &Path) -> Vec<String> {
    // The inline `=` spelling keeps this to a single `-C link-arg`, so rustc
    // cannot separate the option from its value.
    vec![format!("--blinker-record-invocation={}", dir.display())]
}

#[test]
fn minimal_rust_binary_builds_and_runs_through_blinker() {
    let fixture = RustFixture::binary("minimal", MINIMAL_MAIN).unwrap();
    let build = fixture
        .build_with_linker(&blinker(), &record_into(&fixture.recording_dir()))
        .unwrap();

    assert!(
        build.success,
        "build failed through blinker\nstderr:\n{}",
        build.stderr
    );

    // Building is necessary but not sufficient — the produced executable must
    // actually run. A linker that emits a well-formed but non-functional
    // binary would pass a build-only check.
    let exe = fixture
        .path()
        .join("target/aarch64-apple-darwin/debug")
        .join(fixture.name());
    let output = std::process::Command::new(&exe).output().unwrap();
    assert!(output.status.success(), "produced binary did not run");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "fixture ok");
}

#[test]
fn multi_module_binary_with_tls_and_panic_path_builds_and_runs() {
    let fixture = RustFixture::binary("multimod", MULTI_MODULE_MAIN).unwrap();
    let build = fixture.build_with_linker(&blinker(), &[]).unwrap();
    assert!(
        build.success,
        "build failed through blinker\nstderr:\n{}",
        build.stderr
    );

    let exe = fixture
        .path()
        .join("target/aarch64-apple-darwin/debug")
        .join(fixture.name());

    let ok = std::process::Command::new(&exe).output().unwrap();
    assert!(ok.status.success());
    assert!(String::from_utf8_lossy(&ok.stdout).starts_with("alpha beta:"));

    // Panic unwinding must reach the runtime's handler and produce the usual
    // message — this is the M0 smoke test for the unwind path that M3 will
    // properly validate.
    let panicked = std::process::Command::new(&exe)
        .env("FIXTURE_SHOULD_PANIC", "1")
        .output()
        .unwrap();
    assert!(!panicked.status.success());
    assert!(String::from_utf8_lossy(&panicked.stderr).contains("requested panic"));
}

/// The central M0 deliverable: a recorded corpus entry describing a real link.
#[test]
fn recorded_invocation_captures_the_real_link_configuration() {
    let fixture = RustFixture::binary("record", MINIMAL_MAIN).unwrap();
    let build = fixture
        .build_with_linker(&blinker(), &record_into(&fixture.recording_dir()))
        .unwrap();
    assert!(build.success, "stderr:\n{}", build.stderr);

    let record = build.single_recording();

    assert_eq!(record["mode"], "delegated");
    assert_eq!(record["exit_code"], 0);
    assert_eq!(record["arch"], "arm64");
    assert!(
        record["deployment_target"]
            .as_str()
            .is_some_and(|s| s.starts_with("macosx")),
        "unexpected deployment target: {:?}",
        record["deployment_target"]
    );
    assert!(record["output_path"]
        .as_str()
        .is_some_and(|s| s.contains(fixture.name())));

    // A real link reads many inputs; zero would mean classification silently
    // failed to recognize the positional arguments.
    let inputs = record["inputs"].as_array().unwrap();
    assert!(
        inputs.len() > 5,
        "expected several inputs, found {}",
        inputs.len()
    );
    assert!(record["counters"]["bytes_read"].as_u64().unwrap() > 0);

    // Every input rustc named must exist on disk. A `missing: true` entry means
    // we misparsed an argument into a path that was never a path.
    for input in inputs {
        assert_eq!(
            input["missing"], false,
            "classified a non-existent input: {}",
            input["path"]
        );
    }
}

/// If rustc emits an argument we do not model, this test is how we find out —
/// it is the mechanism behind the "unknown arguments are inventoried" bar.
#[test]
fn real_rustc_invocation_contains_no_unrecognized_arguments() {
    let fixture = RustFixture::binary("unknownargs", MULTI_MODULE_MAIN).unwrap();
    let build = fixture
        .build_with_linker(&blinker(), &record_into(&fixture.recording_dir()))
        .unwrap();
    assert!(build.success, "stderr:\n{}", build.stderr);

    let record = build.single_recording();
    let unrecognized = record["unrecognized_arguments"].as_array().unwrap();
    assert!(
        unrecognized.is_empty(),
        "rustc emitted arguments blinker does not model: {unrecognized:?}\n\
         This is expected to happen as the corpus grows — add them to the \
         `arguments` crate's classifier rather than relaxing this assertion."
    );
}

#[test]
fn recorded_invocation_can_be_replayed() {
    let fixture = RustFixture::binary("replay", MINIMAL_MAIN).unwrap();
    let build = fixture
        .build_with_linker(&blinker(), &record_into(&fixture.recording_dir()))
        .unwrap();
    assert!(build.success, "stderr:\n{}", build.stderr);

    let recording = &build.recordings[0];

    // Replaying re-runs the exact recorded argument vector. This is what makes
    // the corpus a regression suite rather than just an archive.
    let status = std::process::Command::new(blinker())
        .arg(format!(
            "--blinker-replay-invocation={}",
            recording.display()
        ))
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "replay failed:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
}

#[test]
fn version_and_help_work_without_a_link_configuration() {
    for flag in ["--blinker-version", "--blinker-help"] {
        let out = std::process::Command::new(blinker())
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag} failed");
        assert!(!out.stdout.is_empty(), "{flag} printed nothing");
    }
}

#[test]
fn unknown_blinker_option_fails_loudly_rather_than_being_forwarded() {
    let out = std::process::Command::new(blinker())
        .arg("--blinker-not-a-real-option")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown blinker option"));
}

#[test]
fn fallback_linker_failure_propagates_its_exit_code() {
    // A link that cannot succeed must surface the underlying linker's status,
    // not a status blinker invented.
    let out = std::process::Command::new(blinker())
        .arg("/nonexistent/blinker/input.o")
        .arg("-o")
        .arg("/nonexistent/blinker/output")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "linking a nonexistent input unexpectedly succeeded"
    );
}
