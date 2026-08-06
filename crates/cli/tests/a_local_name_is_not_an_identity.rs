//! Two objects may define the same local symbol, and they are two symbols.
//!
//! Mach-O says a local symbol is visible only inside the object that defines
//! it, so its name is an identity *within that object* and nowhere else. Every
//! map in the linker that answers "where is this name" therefore has to be
//! keyed by scope as well — `AddressMap` always was, and the GOT and
//! thread-local tables were not.
//!
//! Nothing in the corpus had ever exercised that, because clang refers to its
//! own local data with section-relative relocations and never asks for a GOT
//! slot for one. Cranelift does: it names its per-function constant pool
//! `_.Ldata0` and reaches it through the GOT, so *every* object in a
//! Cranelift-built program declared a local of the same name. Deduplicating
//! the GOT by name collapsed a thousand distinct addresses into one slot, and
//! the link failed with `undefined symbols: _.Ldata0` — resolving that one
//! slot in the global scope, where a local by definition is not (finding 230).
//!
//! The fixture is assembly rather than Cranelift output so the test needs
//! nothing but `cc`: the shape that matters is a GOT-load relocation against a
//! non-`.globl` label, which is two lines of `.s` and is exactly what
//! Cranelift emits.

use blinker_test_support::{blinker, Scratch};
use std::path::PathBuf;
use std::process::Command;

/// A function returning a 32-bit datum reached through the GOT, and the datum.
///
/// `shared_local` is not `.globl`, so the assembler files it as a local: two
/// objects built from this template define two symbols that happen to share
/// spelling.
fn accessor(function: &str, value: i32, exported: bool) -> String {
    let export = if exported {
        ".globl shared_local\n"
    } else {
        ""
    };
    format!(
        r#"
.section __TEXT,__text
.globl {function}
.p2align 2
{function}:
	adrp	x0, shared_local@GOTPAGE
	ldr	x0, [x0, shared_local@GOTPAGEOFF]
	ldr	w0, [x0]
	ret

.section __DATA,__data
{export}.p2align 2
shared_local:
	.long {value}
"#
    )
}

fn assemble(scratch: &Scratch, name: &str, source: &str) -> PathBuf {
    let path = scratch.write(name, source).expect("writable");
    let object = scratch.join(format!("{name}.o"));
    let status = Command::new("cc")
        .args(["-arch", "arm64", "-mmacosx-version-min=11.0", "-c"])
        .arg(&path)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    assert!(status.success(), "cc failed to assemble {name}");
    object
}

/// Link internally and run, returning the program's exit code.
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
        "the link was delegated, so it proves nothing about blinker:\n{json}"
    );
    Command::new(&out)
        .status()
        .expect("the program runs")
        .code()
        .expect("the program exited")
}

/// Two locals of one name, each read through the GOT.
///
/// Before the fix this did not produce a wrong answer — it did not link at
/// all, naming `shared_local` undefined.
#[test]
fn two_objects_may_each_define_the_same_local() {
    let scratch = Scratch::dir("local-identity").expect("scratch");
    let first = assemble(&scratch, "first.s", &accessor("_first", 11, false));
    let second = assemble(&scratch, "second.s", &accessor("_second", 22, false));
    let main = assemble(
        &scratch,
        "main.c",
        "int first(void);\nint second(void);\nint main(void) { return first() + second(); }\n",
    );

    assert_eq!(
        link_and_run(&scratch, "two-locals", &[first, second, main]),
        33,
        "the two locals resolved to one address, so one accessor read the other's datum"
    );
}

/// And a global of the same name alongside them, which must shadow neither and
/// be shadowed by neither.
///
/// This is the direction a scope-keyed table can still get wrong while the
/// case above passes: give every entry the referencing object as its scope and
/// the global's slot is filed under whichever object happened to mention it,
/// where the *next* object's reference cannot find it.
#[test]
fn a_global_of_the_same_name_is_a_third_symbol() {
    let scratch = Scratch::dir("local-identity-global").expect("scratch");
    let first = assemble(&scratch, "first.s", &accessor("_first", 11, false));
    let second = assemble(&scratch, "second.s", &accessor("_second", 22, false));
    let third = assemble(&scratch, "third.s", &accessor("_third", 99, true));
    let main = assemble(
        &scratch,
        "main.c",
        "int first(void);\nint second(void);\nint third(void);\n\
         int main(void) { return first() + second() + third(); }\n",
    );

    assert_eq!(
        link_and_run(&scratch, "locals-and-global", &[first, second, third, main]),
        132,
        "a local and a global sharing a name were confused for one another"
    );
}
