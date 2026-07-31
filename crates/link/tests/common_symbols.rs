//! Tentative definitions — C's common symbols.
//!
//! `int arr[64];` at file scope is not a definition. Every translation unit
//! that declares it emits a *common* symbol carrying the size, and the linker
//! allocates one shared object of the largest size requested. Mach-O encodes
//! this as `N_UNDF | N_EXT` with a non-zero `n_value`, which looks exactly like
//! an undefined reference and is not one.
//!
//! blinker read them as undefined and refused to link (finding 65). It went
//! unnoticed through every Rust milestone because rustc never emits them, and
//! surfaced only when a C fixture written for something else used one.

use blinker_link::{link_to_file, LinkRequest};
use blinker_test_support::Scratch;
use std::path::PathBuf;
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

/// Link with blinker and run, returning stdout.
fn run(tag: &str, sources: &[(&str, &str)]) -> String {
    let scratch = Scratch::dir(tag).expect("scratch");
    let objects = compile(&scratch, sources);
    let program = scratch.join("program");
    link_to_file(&LinkRequest::new(objects), &program).expect("the link succeeds");
    let output = Command::new(&program).output().expect("the program runs");
    assert!(output.status.success(), "exit {:?}", output.status.code());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const MAIN: &str = r#"
#include <stdio.h>
int shared[16];
int counter;
void bump(void);
int main(void) {
    shared[2] = 5;
    bump();
    printf("%d %d\n", shared[2], counter);
    return 0;
}
"#;

const OTHER: &str = r#"
int shared[16];
int counter;
void bump(void) { shared[2] += 37; counter = 99; }
"#;

/// The minimum: one translation unit, one tentative definition.
#[test]
fn a_single_tentative_definition_gets_storage() {
    let source = r#"
#include <stdio.h>
int tentative[64];
int main(void) { tentative[3] = 7; printf("%d\n", tentative[3]); return 0; }
"#;
    assert_eq!(run("common-one", &[("c.c", source)]), "7\n");
}

/// Two objects declaring the same tentative definition must share **one**
/// object, not get one each. Separate storage would let both write and neither
/// see the other, which is a wrong answer rather than a crash.
#[test]
fn two_objects_share_a_single_tentative_definition() {
    assert_eq!(
        run("common-shared", &[("a.c", MAIN), ("b.c", OTHER)]),
        "42 99\n"
    );
}

/// A real definition anywhere makes the commons references to it.
///
/// The initialiser has to be *observed*, not merely present: a first version
/// of this test linked the same three objects and checked the final answer,
/// which is 42 whether the references resolve to the real object or to a
/// freshly zeroed common. Allocating storage that shadows a real definition
/// passed it.
#[test]
fn a_real_definition_wins_over_the_tentative_ones() {
    let main = r#"
#include <stdio.h>
int shared[16];
int main(void) { printf("%d\n", shared[2]); return 0; }
"#;
    let defining = "int shared[16] = {0, 0, 111};\n";
    assert_eq!(
        run("common-defined", &[("a.c", main), ("c.c", defining)]),
        "111\n",
        "the reference resolved to zeroed common storage, not the definition"
    );
}

/// Common storage is zero-filled, which C guarantees for static storage and
/// which the program can observe.
#[test]
fn tentative_storage_starts_zeroed() {
    let source = r#"
#include <stdio.h>
int big[512];
int main(void) {
    int sum = 0;
    for (int i = 0; i < 512; i++) sum += big[i];
    printf("%d\n", sum);
    return 0;
}
"#;
    assert_eq!(run("common-zero", &[("c.c", source)]), "0\n");
}

/// Declarations of different sizes must resolve to the largest, or the
/// translation unit that asked for more writes past the end of the object.
///
/// The canary is what makes this detectable. Reading back the array the
/// program just wrote proves nothing — the write lands *somewhere* and reads
/// back fine either way. `__common` is laid out in name order, so `zz_canary`
/// sits immediately after `aa_flexible`, and an undersized allocation
/// overwrites it.
#[test]
fn the_largest_declaration_decides_the_size() {
    let writer = r#"
#include <stdio.h>
int aa_flexible[256];
int zz_canary[8];
void check(void);
int main(void) {
    for (int i = 0; i < 256; i++) aa_flexible[i] = i + 1;
    check();
    int damage = 0;
    for (int i = 0; i < 8; i++) damage += zz_canary[i];
    printf("%d %d\n", aa_flexible[255], damage);
    return 0;
}
"#;
    // The same name, declared smaller. The 256-element view must still fit
    // entirely inside the object that gets allocated.
    let reader =
        "int aa_flexible[4];\nint zz_canary[8];\nvoid check(void) { aa_flexible[0] = 1; }\n";
    assert_eq!(
        run("common-sizes", &[("a.c", writer), ("b.c", reader)]),
        "256 0\n",
        "the array overran its allocation and damaged the next object"
    );
}

/// The whole point: blinker and the system linker must agree on the answer.
#[test]
fn the_result_matches_the_system_linker() {
    let scratch = Scratch::dir("common-vs-ld").expect("scratch");
    let objects = compile(&scratch, &[("a.c", MAIN), ("b.c", OTHER)]);

    let ours = scratch.join("ours");
    link_to_file(&LinkRequest::new(objects.clone()), &ours).expect("blinker links");

    let theirs = scratch.join("theirs");
    let status = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET])
        .args(&objects)
        .arg("-o")
        .arg(&theirs)
        .status()
        .expect("cc runs");
    assert!(status.success(), "the system linker failed");

    let ours = Command::new(&ours).output().expect("runs");
    let theirs = Command::new(&theirs).output().expect("runs");
    assert_eq!(ours.stdout, theirs.stdout);
    assert_eq!(ours.status.code(), theirs.status.code());
}
