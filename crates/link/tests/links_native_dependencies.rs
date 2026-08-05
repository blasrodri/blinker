//! Programs that are not pure Rust.
//!
//! # Why this file exists
//!
//! Every safety net this project had compared blinker against *itself* — warm
//! bytes against cold — or against ld64 on a Rust workload. Both are blind to a
//! whole class of defect, and pointing the linker at one real C++ project found
//! five of them in a day: libraries read only from `.tbd` stubs (218), an
//! undefined `___dso_handle` and static constructors that silently never ran
//! (219), undefined symbols reported from dead code (220), and two alignment
//! bugs that made a working program unlinkable (221).
//!
//! Not one would have been caught by the gate. So the gate now links C, C++ and
//! a non-SDK dynamic library, and — because the worst of those bugs produced a
//! program that ran and returned the right exit code while doing the wrong
//! thing — it **runs what it links** and compares against the system linker's
//! build of the same objects.

use std::path::{Path, PathBuf};
use std::process::Command;

use blinker_test_support::{blinker, no_daemon, scratch::Scratch};

/// Compile one source file, or `None` if this machine cannot.
fn compile(scratch: &Scratch, name: &str, source: &str, extra: &[&str]) -> Option<PathBuf> {
    let file = scratch.join(name);
    let object = scratch.join(format!("{name}.o"));
    std::fs::write(&file, source).expect("write the source");
    let done = Command::new("cc")
        .arg("-c")
        .args(extra)
        .arg(&file)
        .arg("-o")
        .arg(&object)
        .output()
        .ok()?;
    done.status.success().then_some(object)
}

/// Link with blinker, the way rustc drives it.
fn link(output: &Path, objects: &[&Path], extra: &[&str]) -> std::process::Output {
    let sdk = blinker_link::sdk_root().expect("an SDK to link against");
    let mut command = blinker();
    let command = no_daemon(&mut command);
    command
        .args(["-arch", "arm64"])
        .args(["-platform_version", "macos", "26.0.0", "26.5"])
        .arg("-syslibroot")
        .arg(&sdk)
        .arg("-o")
        .arg(output);
    for object in objects {
        command.arg(object);
    }
    command
        .args(extra)
        .args(["-lSystem"])
        .output()
        .expect("blinker should run")
}

fn run(program: &Path) -> (String, Option<i32>) {
    let done = Command::new(program)
        .output()
        .expect("the program should run");
    (
        String::from_utf8_lossy(&done.stdout).into_owned(),
        done.status.code(),
    )
}

/// A C++ program that does the things the bugs were hiding in: a global with a
/// constructor, a table of aligned constants reached by a runtime index, and
/// string handling — with dead code alongside it, so `-dead_strip` has work.
const A_PROGRAM: &str = r#"
#include <cstdio>
#include <cstring>
#include <string>

struct Registry {
    int count;
    Registry() : count(7) { printf("constructed\n"); }
    ~Registry() { printf("destroyed\n"); }
};
static Registry the_registry;

alignas(16) static const char TABLE[4][16] = {
    "alpha", "bravo", "charlie", "delta",
};

// Never called: dead-stripping should remove it, along with its reference to a
// function this program does not define.
extern "C" void a_function_nothing_defines(void);
extern "C" void unused(void) { a_function_nothing_defines(); }

int main(int argc, char **argv) {
    int at = argc & 3;
    char held[16];
    memcpy(held, TABLE[at], 16);
    printf("row %d is %s\n", at, held);
    std::string built(held);
    built += "-suffix";
    printf("built %s (%zu)\n", built.c_str(), built.size());
    printf("count %d\n", the_registry.count);
    return (int)built.size();
}
"#;

/// The headline case: the same objects through both linkers must produce
/// programs that *behave* the same, not merely link.
#[test]
fn a_cpp_program_behaves_the_same_as_the_system_linker_builds_it() {
    let scratch = Scratch::dir("native-cpp").expect("scratch");
    let Some(object) = compile(&scratch, "program.cc", A_PROGRAM, &["-O2"]) else {
        return;
    };

    let theirs = scratch.join("theirs");
    let built = Command::new("cc")
        .arg(&object)
        .arg("-o")
        .arg(&theirs)
        .args(["-lc++", "-Wl,-dead_strip"])
        .output()
        .expect("cc should run");
    // If the system linker will not build it, the fixture is wrong rather than
    // blinker — say so instead of failing blinker for it.
    assert!(
        built.status.success(),
        "the fixture does not link with cc: {}",
        String::from_utf8_lossy(&built.stderr)
    );

    let ours = scratch.join("ours");
    let linked = link(&ours, &[&object], &["-lc++", "-Wl,-dead_strip"]);
    assert!(
        linked.status.success(),
        "blinker failed to link a program cc links: {}",
        String::from_utf8_lossy(&linked.stderr)
    );

    let (their_output, their_code) = run(&theirs);
    let (our_output, our_code) = run(&ours);
    assert_eq!(
        our_output, their_output,
        "the two programs printed different things"
    );
    assert_eq!(our_code, their_code, "the two programs exited differently");
    // Belt and braces: the constructor's line is the one that silently vanished
    // in finding 219, and an empty comparison would pass if both were broken.
    assert!(
        our_output.contains("constructed") && our_output.contains("destroyed"),
        "neither program ran its global's constructor — output {our_output:?}"
    );
}

