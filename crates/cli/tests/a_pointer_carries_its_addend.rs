//! A pointer into the middle of something must point into the middle of it.
//!
//! ARM64 Mach-O stores a plain pointer's addend **in the bytes being patched**
//! rather than in the relocation entry — `r_addend` is zero on every one of
//! them. The relocation engine *overwrites* those bytes with the target's
//! address, so an addend that is not read out first is not lost at link time:
//! it was never there.
//!
//! It was never there. Three pointers into one anonymous constant pool all
//! resolved to the pool's base, so a table of them read its first element
//! three times (finding 235). Nothing caught it because a Rust program mostly
//! points *at* symbols rather than *into* them — `self` links byte-identically
//! with the fix and without it — while a C program does this the moment it
//! writes `&array[3]` in a static initialiser.

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

/// Link internally, refusing a delegated link — which would prove nothing —
/// and return the program's exit code.
fn link_and_run(scratch: &Scratch, tag: &str, objects: &[PathBuf]) -> i32 {
    let out = scratch.join(format!("program-{tag}"));
    let record = scratch.join(format!("{tag}.json"));
    let status = blinker()
        .arg("--blinker-internal")
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .arg("-o")
        .arg(&out)
        .args(objects)
        .arg("-lSystem")
        .status()
        .expect("blinker runs");
    assert!(status.success(), "blinker failed to link {tag}");
    let json = std::fs::read_to_string(&record).expect("record written");
    assert!(
        !json.contains("\"delegated\""),
        "the link was delegated, so it says nothing about blinker:\n{json}"
    );
    Command::new(&out)
        .status()
        .expect("the program runs")
        .code()
        .expect("the program exited")
}

/// Pointers into an anonymous pool, which is the shape that failed.
///
/// `pool` is `static`, so the pointers are relocations against its symbol with
/// the element offset stored inline. The program returns the sum of what they
/// point at, so a dropped addend gives a wrong number rather than a crash —
/// which is how this stayed invisible.
#[test]
fn a_pointer_into_a_static_array_keeps_its_offset() {
    let scratch = Scratch::dir("addend-pool").expect("scratch");
    let object = compile(
        &scratch,
        "pool.c",
        r#"
static const int pool[8] = {10, 11, 12, 13, 14, 15, 16, 17};
static const int *const table[3] = { &pool[0], &pool[3], &pool[7] };
int main(void) { return *table[0] + *table[1] + *table[2]; }
"#,
    );
    assert_eq!(
        link_and_run(&scratch, "pool", &[object]),
        10 + 13 + 17,
        "the addends were dropped, so every pointer found the pool's first element"
    );
}

/// The same, one object pointing into another's array, so the addend rides on
/// a cross-object reference rather than a local one.
#[test]
fn a_pointer_into_another_objects_array_keeps_its_offset() {
    let scratch = Scratch::dir("addend-cross").expect("scratch");
    let holder = compile(
        &scratch,
        "holder.c",
        "const int numbers[6] = {1, 2, 4, 8, 16, 32};\n",
    );
    let user = compile(
        &scratch,
        "user.c",
        r#"
extern const int numbers[6];
static const int *const picks[2] = { &numbers[2], &numbers[5] };
int main(void) { return *picks[0] + *picks[1]; }
"#,
    );
    assert_eq!(
        link_and_run(&scratch, "cross", &[holder, user]),
        4 + 32,
        "a cross-object pointer lost its offset"
    );
}

/// And a negative addend, which `wrapping_add` has to carry the other way.
///
/// C cannot spell one portably — `&array[-1]` is undefined — so this is
/// assembly, which is also what a compiler emits when it does produce one. The
/// relocation is against `_numbers` with `fffffff8 ffffffff` sitting in the
/// bytes to be patched: minus eight, and an implementation that folds the
/// addend as unsigned lands 4 GB away instead.
#[test]
fn a_pointer_before_a_symbol_keeps_its_negative_offset() {
    let scratch = Scratch::dir("addend-negative").expect("scratch");
    let data = compile(
        &scratch,
        "pointers.s",
        r#"
.section __DATA,__const
.p2align 3
.globl _interior
_interior:
	.quad _numbers + 12
.globl _before
_before:
	.quad _numbers - 8

.section __TEXT,__const
.p2align 2
.globl _numbers
_numbers:
	.long 100
	.long 200
	.long 300
	.long 400
"#,
    );
    let main = compile(
        &scratch,
        "main.c",
        r#"
extern const int numbers[4];
extern const int *const interior;
extern const char *const before;
int main(void) {
  if (*interior != 400) { return 1; }  /* + 12 is the fourth element */
  if (before != (const char *)numbers - 8) { return 2; }
  return 0;
}
"#,
    );
    assert_eq!(
        link_and_run(&scratch, "negative", &[data, main]),
        0,
        "an interior pointer or a negative one resolved to the wrong address"
    );
}
