//! Dead-stripping, as a thing the linker does rather than reports.
//!
//! The analysis half has its own tests in `reachability.rs`, which check a
//! prediction. These check the image: bytes that must be gone, bytes that must
//! not be, and a program that must still behave the same.
//!
//! Every case here is paired with its own negative control — the same fixture
//! linked without `-dead_strip` — because "the output got smaller" and "the
//! output is correct" are both satisfied by a linker that removed the wrong
//! thing, and only the pair distinguishes them.

use blinker_link::{link_to_file, link_to_file_timed, LinkRequest};
use blinker_test_support::Scratch;
use std::path::{Path, PathBuf};
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

/// One fixture linked both ways, which is the only comparison that says
/// anything.
struct Both {
    stripped: PathBuf,
    whole: PathBuf,
    _scratch: Scratch,
}

fn link_both(tag: &str, sources: &[(&str, &str)]) -> Both {
    let scratch = Scratch::dir(tag).expect("scratch");
    let objects = compile(&scratch, sources);

    let stripped = scratch.join("stripped");
    link_to_file(
        &LinkRequest::new(objects.clone()).dead_stripped(true),
        &stripped,
    )
    .expect("the stripped link succeeds");

    let whole = scratch.join("whole");
    link_to_file(&LinkRequest::new(objects), &whole).expect("the plain link succeeds");

    Both {
        stripped,
        whole,
        _scratch: scratch,
    }
}

fn run(program: &Path) -> (String, Option<i32>) {
    let output = Command::new(program).output().expect("the program runs");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code(),
    )
}

