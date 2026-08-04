//! The gate between "I built blinker" and "my build uses it".
//!
//! Everything else in this repository is about what happens after a project is
//! configured. This is about the step before it, which is where a linker is
//! actually lost: an absolute path written by hand, in a file that may not
//! exist, in a directory that may be the wrong one.
//!
//! Three commands, and the test is that each does what it says to a real
//! project on disk — `--blinker-install` writes it, `--blinker-uninstall`
//! leaves the project as it was found, and `--blinker-try` builds through
//! blinker while writing no configuration at all.

use blinker_test_support::{blinker, workspace_binary, Scratch};
use std::path::Path;
use std::process::Command;

/// A project cargo will build offline: one file, no dependencies.
fn project(tag: &str) -> Scratch {
    let scratch = Scratch::dir(tag).expect("scratch");
    scratch
        .write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    scratch
        .write("src/main.rs", "fn main() { std::process::exit(9); }\n")
        .expect("source");
    scratch
}

fn run(directory: &Path, flag: &str, rest: &[&str]) -> std::process::Output {
    blinker()
        .current_dir(directory)
        .arg(flag)
        .args(rest)
        .output()
        .expect("blinker runs")
}

#[test]
fn install_writes_a_config_that_names_this_binary() {
    let scratch = project("setup-install");
    let root = scratch.join("");
    let config = root.join(".cargo/config.toml");

    let out = run(&root, "--blinker-install", &[]);
    assert!(out.status.success(), "install failed: {out:?}");

    let text = std::fs::read_to_string(&config).expect("a config was written");
    assert!(text.contains("[target.aarch64-apple-darwin]"), "{text}");
    // The path of the binary that ran, absolute, so it still means this file
    // from any directory — which is the whole reason not to write it by hand.
    let expected = workspace_binary("blinker")
        .canonicalize()
        .expect("the binary exists");
    assert!(
        text.contains(&format!("linker = \"{}\"", expected.display())),
        "the config does not name this blinker:\n{text}"
    );

    // And the build actually goes through it. `-C link-arg` is not needed and
    // is not written: blinker links internally by default.
    let built = Command::new("cargo")
        .arg("build")
        .arg("--offline")
        .current_dir(&root)
        .env("BLINKER_NO_DAEMON", "1")
        .output()
        .expect("cargo runs");
    assert!(
        built.status.success(),
        "the configured build failed:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let ran = Command::new(root.join("target/debug/fixture"))
        .status()
        .expect("the program runs");
    assert_eq!(ran.code(), Some(9));

    // Twice is the same as once — the command has to be safe to run again,
    // because that is what somebody does when they are not sure it worked.
    let again = run(&root, "--blinker-install", &[]);
    assert!(again.status.success());
    assert_eq!(
        std::fs::read_to_string(&config).expect("still there"),
        text,
        "a second install changed the file"
    );
}

#[test]
fn uninstall_leaves_the_project_as_it_was_found() {
    let scratch = project("setup-uninstall");
    let root = scratch.join("");
    let config = root.join(".cargo/config.toml");

    assert!(run(&root, "--blinker-install", &[]).status.success());
    assert!(config.exists());
    assert!(run(&root, "--blinker-uninstall", &[]).status.success());
    assert!(
        !config.exists(),
        "an empty config file was left behind, which is not how the project was found"
    );
    assert!(
        !root.join(".cargo").exists(),
        "an empty .cargo directory was left behind"
    );
}

/// A config the user wrote keeps everything it said, and the parts blinker did
/// not add survive the uninstall.
#[test]
fn a_config_that_already_exists_is_edited_rather_than_replaced() {
    let scratch = project("setup-existing");
    let root = scratch.join("");
    let before = "# mine\n[build]\njobs = 3\n";
    scratch.write(".cargo/config.toml", before).expect("config");

    assert!(run(&root, "--blinker-install", &[]).status.success());
    let after = std::fs::read_to_string(root.join(".cargo/config.toml")).expect("config");
    assert!(after.contains("# mine"), "the comment was lost:\n{after}");
    assert!(after.contains("jobs = 3"), "a setting was lost:\n{after}");

    assert!(run(&root, "--blinker-uninstall", &[]).status.success());
    assert_eq!(
        std::fs::read_to_string(root.join(".cargo/config.toml")).expect("config"),
        before,
        "uninstalling did not restore the file"
    );
}

/// `--blinker-try` is for the question "does this work on my project" asked
/// before committing to anything. So it must commit to nothing: no config, and
/// its own target directory, so the next ordinary `cargo build` is not made to
/// rebuild everything by the flags this one used.
#[test]
fn try_builds_through_blinker_and_writes_no_configuration() {
    let scratch = project("setup-try");
    let root = scratch.join("");

    let out = blinker()
        .current_dir(&root)
        .env("BLINKER_NO_DAEMON", "1")
        .arg("--blinker-try")
        .args(["build", "--offline"])
        .output()
        .expect("blinker runs");
    assert!(
        out.status.success(),
        "the try build failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !root.join(".cargo").exists(),
        "--blinker-try wrote configuration, which is the one thing it must not do"
    );
    assert!(
        root.join("target/blinker-try/debug/fixture").exists(),
        "the build did not land in a target directory of its own"
    );
    assert!(
        !root.join("target/debug/fixture").exists(),
        "the try build used the project's own target directory"
    );

    // It is blinker's output, not the system linker's: blinker signs with an
    // identifier taken from the output's name, and nothing else does.
    let program = root.join("target/blinker-try/debug/fixture");
    let ran = Command::new(&program).status().expect("the program runs");
    assert_eq!(
        ran.code(),
        Some(9),
        "the program blinker linked did not run"
    );
}