/// A dynamic library with no `.tbd` beside it, which is every library outside
/// the SDK. Finding 218: these were resolved, then silently discarded, and
/// every symbol they defined came out undefined.
#[test]
fn a_program_links_against_a_dylib_that_has_no_text_stub() {
    let scratch = Scratch::dir("native-dylib").expect("scratch");
    let library_source = scratch.join("answer.c");
    std::fs::write(&library_source, b"int answer(void) { return 42; }\n").expect("write");
    let library = scratch.join("libanswer.dylib");
    let built = Command::new("cc")
        .args(["-dynamiclib", "-o"])
        .arg(&library)
        .arg(&library_source)
        .args(["-install_name", "@rpath/libanswer.dylib"])
        .output()
        .expect("cc should run");
    if !built.status.success() {
        return;
    }
    assert!(
        !scratch.join("libanswer.tbd").exists(),
        "the fixture must have no stub, or it proves nothing"
    );

    let program = "extern int answer(void);\nint main(void) { return answer(); }\n";
    let Some(object) = compile(&scratch, "caller.c", program, &[]) else {
        return;
    };
    let output = scratch.join("program");
    let linked = link(
        &output,
        &[&object],
        &[
            "-L",
            &scratch.path().to_string_lossy(),
            "-lanswer",
            "-rpath",
            &scratch.path().to_string_lossy(),
        ],
    );
    assert!(
        linked.status.success(),
        "a dylib with no stub should still be linkable: {}",
        String::from_utf8_lossy(&linked.stderr)
    );

    let (_, code) = run(&output);
    assert_eq!(code, Some(42), "the library's function did not run");

    // The recorded install name, not the path it was found at — that is what
    // dyld looks for, and getting it from the symlink instead is a load-time
    // failure rather than a link-time one.
    let listed = Command::new("otool")
        .arg("-L")
        .arg(&output)
        .output()
        .expect("otool should run");
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("@rpath/libanswer.dylib"),
        "the dependency should be recorded by its install name"
    );
}

/// A link the session declines to cache must produce the same bytes as one it
/// caches. `BLINKER_MEMORY_BUDGET=0` makes every link "oversized", which is the
/// path finding 224 added: inputs too large to hold are not held.
///
/// Byte equality is the point. The bail-out changes *what is remembered*, and
/// remembering less may never change what is emitted.
///
/// Both outputs are named `prog`, in different directories. The output's file
/// name is part of the image's identity and reaches its UUID — linking to
/// `held` and `dropped` produces two different binaries for a reason that has
/// nothing to do with caching, which is how the first version of this test came
/// to report a linker bug that was its own.
#[test]
fn declining_to_cache_a_link_does_not_change_its_output() {
    let scratch = Scratch::dir("oversized").expect("scratch");
    let Some(object) = compile(&scratch, "program.cc", A_PROGRAM, &["-O2"]) else {
        return;
    };
    let held_dir = scratch.join("held");
    let dropped_dir = scratch.join("dropped");
    std::fs::create_dir_all(&held_dir).expect("mkdir");
    std::fs::create_dir_all(&dropped_dir).expect("mkdir");

    let held = held_dir.join("prog");
    let linked = link(&held, &[&object], &["-lc++", "-Wl,-dead_strip"]);
    assert!(linked.status.success());

    // The same link, with a budget nothing can fit inside.
    let dropped = dropped_dir.join("prog");
    let sdk = blinker_link::sdk_root().expect("an SDK to link against");
    let mut command = blinker();
    let done = no_daemon(&mut command)
        .env("BLINKER_MEMORY_BUDGET", "0")
        .args(["-arch", "arm64"])
        .args(["-platform_version", "macos", "26.0.0", "26.5"])
        .arg("-syslibroot")
        .arg(&sdk)
        .arg("-o")
        .arg(&dropped)
        .arg(&object)
        .args(["-lc++", "-Wl,-dead_strip", "-lSystem"])
        .output()
        .expect("blinker should run");
    assert!(
        done.status.success(),
        "an uncacheable link should still link: {}",
        String::from_utf8_lossy(&done.stderr)
    );

    assert_eq!(
        std::fs::read(&held).expect("read"),
        std::fs::read(&dropped).expect("read"),
        "declining to cache changed the output bytes"
    );
    let (said, code) = run(&dropped);
    assert!(said.contains("constructed"), "and it must still run");
    assert_eq!(code, run(&held).1);
}
