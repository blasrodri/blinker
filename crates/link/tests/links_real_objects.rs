//! Linking object files the real compiler produced, and running the result.
//!
//! The unit tests in each crate check one stage against its own model of the
//! format. This checks all of them against the only judge that matters: a
//! process that either produces the right exit status or does not.
//!
//! Every test here compiles its input with `cc`, so the objects are the same
//! ones `ld64` would be handed — not synthetic ones shaped to suit blinker.

use blinker_link::{link_to_file, LinkRequest};
use blinker_test_support::Scratch;
use std::path::{Path, PathBuf};
use std::process::Command;

/// rustc's deployment target, which is the one blinker must link for.
///
/// `cc` defaults to the running OS version, where the toolchain switches to
/// chained fixups; blinker implements the classic opcode streams that macOS 11
/// selects. Compiling the fixtures at the default would mean testing against
/// object files rustc never produces.
const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

/// Compile C sources to object files in a scratch directory.
fn compile(scratch: &Scratch, sources: &[(&str, &str)]) -> Vec<PathBuf> {
    let mut objects = Vec::new();
    for (name, code) in sources {
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
        objects.push(object);
    }
    objects
}

/// Run a program and return its exit status, with output on failure.
fn run(path: &Path) -> (Option<i32>, String, String) {
    let output = Command::new(path)
        .output()
        .unwrap_or_else(|e| panic!("could not execute {}: {e}", path.display()));
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The headline: a real object file, linked by blinker, that runs.
#[test]
fn a_single_object_links_and_runs() {
    let scratch = Scratch::dir("link-single").expect("scratch");
    let objects = compile(&scratch, &[("main.c", "int main(void) { return 42; }\n")]);

    let output = scratch.join("program");
    let request = LinkRequest::new(objects).identifier("program");
    link_to_file(&request, &output).expect("blinker links the object");

    let (status, stdout, stderr) = run(&output);
    assert_eq!(
        status,
        Some(42),
        "the linked program returned the wrong status\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// Several objects, with a call across the boundary between them.
///
/// This is where the coordinate systems have to agree: `helper` is defined in
/// one object and called from another, so the branch is patched using an
/// address that layout assigned to a section from a *different* file. Getting
/// the per-object offset wrong produces a program that jumps into the middle
/// of something and crashes rather than returning a wrong number.
#[test]
fn multiple_objects_link_with_a_call_between_them() {
    let scratch = Scratch::dir("link-multi").expect("scratch");
    let objects = compile(
        &scratch,
        &[
            ("helper.c", "int helper(int n) { return n * 3; }\n"),
            (
                "main.c",
                "int helper(int n);\nint main(void) { return helper(7); }\n",
            ),
        ],
    );

    let output = scratch.join("program");
    let request = LinkRequest::new(objects).identifier("program");
    link_to_file(&request, &output).expect("blinker links both objects");

    let (status, stdout, stderr) = run(&output);
    assert_eq!(
        status,
        Some(21),
        "cross-object call produced the wrong result\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// Data in `__DATA`, referenced from code via ADRP/ADD.
///
/// Exercises the page-relative relocation pair against a section in a
/// different segment, where the target address and the place being patched are
/// far apart.
#[test]
fn a_global_variable_is_read_at_the_right_address() {
    let scratch = Scratch::dir("link-data").expect("scratch");
    let objects = compile(
        &scratch,
        &[(
            "main.c",
            "int value = 99;\nint main(void) { return value; }\n",
        )],
    );

    let output = scratch.join("program");
    let request = LinkRequest::new(objects).identifier("program");
    link_to_file(&request, &output).expect("blinker links the object");

    let (status, stdout, stderr) = run(&output);
    assert_eq!(
        status,
        Some(99),
        "global read gave the wrong value\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// Data defined in one object and read from another.
///
/// This case is why hand-running a link mattered: the suite passed without a
/// GOT at all, because a global read from the *same* object is a direct
/// ADRP/ADD and a cross-object *call* is a direct branch. Only cross-object
/// data takes the indirect path — the compiler cannot know whether the
/// definition will land in this image or in a dylib, so it emits
/// `GOT_LOAD_PAGE21`/`PAGEOFF12` and leaves the choice to the linker.
///
/// The expected value is computed two ways on purpose: `pick(2)` reads the
/// table through the GOT, and `table[1]` reads it directly.
#[test]
fn data_shared_between_objects_is_reached_through_the_got() {
    let scratch = Scratch::dir("link-got").expect("scratch");
    let objects = compile(
        &scratch,
        &[
            (
                "a.c",
                "int table[4] = {10, 20, 30, 40};\nint pick(int i);\n\
                 int main(void) { return pick(2) + table[1]; }\n",
            ),
            (
                "b.c",
                "extern int table[4];\nint pick(int i) { return table[i]; }\n",
            ),
        ],
    );

    let output = scratch.join("program");
    let request = LinkRequest::new(objects).identifier("program");
    let image = link_to_file(&request, &output).expect("blinker links through the GOT");

    assert!(
        image.layout.sections.iter().any(|s| s.name == "__got"),
        "no __got section was synthesised"
    );

    let (status, stdout, stderr) = run(&output);
    assert_eq!(
        status,
        Some(50),
        "GOT-indirect read gave the wrong value\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// blinker's answer must equal ld64's for the same objects.
///
/// Guards against a program that runs and is confidently wrong.
#[test]
fn blinker_and_ld64_agree_on_the_result() {
    let scratch = Scratch::dir("link-agree").expect("scratch");
    let sources: &[(&str, &str)] = &[
        (
            "a.c",
            "int table[4] = {10, 20, 30, 40};\nint pick(int i);\n\
             int main(void) { return pick(3) + table[0]; }\n",
        ),
        (
            "b.c",
            "extern int table[4];\nint pick(int i) { return table[i]; }\n",
        ),
    ];
    let objects = compile(&scratch, sources);

    let ours = scratch.join("ours");
    link_to_file(&LinkRequest::new(objects.clone()).identifier("ours"), &ours)
        .expect("blinker links");

    let theirs = scratch.join("theirs");
    let status = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-o"])
        .arg(&theirs)
        .args(&objects)
        .status()
        .expect("cc runs");
    assert!(status.success(), "ld64 failed to link the same objects");

    let (ours_status, _, _) = run(&ours);
    let (theirs_status, _, _) = run(&theirs);
    assert_eq!(
        ours_status, theirs_status,
        "blinker and ld64 produced programs with different results"
    );
}

/// An undefined symbol must be an error, not a program that crashes later.
#[test]
fn an_undefined_symbol_is_reported_rather_than_linked() {
    let scratch = Scratch::dir("link-undef").expect("scratch");
    let objects = compile(
        &scratch,
        &[(
            "main.c",
            "int nowhere(void);\nint main(void) { return nowhere(); }\n",
        )],
    );

    let request = LinkRequest::new(objects);
    let error = blinker_link::link(&request).expect_err("link should fail");
    let message = error.to_string();
    assert!(
        message.contains("nowhere") || message.contains("undefined"),
        "the error should name the missing symbol: {message}"
    );
}

/// A missing entry point is an error too.
#[test]
fn a_missing_entry_symbol_is_reported() {
    let scratch = Scratch::dir("link-noentry").expect("scratch");
    let objects = compile(&scratch, &[("lib.c", "int thing(void) { return 1; }\n")]);

    let mut request = LinkRequest::new(objects);
    request.entry_symbol = "_main".to_string();
    let error = blinker_link::link(&request).expect_err("link should fail");
    assert!(
        error.to_string().contains("_main"),
        "the error should name the entry symbol: {error}"
    );
}

/// The linked image must satisfy the system's signature verifier.
#[test]
fn the_linked_image_is_validly_signed() {
    let scratch = Scratch::dir("link-signed").expect("scratch");
    let objects = compile(&scratch, &[("main.c", "int main(void) { return 0; }\n")]);

    let output = scratch.join("program");
    let request = LinkRequest::new(objects).identifier("program");
    link_to_file(&request, &output).expect("links");

    let verify = Command::new("codesign")
        .arg("-v")
        .arg(&output)
        .output()
        .expect("codesign runs");
    assert!(
        verify.status.success(),
        "codesign rejected blinker's output: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
}