/// Whether a byte string appears anywhere in a file.
fn contains(path: &Path, needle: &str) -> bool {
    let bytes = std::fs::read(path).expect("readable");
    bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

fn text_size(path: &Path) -> u64 {
    let output = Command::new("size")
        .arg("-m")
        .arg(path)
        .output()
        .expect("size runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.contains("Section __text:"))
        .and_then(|line| line.rsplit(' ').next()?.parse().ok())
        .expect("a __text section")
}

const DEAD_AND_LIVE: &str = r#"
#include <stdio.h>
__attribute__((noinline)) int never_called(int n) {
    printf("this-string-belongs-to-dead-code\n");
    return n * 9;
}
__attribute__((noinline)) int reached(int n) {
    printf("this-string-belongs-to-live-code\n");
    return n + 1;
}
int main(void) { return reached(41) == 42 ? 0 : 3; }
"#;

/// The point of the feature: an uncalled function's bytes leave the image.
///
/// Measured through `__text`'s size rather than the file's, because a Mach-O
/// executable is mostly padding, load commands and signature — a real strip
/// moves the file size by less than the page alignment hides.
#[test]
fn an_uncalled_functions_code_is_removed() {
    let both = link_both("strip-text", &[("c.c", DEAD_AND_LIVE)]);
    let stripped = text_size(&both.stripped);
    let whole = text_size(&both.whole);
    assert!(
        stripped < whole,
        "__text did not shrink: {stripped} vs {whole}"
    );
}

/// And the data only that function reached goes with it. `__cstring` is not
/// referenced by anything else, so its atom dies when the code does.
#[test]
fn data_reached_only_by_dead_code_is_removed() {
    let both = link_both("strip-cstring", &[("c.c", DEAD_AND_LIVE)]);
    assert!(
        contains(&both.whole, "this-string-belongs-to-dead-code"),
        "the control is wrong: the string was never in the unstripped image"
    );
    assert!(
        !contains(&both.stripped, "this-string-belongs-to-dead-code"),
        "a literal only unreachable code refers to survived"
    );
    // The other half of the same section must not go with it.
    assert!(
        contains(&both.stripped, "this-string-belongs-to-live-code"),
        "a literal live code refers to was stripped"
    );
}

/// The program must behave identically. Stripping that changes an answer is
/// not stripping.
#[test]
fn the_stripped_program_behaves_the_same() {
    let source = r#"
#include <stdio.h>
__attribute__((noinline)) int unused_one(int n) { return n * 9; }
__attribute__((noinline)) int unused_two(int n) { return n - 4; }
__attribute__((noinline)) int helper(int n) { return n * 2 + 1; }
int main(void) {
    for (int i = 0; i < 4; i++) printf("%d\n", helper(i));
    return 0;
}
"#;
    let both = link_both("strip-behaviour", &[("c.c", source)]);
    assert_eq!(run(&both.stripped), run(&both.whole));
    assert_eq!(run(&both.stripped).0, "1\n3\n5\n7\n");
}

/// A function reached only through a pointer in data has no call site. Getting
/// this wrong is not a smaller binary, it is a jump into whatever replaced it.
#[test]
fn a_function_reached_only_through_data_survives() {
    let source = r#"
#include <stdio.h>
int via_pointer(int n) { return n + 5; }
int (*const table[])(int) = { via_pointer };
int main(void) { printf("%d\n", table[0](37)); return 0; }
"#;
    let both = link_both("strip-pointer", &[("c.c", source)]);
    assert_eq!(run(&both.stripped).0, "42\n");
    assert_eq!(run(&both.stripped), run(&both.whole));
}

/// `__attribute__((used))` sets `N_NO_DEAD_STRIP`, which says "keep this
/// whatever the graph says". Nothing calls it, so a linker that ignores the
/// flag passes every other test here.
#[test]
fn a_symbol_marked_no_dead_strip_survives() {
    let source = r#"
__attribute__((used)) int kept_by_attribute(int n) { return n + 7; }
int main(void) { return 0; }
"#;
    let both = link_both("strip-used", &[("c.c", source)]);
    let names = Command::new("nm")
        .arg(&both.stripped)
        .output()
        .expect("nm runs");
    let names = String::from_utf8_lossy(&names.stdout);
    assert!(
        names.contains("_kept_by_attribute"),
        "a symbol marked used was stripped:\n{names}"
    );
}

/// The symbol table has to agree with the image. An entry for a function whose
/// bytes are gone points at whatever moved into its place, which is worse than
/// having no entry: every symbolizer would believe it.
#[test]
fn the_symbol_table_drops_what_the_image_dropped() {
    let both = link_both("strip-symtab", &[("c.c", DEAD_AND_LIVE)]);
    let names = |path: &Path| {
        let output = Command::new("nm").arg(path).output().expect("nm runs");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let whole = names(&both.whole);
    assert!(
        whole.contains("_never_called"),
        "the control is wrong: it was never in the unstripped table"
    );
    let stripped = names(&both.stripped);
    assert!(
        !stripped.contains("_never_called"),
        "a stripped function kept its symbol:\n{stripped}"
    );
    assert!(stripped.contains("_reached"), "{stripped}");
}

/// Stripping is off unless asked for, as in `ld`.
///
/// Not a stylistic preference: a link that drops input the command line did
/// not ask it to drop produces a binary that cannot be explained from its
/// arguments.
#[test]
fn nothing_is_stripped_without_the_flag() {
    let both = link_both("strip-optin", &[("c.c", DEAD_AND_LIVE)]);
    assert!(contains(&both.whole, "this-string-belongs-to-dead-code"));
    let names = Command::new("nm").arg(&both.whole).output().expect("nm");
    assert!(String::from_utf8_lossy(&names.stdout).contains("_never_called"));
}

/// Archive extraction runs before reachability, so a reference from dead code
/// can pull in a member whose every output byte is then discarded. The counter
/// makes that avoidable loader work measurable.
#[test]
fn a_member_extracted_only_for_dead_code_is_reported() {
    let scratch = Scratch::dir("strip-dead-archive-member").expect("scratch");
    let objects = compile(
        &scratch,
        &[
            (
                "main.c",
                r#"
extern int archive_answer(void);
__attribute__((noinline)) int never_called(void) { return archive_answer(); }
int main(void) { return 0; }
"#,
            ),
            ("answer.c", "int archive_answer(void) { return 42; }\n"),
        ],
    );
    let archive = scratch.join("libanswer.a");
    let status = Command::new("ar")
        .arg("rcs")
        .arg(&archive)
        .arg(&objects[1])
        .status()
        .expect("ar runs");
    assert!(status.success(), "ar failed to build the fixture");

    let stripped = scratch.join("stripped-archive");
    let timings = link_to_file_timed(
        &LinkRequest::new(vec![objects[0].clone(), archive.clone()]).dead_stripped(true),
        &stripped,
    )
    .expect("the stripped archive link succeeds");
    assert_eq!(timings.extracted_archive_members, 1);
    assert_eq!(timings.fully_dead_archive_members, 1);
    assert!(timings.fully_dead_archive_member_bytes > 0);

    let whole = scratch.join("whole-archive");
    let control = link_to_file_timed(&LinkRequest::new(vec![objects[0].clone(), archive]), &whole)
        .expect("the unstripped archive link succeeds");
    assert_eq!(control.extracted_archive_members, 1);
    assert_eq!(control.fully_dead_archive_members, 0);
}

/// And the result must match the linker that already does this.
#[test]
fn the_stripped_result_matches_the_system_linker() {
    let scratch = Scratch::dir("strip-vs-ld").expect("scratch");
    let objects = compile(&scratch, &[("c.c", DEAD_AND_LIVE)]);

    let ours = scratch.join("ours");
    link_to_file(
        &LinkRequest::new(objects.clone()).dead_stripped(true),
        &ours,
    )
    .expect("blinker links");

    let theirs = scratch.join("theirs");
    let status = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-Wl,-dead_strip"])
        .args(&objects)
        .arg("-o")
        .arg(&theirs)
        .status()
        .expect("cc runs");
    assert!(status.success(), "the system linker failed");

    assert_eq!(run(&ours), run(&theirs));
}
