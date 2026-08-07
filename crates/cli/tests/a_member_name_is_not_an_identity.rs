//! Two members of one archive may share a name, and they are two objects.
//!
//! `ar` has never required member names to be unique, and real archives are
//! not: a C++ project with `parser/utils.cpp` and `codegen/utils.cpp` compiles
//! both to `utils.cpp.o` and archives both under that name.
//! `libllvm_sys.rlib` holds *three* members called `COFF.cpp.o`, and pulsevm's
//! 312 inputs contain 48 such duplicates.
//!
//! The incremental layout keys a contribution on its archive's path, its
//! member name and its section, so all the copies were one contribution. All
//! of them looked up the previous link's single slot, all of them were told
//! they could stay, and the allocator placed them at one offset — overlapping
//! bytes, which `carve` refuses because the slices would alias.
//!
//! So **every incremental relink of pulsevm failed**, and it failed saying
//! `no input sections to link` on a link with 3676 objects and 47 MB of
//! `__text` (finding 241). Nothing in the corpus caught it: rustc names every
//! codegen unit uniquely, so a Rust-only archive cannot produce the shape, and
//! the linker had never been asked to relink a workload that could.
//!
//! Cold linking is not enough to exercise this. A cold link assigns offsets
//! sequentially and cannot overlap whatever the identities say; the collision
//! only becomes bytes when a *second* link reads the first one's placement
//! back. So these tests link twice, with an edit in between.

use blinker_test_support::{blinker, Scratch};
use std::path::PathBuf;
use std::process::Command;

