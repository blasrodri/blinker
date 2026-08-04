//! Dead-stripping a library, whose roots are not where a program's are.
//!
//! An executable enters at `_main` and everything live is reachable from it. A
//! dylib is entered at every symbol it exports: dyld resolves a name by walking
//! the export trie, so a function nothing *inside* the library calls is still
//! reachable from outside it. Strip a dylib from one root and it links, loads,
//! and has had a live function deleted — a failure with no symptom until
//! something calls the missing one.
//!
//! So the test is two-sided. An exported function nothing internal refers to
//! must survive and run; an unexported function nothing refers to must go.

use blinker_test_support::{blinker, Scratch};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `exported` is called by nobody in the library — only named in the export
/// list. `orphan` is called by nobody and exported by nobody.
const SOURCE: &str = r#"
int shared(int x) { return x * 7; }
int exported(void) { return shared(6); }
int orphan(void) { return shared(1000000); }
"#;

fn compile(scratch: &Scratch) -> PathBuf {
    let source = scratch.write("lib.c", SOURCE).expect("writable");
    let object = scratch.join("lib.o");
    let status = Command::new("cc")
        .args(["-arch", "arm64", "-mmacosx-version-min=11.0", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    assert!(status.success(), "cc failed");
    object
}

fn symbols(image: &Path) -> String {
    let output = Command::new("nm")
        .arg("-a")
        .arg(image)
        .output()
        .expect("nm runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn an_exported_function_nothing_calls_survives_the_strip() {
    let scratch = Scratch::dir("dylib-strip").expect("scratch");
    let object = compile(&scratch);
    let list = scratch
        .write("exports.txt", "_exported\n")
        .expect("writable");
    let out = scratch.join("libstripped.dylib");

    let status = blinker()
        .arg("--blinker-internal")
        .args(["-dynamiclib", "-lSystem", "-Wl,-dead_strip"])
        .arg("-Wl,-exported_symbols_list")
        .arg(format!("-Wl,{}", list.display()))
        .arg("-o")
        .arg(&out)
        .arg(&object)
        .status()
        .expect("blinker runs");
    assert!(status.success(), "the dylib did not link");

    let symbols = symbols(&out);
    assert!(
        symbols.contains("_exported"),
        "the exported root was stripped, so nothing can enter the library:\n{symbols}"
    );
    assert!(
        symbols.contains("_shared"),
        "what the root reaches was stripped with it:\n{symbols}"
    );
    assert!(
        !symbols.contains("_orphan"),
        "nothing exported reaches _orphan, so it should be gone:\n{symbols}"
    );

    // And it runs. A symbol table entry is not a function: the strip could have
    // kept the name and removed the bytes it points at.
    let host = scratch
        .write(
            "host.c",
            r#"
#include <dlfcn.h>
#include <stdio.h>
int main(int argc, char **argv) {
  void *library = dlopen(argv[1], RTLD_NOW);
  if (!library) { printf("dlopen: %s\n", dlerror()); return 1; }
  int (*exported)(void) = dlsym(library, "exported");
  if (!exported) { printf("dlsym: %s\n", dlerror()); return 2; }
  return exported();
}
"#,
        )
        .expect("writable");
    let host_binary = scratch.join("host");
    assert!(
        Command::new("cc")
            .arg(&host)
            .arg("-o")
            .arg(&host_binary)
            .status()
            .expect("cc runs")
            .success(),
        "the host program did not compile"
    );
    let ran = Command::new(&host_binary)
        .arg(&out)
        .status()
        .expect("the host runs");
    assert_eq!(
        ran.code(),
        Some(42),
        "the stripped library did not load, or the surviving function was wrong"
    );
}
