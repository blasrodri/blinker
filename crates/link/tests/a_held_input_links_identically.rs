//! A link that reuses parsed inputs must produce what a cold link produces.
//!
//! This is the property that makes a resident linker usable at all, and it is
//! not obvious: the second link takes objects and archive members out of
//! memory instead of off disk, and every one of them carries a byte window into
//! a buffer, an object id, and a symbol table that the rest of the link keys
//! on.
//!
//! The first version of the member cache handed back the parsed member paired
//! with *the whole archive* as its bytes, so every section was read from the
//! wrong offset. That failed loudly — a misaligned relocation, on the second
//! link — and it failed loudly only because the offsets happened not to line
//! up. A member whose sections landed somewhere plausible would have produced a
//! binary, and no test that checks "the link succeeded" would have noticed.
//!
//! So this checks the bytes.

use blinker_link::{link_to_file, link_to_file_in, LinkRequest, Session};
use blinker_test_support::Scratch;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAIN: &str = r#"
#include <stdio.h>
int helper(int n);
int other(int n);
int main(void) { printf("%d\n", helper(3) + other(4)); return 0; }
"#;

const HELPER: &str = "int helper(int n) { return n * 7; }\n";
const OTHER: &str = "int other(int n) { return n + 100; }\n";

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

/// The two helpers in an archive, so members are exercised and not only
/// top-level objects — members are the majority of a Rust link's inputs and
/// the half that was wrong.
fn archive(scratch: &Scratch, members: &[PathBuf]) -> PathBuf {
    let path = scratch.join("libhelpers.a");
    let mut command = Command::new("ar");
    command.arg("crs").arg(&path);
    for member in members {
        command.arg(member);
    }
    assert!(command.status().expect("ar runs").success(), "ar failed");
    path
}

fn inputs(scratch: &Scratch) -> Vec<PathBuf> {
    let main = compile(scratch, "main.c", MAIN);
    let helper = compile(scratch, "helper.c", HELPER);
    let other = compile(scratch, "other.c", OTHER);
    vec![main, archive(scratch, &[helper, other])]
}

fn run(binary: &Path) -> String {
    let output = Command::new(binary).output().expect("the program runs");
    assert!(output.status.success(), "the program exited non-zero");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The property: a second link through a warm session is byte-identical.
#[test]
fn a_second_link_through_one_session_is_byte_identical() {
    let scratch = Scratch::dir("session-identical").expect("scratch");
    let request = LinkRequest::new(inputs(&scratch));

    let cold = scratch.join("cold");
    link_to_file(&request, &cold).expect("the cold link succeeds");

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("the first link succeeds");
    let second = scratch.join("second");
    let timings = link_to_file_in(&request, &second, &mut session).expect("the second link");

    assert!(
        timings.inputs_held > 0,
        "nothing was held, so this proves nothing about holding"
    );
    assert_eq!(
        timings.inputs_read, 0,
        "an unchanged input was read again: {} held, {} read",
        timings.inputs_held, timings.inputs_read
    );
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold"),
        "the held link differs from a cold one"
    );
    assert_eq!(run(&second), "125\n");
}

/// And the case the byte comparison exists for: an archive member's bytes must
/// come from the member, not from the archive around it.
///
/// A held member with the wrong window still links; whether it *crashes*
/// depends on where its sections happen to land. Running the program is what
/// separates "it produced a file" from "it produced the program".
#[test]
fn a_held_archive_member_keeps_its_own_bytes() {
    let scratch = Scratch::dir("session-member-window").expect("scratch");
    let request = LinkRequest::new(inputs(&scratch));

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("first");
    let second = scratch.join("second");
    link_to_file_in(&request, &second, &mut session).expect("second");

    assert_eq!(run(&first), run(&second));
    assert_eq!(run(&second), "125\n", "3 * 7 + (4 + 100)");
}

/// A changed input is re-read even though the session holds a parse of it, and
/// the result is the one the new bytes produce.
#[test]
fn a_changed_input_is_not_served_from_the_session() {
    let scratch = Scratch::dir("session-changed").expect("scratch");
    let objects = inputs(&scratch);
    let request = LinkRequest::new(objects.clone());

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("first");
    assert_eq!(run(&first), "125\n");

    // helper now multiplies by 8 rather than 7, which changes the answer by 3.
    compile(
        &scratch,
        "helper.c",
        "int helper(int n) { return n * 8; }\n",
    );
    let helper = scratch.join("helper.o");
    let other = scratch.join("other.o");
    archive(&scratch, &[helper, other]);

    let second = scratch.join("second");
    let timings = link_to_file_in(&request, &second, &mut session).expect("second");
    assert!(
        timings.inputs_read > 0,
        "the changed archive was served from memory"
    );
    assert_eq!(run(&second), "128\n", "the edit did not reach the output");

    // And it matches what a linker with no memory of the first build produces.
    let cold = scratch.join("cold");
    link_to_file(&LinkRequest::new(objects), &cold).expect("cold");
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold")
    );
}
