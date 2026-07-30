//! End-to-end tests: real `cargo build` invocations driven through the real
//! blinker binary.
//!
//! These establish the M0 acceptance criteria. Unit tests over synthetic argv
//! can only confirm we handle the arguments we *imagined*; only a genuine build
//! confirms we handle the ones rustc emits.
//!
//! The project-shape tests are driven by [`blinker_test_support::catalog`], so
//! adding a shape there gets it built, recorded, and checked here without a new
//! test being written by hand.

use blinker_test_support::{catalog, workspace_binary, Network, RustFixture, MULTI_MODULE_MAIN};
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

/// Whether crates.io is reachable, so network-dependent fixtures can be skipped
/// rather than reported as failures on an offline machine.
fn network_available() -> bool {
    std::net::TcpStream::connect_timeout(
        &"3.33.220.204:443".parse().expect("literal address parses"),
        std::time::Duration::from_secs(3),
    )
    .is_ok()
        || std::net::TcpStream::connect_timeout(
            &"151.101.64.223:443"
                .parse()
                .expect("literal address parses"),
            std::time::Duration::from_secs(3),
        )
        .is_ok()
}

/// Every project shape in the catalog must build through blinker, and every
/// argument rustc emits for it must classify.
///
/// This is the M0 acceptance bar. It is one test rather than nine because the
/// catalog is the source of truth — a new shape should not need a new test.
#[test]
fn every_project_shape_builds_and_fully_classifies() {
    let online = network_available();
    let mut checked = 0;

    for kind in catalog() {
        if kind.network == Network::Required && !online {
            eprintln!("skipping {}: needs crates.io", kind.tag);
            continue;
        }

        let fixture = kind.build().expect("fixture is creatable");
        let build = fixture
            .build_with_linker(&blinker(), &record_into(&fixture.recording_dir()))
            .expect("cargo runs");

        assert!(
            build.success,
            "fixture `{}` ({}) failed to build through blinker\nstderr:\n{}",
            kind.tag, kind.exercises, build.stderr
        );

        for record in build.all_recordings() {
            let unrecognized = record["unrecognized_arguments"]
                .as_array()
                .expect("records always carry the field");
            assert!(
                unrecognized.is_empty(),
                "fixture `{}` produced arguments blinker does not model: {unrecognized:?}\n\
                 Add them to the `arguments` crate — check `reference.rs` for the \
                 option's arity before assuming it takes no value.",
                kind.tag
            );
            assert_eq!(record["mode"], "delegated");
            assert_eq!(record["exit_code"], 0);
            assert_eq!(record["arch"], "arm64");

            // A `missing: true` input means an argument was misparsed into a
            // path that was never a path — the signature of an arity bug.
            for input in record["inputs"].as_array().expect("inputs is an array") {
                assert_eq!(
                    input["missing"], false,
                    "fixture `{}` classified a non-existent input: {}\n\
                     This usually means a value-taking option's value was read \
                     as an input file.",
                    kind.tag, input["path"]
                );
            }
        }
        checked += 1;
    }

    assert!(
        checked >= 8,
        "expected at least 8 shapes, checked {checked}"
    );
}

/// Building is necessary but not sufficient — the produced executable has to
/// run, and its panic path has to reach the runtime's handler.
#[test]
fn produced_binaries_run_and_unwind() {
    let fixture = RustFixture::new("runcheck")
        .and_then(|f| {
            let name = f.name();
            f.file(
                "Cargo.toml",
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
                ),
            )
        })
        .and_then(|f| f.file("src/main.rs", MULTI_MODULE_MAIN))
        .expect("fixture is creatable");

    let build = fixture
        .build_with_linker(&blinker(), &[])
        .expect("cargo runs");
    assert!(build.success, "stderr:\n{}", build.stderr);

    let exe = fixture.built_binary();
    let ok = std::process::Command::new(&exe)
        .output()
        .expect("binary runs");
    assert!(ok.status.success());
    assert!(String::from_utf8_lossy(&ok.stdout).starts_with("alpha beta:"));

    let panicked = std::process::Command::new(&exe)
        .env("FIXTURE_SHOULD_PANIC", "1")
        .output()
        .expect("binary runs");
    assert!(!panicked.status.success());
    assert!(String::from_utf8_lossy(&panicked.stderr).contains("requested panic"));
}

/// The central M0 deliverable: a recording that describes a real link
/// completely enough to be useful.
#[test]
fn recorded_invocation_captures_the_real_link_configuration() {
    let fixture = catalog()
        .into_iter()
        .find(|k| k.tag == "multimod")
        .expect("multimod fixture exists")
        .build()
        .expect("fixture is creatable");

    let build = fixture
        .build_with_linker(&blinker(), &record_into(&fixture.recording_dir()))
        .expect("cargo runs");
    assert!(build.success, "stderr:\n{}", build.stderr);

    let record = build.single_recording();
    assert!(record["deployment_target"]
        .as_str()
        .is_some_and(|s| s.starts_with("macosx")));
    assert!(record["output_path"]
        .as_str()
        .is_some_and(|s| s.contains(fixture.name().as_str())));

    let inputs = record["inputs"].as_array().expect("inputs is an array");
    assert!(
        inputs.len() > 5,
        "expected several inputs, got {}",
        inputs.len()
    );
    assert!(record["counters"]["bytes_read"].as_u64().expect("counter") > 0);

    // Archiving is what makes the recording replayable at all: rustc deletes
    // the temp directory holding the object files as soon as the link returns.
    assert!(
        inputs.iter().all(|i| i["archived_path"].is_string()),
        "every input should have been archived"
    );
    assert!(record["replay_argv"].is_array());
}

#[test]
fn recorded_invocation_can_be_replayed() {
    let fixture = catalog()
        .into_iter()
        .find(|k| k.tag == "minimal")
        .expect("minimal fixture exists")
        .build()
        .expect("fixture is creatable");

    let build = fixture
        .build_with_linker(&blinker(), &record_into(&fixture.recording_dir()))
        .expect("cargo runs");
    assert!(build.success, "stderr:\n{}", build.stderr);

    let out = std::process::Command::new(blinker())
        .arg(format!(
            "--blinker-replay-invocation={}",
            build.recordings[0].display()
        ))
        .output()
        .expect("blinker runs");
    assert!(
        out.status.success(),
        "replay failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn version_and_help_work_without_a_link_configuration() {
    for flag in ["--blinker-version", "--blinker-help"] {
        let out = std::process::Command::new(blinker())
            .arg(flag)
            .output()
            .expect("blinker runs");
        assert!(out.status.success(), "{flag} failed");
        assert!(!out.stdout.is_empty(), "{flag} printed nothing");
    }
}

#[test]
fn unknown_blinker_option_fails_loudly_rather_than_being_forwarded() {
    let out = std::process::Command::new(blinker())
        .arg("--blinker-not-a-real-option")
        .output()
        .expect("blinker runs");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown blinker option"));
}

#[test]
fn fallback_linker_failure_propagates_its_exit_code() {
    let out = std::process::Command::new(blinker())
        .arg("/nonexistent/blinker/input.o")
        .arg("-o")
        .arg("/nonexistent/blinker/output")
        .output()
        .expect("blinker runs");
    assert!(
        !out.status.success(),
        "linking a nonexistent input unexpectedly succeeded"
    );
}
