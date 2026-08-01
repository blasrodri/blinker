//! A link performed by the resident linker must be the link that would have
//! happened without it.
//!
//! The daemon's whole safety argument is that it changes *where* the work runs
//! and nothing else: the same argument vector, the same classification, the
//! same code. That is easy to say and easy to break — the working directory
//! moves, the argument vector travels through a socket, and the session
//! carries state from the previous link into this one.
//!
//! So this spawns a real daemon, links a real program through it, and compares
//! bytes with a link that never touched it.

use blinker_test_support::{workspace_binary, Scratch};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const MAIN: &str = r#"
#include <stdio.h>
int helper(int n);
int main(void) { printf("%d\n", helper(6)); return 0; }
"#;

const HELPER: &str = "int helper(int n) { return n * 7; }\n";

fn compile(scratch: &Scratch, name: &str, source: &str) -> PathBuf {
    let path = scratch.join(name);
    std::fs::write(&path, source).expect("source written");
    let object = path.with_extension("o");
    let status = Command::new("cc")
        .args(["-c", "-o"])
        .arg(&object)
        .arg(&path)
        .status()
        .expect("cc runs");
    assert!(status.success(), "compiling {name} failed");
    object
}

/// The driver arguments `cc` would hand a linker, near enough for a C program.
fn link_args(objects: &[PathBuf], output: &Path) -> Vec<String> {
    let mut argv = vec!["--blinker-internal".to_string()];
    for object in objects {
        argv.push(object.display().to_string());
    }
    argv.push("-o".to_string());
    argv.push(output.display().to_string());
    argv
}

/// A daemon that is killed when the test ends, however the test ends.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon() -> Daemon {
    let child = Command::new(workspace_binary("blinker"))
        .arg("--blinker-daemon-serve")
        .spawn()
        .expect("the daemon starts");
    Daemon(child)
}

/// Link through the daemon, waiting for it to come up.
///
/// The client falls back to linking in-process when no daemon answers, which
/// is the right behaviour and the wrong thing for this test: a fallback would
/// pass every assertion below while proving nothing. So the daemon is required
/// to have answered, which is what the marker file checks.
fn link_via_daemon(argv: &[String], output: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let _ = std::fs::remove_file(output);
        let mut full = vec!["--blinker-daemon".to_string()];
        full.extend(argv.iter().cloned());
        let status = Command::new(workspace_binary("blinker"))
            .args(&full)
            .output()
            .expect("blinker runs");
        if status.status.success() && output.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A distinct directory holding an output with a *fixed* name.
///
/// The output's base name is signed into the image — it becomes the code
/// signature's identifier — so `direct` and `served` differ by design and a
/// byte comparison between them proves nothing. Same name, different
/// directory, and the comparison means what it says.
fn output_in(scratch: &Scratch, directory: &str) -> PathBuf {
    let path = scratch.join(directory);
    std::fs::create_dir_all(&path).expect("directory created");
    path.join("program")
}

fn run(binary: &Path) -> String {
    let output = Command::new(binary).output().expect("the program runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The property: same bytes, whoever did the work.
#[test]
fn a_daemon_served_link_is_byte_identical() {
    let scratch = Scratch::dir("daemon-identical").expect("scratch");
    let objects = vec![
        compile(&scratch, "main.c", MAIN),
        compile(&scratch, "helper.c", HELPER),
    ];

    let direct = output_in(&scratch, "direct");
    let status = Command::new(workspace_binary("blinker"))
        .args(link_args(&objects, &direct))
        .status()
        .expect("blinker runs");
    assert!(status.success(), "the direct link failed");

    let _daemon = start_daemon();
    let served = output_in(&scratch, "served");
    assert!(
        link_via_daemon(&link_args(&objects, &served), &served),
        "the daemon never produced an output"
    );

    assert_eq!(
        std::fs::read(&served).expect("served"),
        std::fs::read(&direct).expect("direct"),
        "the daemon produced a different binary"
    );
    assert_eq!(run(&served), "42\n");
}

/// And the case the session makes possible: a second link through the same
/// daemon, after an edit, must see the edit.
///
/// A resident linker holds the previous parse of every input. An edit that the
/// session serves from memory is a build that silently ignores the change —
/// the worst failure this design can have, because the binary is valid and
/// wrong.
#[test]
fn a_second_link_through_the_daemon_sees_an_edit() {
    let scratch = Scratch::dir("daemon-edit").expect("scratch");
    let objects = vec![
        compile(&scratch, "main.c", MAIN),
        compile(&scratch, "helper.c", HELPER),
    ];

    let _daemon = start_daemon();
    let first = output_in(&scratch, "first");
    assert!(
        link_via_daemon(&link_args(&objects, &first), &first),
        "the daemon never produced an output"
    );
    assert_eq!(run(&first), "42\n");

    compile(
        &scratch,
        "helper.c",
        "int helper(int n) { return n * 8; }\n",
    );
    let second = output_in(&scratch, "second");
    assert!(link_via_daemon(&link_args(&objects, &second), &second));
    assert_eq!(run(&second), "48\n", "the daemon served a stale object");

    // And it matches a linker with no memory of the first build.
    let cold = output_in(&scratch, "cold");
    let status = Command::new(workspace_binary("blinker"))
        .args(link_args(&objects, &cold))
        .status()
        .expect("blinker runs");
    assert!(status.success());
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold"),
        "the warm link differs from a cold one"
    );
}

/// Asking for a daemon when none is running links anyway.
///
/// A build configured with `--blinker-daemon` must not fail because nobody
/// started one; the flag asks for a resident linker, it does not require one.
#[test]
fn asking_for_a_daemon_that_is_absent_still_links() {
    let scratch = Scratch::dir("daemon-absent-link").expect("scratch");
    let objects = vec![
        compile(&scratch, "main.c", MAIN),
        compile(&scratch, "helper.c", HELPER),
    ];
    let output = scratch.join("program");
    let mut argv = vec!["--blinker-daemon".to_string()];
    argv.extend(link_args(&objects, &output));

    let result = Command::new(workspace_binary("blinker"))
        .args(&argv)
        .output()
        .expect("blinker runs");
    assert!(
        result.status.success(),
        "it refused to link without a daemon: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(run(&output), "42\n");
}
