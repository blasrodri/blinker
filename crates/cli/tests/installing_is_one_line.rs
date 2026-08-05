//! The one line every user runs, run.
//!
//! # Why this file exists
//!
//! `install.sh` is the first thing anyone executes, it is piped from the
//! network into `/bin/sh`, and it had no test of any kind. Every other entry
//! point in this project is covered; the one that decides whether a user ever
//! reaches them was not.
//!
//! It could not be tested because it downloaded from a fixed URL, so the tests
//! could only have run against a real release. `BLINKER_RELEASE_BASE` exists
//! for that reason and no other: it points the download at a `file://`
//! directory this test builds, so the whole path — download, checksum, unpack,
//! install, configure — runs exactly as it does in production.

use std::path::Path;
use std::process::Command;

use blinker_test_support::{scratch::Scratch, workspace_binary};

/// Build a release the script can install: the real binary, tarred and
/// checksummed the way the release workflow does it.
fn publish(into: &Path) -> bool {
    let asset = "blinker-aarch64-apple-darwin.tar.gz";
    let binary = workspace_binary("blinker");
    std::fs::copy(&binary, into.join("blinker")).expect("copy the binary");
    let tarred = Command::new("tar")
        .current_dir(into)
        .args(["-czf", asset, "blinker"])
        .status()
        .expect("tar should run");
    if !tarred.success() {
        return false;
    }
    std::fs::remove_file(into.join("blinker")).expect("remove the loose copy");
    let sum = Command::new("shasum")
        .current_dir(into)
        .args(["-a", "256", asset])
        .output()
        .expect("shasum should run");
    std::fs::write(into.join(format!("{asset}.sha256")), sum.stdout).expect("write the checksum");
    true
}

fn installer() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("install.sh")
}

fn run(project: &Path, release: &Path, bin: &Path, args: &[&str]) -> std::process::Output {
    Command::new("sh")
        .arg(installer())
        .args(args)
        .current_dir(project)
        .env(
            "BLINKER_RELEASE_BASE",
            format!("file://{}", release.display()),
        )
        .env("BLINKER_INSTALL_DIR", bin)
        .output()
        .expect("the installer should run")
}

/// One line, from nothing to a project that links with blinker.
#[test]
fn one_line_installs_and_sets_the_project_up() {
    let scratch = Scratch::dir("install-one-line").expect("scratch");
    let release = scratch.join("release");
    let bin = scratch.join("bin");
    let project = scratch.join("project");
    for directory in [&release, &bin, &project] {
        std::fs::create_dir_all(directory).expect("mkdir");
    }
    if !publish(&release) {
        return;
    }
    std::fs::write(
        project.join("Cargo.toml"),
        b"[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write a project");

    let done = run(&project, &release, &bin, &["--use"]);
    assert!(
        done.status.success(),
        "the one-line install failed: {}",
        String::from_utf8_lossy(&done.stderr)
    );

    let installed = bin.join("blinker");
    assert!(installed.exists(), "no binary was installed");

    // The setting, pointing at the binary that was just installed — not at
    // whichever blinker happened to be on the machine already.
    let config = project.join(".cargo/config.toml");
    let written = std::fs::read_to_string(&config).expect("the project was not configured");
    assert!(
        written.contains(installed.to_string_lossy().as_ref()),
        "config.toml does not name the installed binary: {written}"
    );
    assert!(written.contains("aarch64-apple-darwin"));
}

/// Without `--use` it installs the binary and touches nothing else. This is the
/// documented promise that you can install it before deciding to use it.
#[test]
fn without_the_flag_it_configures_nothing() {
    let scratch = Scratch::dir("install-bare").expect("scratch");
    let release = scratch.join("release");
    let bin = scratch.join("bin");
    let project = scratch.join("project");
    for directory in [&release, &bin, &project] {
        std::fs::create_dir_all(directory).expect("mkdir");
    }
    if !publish(&release) {
        return;
    }
    std::fs::write(project.join("Cargo.toml"), b"[package]\nname = \"probe\"\n").expect("write");

    let done = run(&project, &release, &bin, &[]);
    assert!(done.status.success());
    assert!(bin.join("blinker").exists(), "no binary was installed");
    assert!(
        !project.join(".cargo/config.toml").exists(),
        "an install with no --use configured the project anyway"
    );
}

/// A checksum that does not match must stop the install rather than warn about
/// it. A linker is a program every build runs; this is the one check in the
/// script that is not allowed to be advisory.
#[test]
fn a_tampered_download_is_refused() {
    let scratch = Scratch::dir("install-tampered").expect("scratch");
    let release = scratch.join("release");
    let bin = scratch.join("bin");
    let project = scratch.join("project");
    for directory in [&release, &bin, &project] {
        std::fs::create_dir_all(directory).expect("mkdir");
    }
    if !publish(&release) {
        return;
    }
    std::fs::write(project.join("Cargo.toml"), b"[package]\nname = \"probe\"\n").expect("write");
    // The archive changes; the published checksum does not.
    std::fs::write(
        release.join("blinker-aarch64-apple-darwin.tar.gz"),
        b"not the binary you were promised",
    )
    .expect("tamper");

    let done = run(&project, &release, &bin, &["--use"]);
    assert!(
        !done.status.success(),
        "a bad checksum was installed anyway"
    );
    assert!(
        String::from_utf8_lossy(&done.stderr).contains("checksum"),
        "the refusal did not say why: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    assert!(
        !bin.join("blinker").exists(),
        "a binary that failed its checksum reached the install directory"
    );
}

/// `--use` in a directory with no project must fail *before* anything is
/// downloaded — the answer is already known, and finding out after a binary has
/// been put on the machine is a worse version of the same message.
#[test]
fn asking_to_set_up_a_directory_that_is_not_a_project_installs_nothing() {
    let scratch = Scratch::dir("install-no-project").expect("scratch");
    let release = scratch.join("release");
    let bin = scratch.join("bin");
    let empty = scratch.join("empty");
    for directory in [&release, &bin, &empty] {
        std::fs::create_dir_all(directory).expect("mkdir");
    }
    if !publish(&release) {
        return;
    }

    let done = run(&empty, &release, &bin, &["--use"]);
    assert!(!done.status.success());
    assert!(
        String::from_utf8_lossy(&done.stderr).contains("Cargo.toml"),
        "the refusal did not name what was missing: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    assert!(
        !bin.join("blinker").exists(),
        "it downloaded and installed before noticing there was no project"
    );
}

/// An option it does not know must be refused rather than ignored. A typo in a
/// piped one-liner should not silently install without setting anything up.
#[test]
fn an_unknown_option_is_refused_before_anything_is_downloaded() {
    let scratch = Scratch::dir("install-bad-flag").expect("scratch");
    let release = scratch.join("release");
    let bin = scratch.join("bin");
    for directory in [&release, &bin] {
        std::fs::create_dir_all(directory).expect("mkdir");
    }

    let done = run(scratch.path(), &release, &bin, &["--uze"]);
    assert!(!done.status.success());
    assert!(String::from_utf8_lossy(&done.stderr).contains("--uze"));
    assert!(!bin.join("blinker").exists());
}