fn compile(scratch: &Scratch, name: &str, code: &str) -> PathBuf {
    let source = scratch.write(name, code).expect("writable");
    let object = scratch.join(format!("{name}.o"));
    let status = Command::new("cc")
        .args(["-arch", "arm64", "-mmacosx-version-min=11.0", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    assert!(status.success(), "cc failed to compile {name}");
    object
}

/// An archive holding two members that share a name.
///
/// `ar` is given the two objects under one name in turn, which is what a build
/// system does when two directories produce the same basename. `libtool` and
/// `ar rcs` both preserve them; neither deduplicates.
fn archive_with_repeated_member(scratch: &Scratch, first: &PathBuf, second: &PathBuf) -> PathBuf {
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let archive = scratch.join("librepeat.a");
    let _ = std::fs::remove_file(&archive);
    for object in [first, second] {
        let staged = staging.join("utils.cpp.o");
        std::fs::copy(object, &staged).expect("stage the member");
        let status = Command::new("ar")
            .arg("-q") // append, never replace: `r` would collapse the two
            .arg(&archive)
            .arg(&staged)
            .status()
            .expect("ar runs");
        assert!(status.success(), "ar failed to append a member");
    }
    let members = Command::new("ar")
        .args(["-t"])
        .arg(&archive)
        .output()
        .expect("ar -t runs");
    let listed = String::from_utf8_lossy(&members.stdout);
    assert_eq!(
        listed
            .split_whitespace()
            .filter(|m| *m == "utils.cpp.o")
            .count(),
        2,
        "the archive did not keep both members; the fixture proves nothing:\n{listed}"
    );
    archive
}

/// Link into `out`, refusing a delegated link, and return the program's exit
/// code. The cache is on: without it there is no previous layout to read back
/// and the bug cannot appear.
fn link_and_run(scratch: &Scratch, tag: &str, out: &PathBuf, inputs: &[PathBuf]) -> i32 {
    let record = scratch.join(format!("{tag}.json"));
    let status = blinker()
        .arg("--blinker-internal")
        .arg("--blinker-no-daemon")
        .arg("--blinker-cache")
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .arg("-o")
        .arg(out)
        .args(inputs)
        .arg("-lSystem")
        .status()
        .expect("blinker runs");
    assert!(status.success(), "blinker failed to link {tag}");
    let json = std::fs::read_to_string(&record).expect("record written");
    assert!(
        !json.contains("\"delegated\""),
        "the link was delegated, so it says nothing about blinker:\n{json}"
    );
    Command::new(out)
        .status()
        .expect("the program runs")
        .code()
        .expect("the program exited")
}

/// The failure, at its smallest: relink an archive with a repeated member.
///
/// The second link is the one that matters. It reads the first link's
/// placement table, and both copies of `utils.cpp.o` ask it for the same slot.
#[test]
fn an_archive_with_two_members_of_one_name_relinks() {
    let scratch = Scratch::dir("member-identity").expect("scratch");
    let first = compile(
        &scratch,
        "first.c",
        "int alpha(void) { return 10; }\nstatic const int pad_a[64] = {1};\nint use_a(void){return pad_a[0];}\n",
    );
    let second = compile(
        &scratch,
        "second.c",
        "int beta(void) { return 7; }\nstatic const int pad_b[128] = {2};\nint use_b(void){return pad_b[0];}\n",
    );
    let archive = archive_with_repeated_member(&scratch, &first, &second);
    let main = compile(
        &scratch,
        "main.c",
        r#"
int alpha(void);
int beta(void);
int use_a(void);
int use_b(void);
int main(void) { return alpha() + beta() + use_a() + use_b() == 20 ? 0 : 1; }
"#,
    );

    let out = scratch.join("program");
    assert_eq!(
        link_and_run(&scratch, "cold", &out, &[main.clone(), archive.clone()]),
        0,
        "the cold link is wrong before the relink is even attempted"
    );

    // An edit, so the second link is a relink and not a replay of the finished
    // image — a replay would never reach the allocator.
    let edited = compile(
        &scratch,
        "main.c",
        r#"
int alpha(void);
int beta(void);
int use_a(void);
int use_b(void);
int spacer(void) { return 3; }
int main(void) { return alpha() + beta() + use_a() + use_b() + spacer() == 23 ? 0 : 1; }
"#,
    );
    assert_eq!(
        link_and_run(&scratch, "warm", &out, &[edited, archive]),
        0,
        "the relink placed both copies of the repeated member at one offset"
    );
}

/// And the same with three copies, which is what `libllvm_sys.rlib` actually
/// has — two could be a coincidence of a pairwise check, three cannot.
#[test]
fn three_members_of_one_name_relink() {
    let scratch = Scratch::dir("member-identity-three").expect("scratch");
    let staging = scratch.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let archive = scratch.join("libthree.a");
    let _ = std::fs::remove_file(&archive);
    for (index, size) in [16usize, 48, 96].iter().enumerate() {
        let object = compile(
            &scratch,
            &format!("unit{index}.c"),
            &format!(
                "int part{index}(void) {{ return {index}; }}\n\
                 static const int pad{index}[{size}] = {{1}};\n\
                 int reach{index}(void) {{ return pad{index}[0]; }}\n"
            ),
        );
        let staged = staging.join("COFF.cpp.o");
        std::fs::copy(&object, &staged).expect("stage");
        let status = Command::new("ar")
            .arg("-q")
            .arg(&archive)
            .arg(&staged)
            .status()
            .expect("ar runs");
        assert!(status.success());
    }
    let body = (0..3)
        .map(|i| format!("int part{i}(void); int reach{i}(void);"))
        .collect::<Vec<_>>()
        .join("\n");
    let sum = (0..3)
        .map(|i| format!("part{i}() + reach{i}()"))
        .collect::<Vec<_>>()
        .join(" + ");
    let main = compile(
        &scratch,
        "main.c",
        &format!("{body}\nint main(void) {{ return {sum} == 6 ? 0 : 1; }}\n"),
    );
    let out = scratch.join("program");
    assert_eq!(
        link_and_run(&scratch, "cold", &out, &[main, archive.clone()]),
        0
    );

    let edited = compile(
        &scratch,
        "main.c",
        &format!("{body}\nint spacer(void) {{ return 4; }}\nint main(void) {{ return {sum} + spacer() == 10 ? 0 : 1; }}\n"),
    );
    assert_eq!(
        link_and_run(&scratch, "warm", &out, &[edited, archive]),
        0,
        "three copies of one member name did not survive a relink"
    );
}
