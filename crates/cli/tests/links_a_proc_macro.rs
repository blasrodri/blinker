//! The case blinker delegated for its whole life until now.
//!
//! A proc-macro crate is a `-dynamiclib`, and rustc does not merely link it —
//! it `dlopen`s the result *inside its own process* and calls into it to expand
//! macros. So the compiler is the loader here, and "the library is correct"
//! and "the build works" are the same statement. Nothing short of running the
//! compiler against it establishes that.
//!
//! Two crates, because one proves less than it looks: `pm` is linked by blinker
//! and `uses` is compiled by a rustc that has to load it.

use blinker_test_support::{workspace_binary, Scratch};
use std::process::Command;

/// `cargo build` (or `run`) in `directory`, with blinker as the linker.
fn cargo(command: &str, scratch: &Scratch, directory: &str) -> std::process::Output {
    let blinker = workspace_binary("blinker");
    Command::new("cargo")
        .arg(command)
        .current_dir(scratch.join(directory))
        // Offline: the fixtures have no dependencies, and a test that reaches
        // the network is a test that fails on a train.
        .arg("--offline")
        .env("BLINKER_NO_DAEMON", "1")
        .env(
            "RUSTFLAGS",
            format!(
                "-C linker={} -C link-arg=--blinker-internal",
                blinker.display()
            ),
        )
        .output()
        .expect("cargo runs")
}

#[test]
fn a_proc_macro_blinker_linked_is_loaded_and_run_by_rustc() {
    let scratch = Scratch::dir("proc-macro").expect("scratch");
    scratch
        .write(
            "pm/Cargo.toml",
            "[package]\nname = \"pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [lib]\nproc-macro = true\n",
        )
        .expect("manifest");
    scratch
        .write(
            "pm/src/lib.rs",
            "use proc_macro::TokenStream;\n\
             #[proc_macro]\n\
             pub fn answer(_: TokenStream) -> TokenStream {\n    \
                 \"42\".parse().expect(\"a literal parses\")\n\
             }\n",
        )
        .expect("source");

    let built = cargo("build", &scratch, "pm");
    assert!(
        built.status.success(),
        "the proc-macro crate did not link:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    // What blinker produced, before anything tries to use it: a dylib that
    // exports what rustc asked it to export and nothing else. rustc passes
    // `-exported_symbols_list` naming two symbols out of the tens of thousands
    // the crate defines, and a library that exports the rest offers a
    // definition for every Rust symbol in it.
    let library = std::fs::read_dir(scratch.join("pm/target/debug/deps"))
        .expect("a deps directory")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|kind| kind == "dylib"))
        .expect("a dylib was produced");
    let exports = Command::new("dyld_info")
        .arg("-exports")
        .arg(&library)
        .output()
        .expect("dyld_info runs");
    let exports = String::from_utf8_lossy(&exports.stdout);
    let names: Vec<&str> = exports
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|name| name.starts_with('_'))
        .collect();
    assert_eq!(
        names.len(),
        2,
        "a proc-macro dylib exports the declarations and its metadata, nothing else:\n{exports}"
    );
    assert!(
        names.iter().any(|n| n.contains("proc_macro_decls")),
        "the declarations rustc looks up are not exported:\n{exports}"
    );

    // And the deliverable: a crate whose macro expands, which happens inside
    // rustc, through dyld, in the library blinker wrote.
    scratch
        .write(
            "uses/Cargo.toml",
            "[package]\nname = \"uses\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\npm = { path = \"../pm\" }\n",
        )
        .expect("manifest");
    scratch
        .write(
            "uses/src/main.rs",
            "fn main() { std::process::exit(pm::answer!()); }\n",
        )
        .expect("source");

    let ran = cargo("run", &scratch, "uses");
    assert_eq!(
        ran.status.code(),
        Some(42),
        "rustc could not load the proc macro, or it expanded to the wrong thing:\n{}",
        String::from_utf8_lossy(&ran.stderr)
    );
}
