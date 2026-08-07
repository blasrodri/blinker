//! Output kinds blinker does not produce must delegate, not fail.
//!
//! `rustc` hands the same linker every crate in a workspace, so a linker that
//! refuses what it cannot do is unusable on any project containing one crate
//! it does not cover. It was unusable on *this* one: building blinker with
//! blinker stopped at `serde_derive` with "entry symbol _main is not defined
//! in any input", because a proc-macro crate is a `-dynamiclib`.
//!
//! blinker now produces dylibs, so that case is a control here rather than the
//! subject. `--blinker-internal` still means "link internally where you can",
//! and the fallback still carries a structured reason, so a delegation is
//! visible rather than silent.

use blinker_test_support::{blinker, Scratch};
use std::path::PathBuf;
use std::process::Command;

const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

fn compile(scratch: &Scratch, name: &str, code: &str) -> PathBuf {
    let source = scratch.write(name, code).expect("writable");
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
}

/// Run blinker, returning its exit status and the diagnostics record.
fn link(scratch: &Scratch, tag: &str, extra: &[&str], object: &PathBuf, out: &PathBuf) -> String {
    let record = scratch.join(format!("{tag}.json"));
    let status = blinker()
        .arg("--blinker-internal")
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .args(extra)
        .arg("-o")
        .arg(out)
        .arg(object)
        .status()
        .expect("blinker runs");
    assert!(
        status.success(),
        "blinker failed on {tag}; it should have delegated"
    );
    std::fs::read_to_string(&record).expect("record written")
}

/// A dynamic library is what every proc-macro crate is, and blinker now makes
/// one itself. The deliverable is not "a file appeared": it is a library dyld
/// loads and calls into, which is the only definition of a working dylib.
#[test]
fn a_dynamic_library_is_linked_internally_and_loads() {
    let scratch = Scratch::dir("internal-dylib").expect("scratch");
    let object = compile(
        &scratch,
        "lib.c",
        "int helper(int x) { return x * 7; }\nint answer(void) { return helper(6); }\n",
    );
    let out = scratch.join("libanswer.dylib");
    let json = link(
        &scratch,
        "dylib",
        &["-dynamiclib", "-lSystem"],
        &object,
        &out,
    );

    assert!(
        !json.contains("\"delegated\""),
        "the dylib was delegated:\n{json}"
    );
    let kind = Command::new("file").arg(&out).output().expect("file runs");
    let kind = String::from_utf8_lossy(&kind.stdout);
    assert!(
        kind.contains("dynamically linked shared library"),
        "not a dylib: {kind}"
    );

    // dlopen, then call. Everything else about a dylib can be right while it
    // is unloadable, and nothing but the loader can say that it is not.
    let host = scratch
        .write(
            "host.c",
            r#"
#include <dlfcn.h>
#include <stdio.h>
int main(int argc, char **argv) {
  void *library = dlopen(argv[1], RTLD_NOW);
  if (!library) { printf("dlopen: %s\n", dlerror()); return 1; }
  int (*answer)(void) = dlsym(library, "answer");
  if (!answer) { printf("dlsym: %s\n", dlerror()); return 2; }
  return answer();
}
"#,
        )
        .expect("writable");
    let host_binary = scratch.join("host");
    let built = Command::new("cc")
        .arg(&host)
        .arg("-o")
        .arg(&host_binary)
        .status()
        .expect("cc runs");
    assert!(built.success(), "the host program did not compile");

    let ran = Command::new(&host_binary)
        .arg(&out)
        .status()
        .expect("the host runs");
    assert_eq!(
        ran.code(),
        Some(42),
        "dyld could not load the library, or the function it called was wrong"
    );
}

/// The other output kinds blinker does not emit, each named separately so the
/// diagnostic says which one arrived.
#[test]
fn every_unsupported_output_kind_delegates() {
    let scratch = Scratch::dir("delegate-kinds").expect("scratch");
    let object = compile(&scratch, "u.c", "int used(void) { return 1; }\n");

    for (tag, flag) in [("bundle", "-bundle"), ("partial", "-r")] {
        let out = scratch.join(format!("out-{tag}"));
        let json = link(&scratch, tag, &[flag], &object, &out);
        assert!(
            json.contains("\"delegated\""),
            "{flag} was not delegated:\n{json}"
        );
    }
}

/// An input that is bitcode rather than Mach-O, which is what `-flto`
/// produces.
///
/// This used to fail the build with `malformed Mach-O object a.o: Unsupported
/// Mach-O header` — a sentence that describes a corrupt file, about a file that
/// is perfectly well formed and simply in a format blinker does not link. The
/// rule for output kinds applies unchanged to input formats: rustc hands the
/// same linker every crate, and one C dependency built with `-flto` should not
/// stop a workspace building.
///
/// The deliverable is the running program, not the delegation: what matters is
/// that the link *worked*, by whatever route.
#[test]
fn a_bitcode_input_delegates_and_still_produces_a_program() {
    let scratch = Scratch::dir("delegate-bitcode").expect("scratch");
    let source = scratch
        .write("lto.c", "int main(void) { return 9; }\n")
        .expect("writable");
    let object = scratch.join("lto.o");
    let compiled = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-flto", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    // A toolchain that will not produce bitcode has nothing to say here.
    if !compiled.success() {
        return;
    }
    let head = std::fs::read(&object).expect("read the object");
    assert!(
        blinker_macho::is_bitcode(&head),
        "the fixture is not bitcode, so it proves nothing"
    );

    let out = scratch.join("program");
    let json = link(&scratch, "bitcode", &[], &object, &out);
    assert!(
        json.contains("unsupported_input_format"),
        "a bitcode input was not delegated for that reason:\n{json}"
    );
    let status = Command::new(&out).status().expect("the program runs");
    assert_eq!(status.code(), Some(9), "the delegated link did not work");
}

/// Detection belongs at the parse boundary, not in a loose-file preflight: an
/// archive member is just as capable of being bitcode as a top-level object.
#[test]
fn a_bitcode_archive_member_delegates_too() {
    let scratch = Scratch::dir("delegate-bitcode-archive").expect("scratch");
    let main = compile(
        &scratch,
        "main.c",
        "extern int answer(void); int main(void) { return answer(); }\n",
    );
    let source = scratch
        .write("answer.c", "int answer(void) { return 11; }\n")
        .expect("writable");
    let bitcode = scratch.join("answer.o");
    let compiled = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-flto", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&bitcode)
        .status()
        .expect("cc runs");
    if !compiled.success() {
        return;
    }

    let archive = scratch.join("libanswer.a");
    let archived = Command::new("ar")
        .arg("rcs")
        .arg(&archive)
        .arg(&bitcode)
        .status()
        .expect("ar runs");
    assert!(archived.success(), "ar failed to build the fixture");

    let out = scratch.join("program");
    let record = scratch.join("bitcode-archive.json");
    let status = blinker()
        .arg("--blinker-internal")
        .arg("--blinker-json-diagnostics")
        .arg(&record)
        .arg("-o")
        .arg(&out)
        .arg(&main)
        .arg(&archive)
        .arg("-lSystem")
        .status()
        .expect("blinker runs");
    assert!(status.success(), "the archive link did not delegate");
    let json = std::fs::read_to_string(&record).expect("record written");
    assert!(
        json.contains("unsupported_input_format"),
        "a bitcode archive member was not delegated for that reason:\n{json}"
    );
    let status = Command::new(&out).status().expect("the program runs");
    assert_eq!(status.code(), Some(11), "the delegated link did not work");
}

/// And the control: an ordinary executable must **not** delegate, or the rule
/// above would be satisfied by a linker that delegates everything.
#[test]
fn an_ordinary_executable_is_still_linked_internally() {
    let scratch = Scratch::dir("delegate-control").expect("scratch");
    let object = compile(&scratch, "m.c", "int main(void) { return 3; }\n");
    let out = scratch.join("program");
    let json = link(&scratch, "exe", &[], &object, &out);

    assert!(
        !json.contains("\"delegated\""),
        "an ordinary executable was delegated:\n{json}"
    );
    let status = Command::new(&out).status().expect("the program runs");
    assert_eq!(status.code(), Some(3));
}
