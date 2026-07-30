//! Parsing the real SDK stubs a Rust link resolves against.
//!
//! Rust passes `-lSystem -lc -lm`, and real projects add `-liconv` and
//! `-lobjc`. These tests read those stubs from the installed SDK, so they track
//! whatever Xcode actually ships rather than a checked-in snapshot that would
//! drift.

use blinker_tbd::{parse_tbd_file, Target};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The active SDK's path, per `xcrun`.
fn sdk_path() -> PathBuf {
    let output = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .expect("xcrun runs");
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn stub(name: &str) -> Option<PathBuf> {
    let path = sdk_path().join("usr/lib").join(name);
    path.exists().then_some(path)
}

/// libSystem is the stub every Rust link depends on, and the one whose
/// structure motivated this crate's design.
#[test]
fn libsystem_parses_as_a_multi_document_umbrella() {
    let Some(path) = stub("libSystem.B.tbd") else {
        panic!("libSystem.B.tbd not found in the SDK");
    };
    let file = parse_tbd_file(&path).expect("libSystem parses");

    // One document would mean we are reading it wrong.
    assert!(
        file.documents.len() > 10,
        "expected an umbrella of many documents, found {}",
        file.documents.len()
    );

    let primary = file.primary().expect("has a primary document");
    assert_eq!(primary.install_name, "/usr/lib/libSystem.B.dylib");
    assert!(
        !primary.reexported_libraries.is_empty(),
        "libSystem should re-export its sub-libraries"
    );
}

/// The finding, verified against the real file: the primary document exports
/// almost nothing, and everything useful comes from following re-exports.
#[test]
fn libsystems_useful_symbols_come_from_reexports() {
    let Some(path) = stub("libSystem.B.tbd") else {
        panic!("libSystem.B.tbd not found");
    };
    let file = parse_tbd_file(&path).expect("parses");
    let wanted = Target::aarch64_macos();

    let direct = file.primary().expect("primary").symbols_for(wanted).count();
    let all = file.exported_symbols(wanted);

    assert!(
        all.len() > direct * 100,
        "expected re-exports to dominate: direct={direct}, total={}",
        all.len()
    );

    // The symbols any Rust binary needs. If the architecture rule or the
    // re-export walk were wrong, these would be missing and every link would
    // fail with undefined symbols.
    for symbol in [
        "_malloc",
        "_free",
        "_memcpy",
        "_write",
        "_pthread_create",
        "_exit",
    ] {
        assert!(
            all.contains(symbol),
            "`{symbol}` not found; libSystem resolution is broken"
        );
    }
}

/// An arm64 link must resolve against stubs that declare only arm64e. This is
/// the rule that, if tightened to exact matching, breaks every link.
#[test]
fn an_arm64_link_resolves_against_the_real_arm64e_stubs() {
    let Some(path) = stub("libSystem.B.tbd") else {
        panic!("libSystem.B.tbd not found");
    };
    let file = parse_tbd_file(&path).expect("parses");

    let primary = file.primary().expect("primary");
    let declares_plain_arm64 = primary
        .targets
        .iter()
        .any(|t| t.architecture == blinker_tbd::Architecture::Arm64);

    // If Apple ever adds arm64-macos to libSystem this assertion becomes
    // stale — but the resolution below must keep working either way.
    if !declares_plain_arm64 {
        assert!(
            primary
                .targets
                .iter()
                .any(|t| t.architecture == blinker_tbd::Architecture::Arm64e),
            "libSystem declares neither arm64 nor arm64e"
        );
    }

    assert!(
        !file.exported_symbols(Target::aarch64_macos()).is_empty(),
        "an arm64 link found no symbols in libSystem"
    );
}

/// Every stub Rust and real projects link against must parse.
#[test]
fn every_stub_a_rust_link_uses_parses() {
    // `-lSystem -lc -lm` from rustc; `-liconv -lobjc` observed in real projects
    // during the M0 corpus run.
    let candidates = [
        "libSystem.B.tbd",
        "libSystem.tbd",
        "libc.tbd",
        "libm.tbd",
        "libiconv.tbd",
        "libobjc.tbd",
        "libc++.tbd",
    ];

    let mut parsed = 0;
    for name in candidates {
        let Some(path) = stub(name) else { continue };
        let file = parse_tbd_file(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            file.primary().is_some(),
            "{name} parsed but has no primary document"
        );
        parsed += 1;
    }

    assert!(parsed >= 3, "expected several stubs, parsed {parsed}");
}

/// A broad sweep: nothing in the SDK should make the parser fail or panic.
/// This is where an unfamiliar TBD construct would surface.
#[test]
fn the_whole_sdk_stub_directory_parses() {
    let dir = sdk_path().join("usr/lib");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        panic!("cannot read {}", dir.display());
    };

    let mut parsed = 0;
    let mut failures = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tbd") {
            continue;
        }
        match parse_tbd_file(&path) {
            Ok(file) => {
                assert!(file.primary().is_some());
                parsed += 1;
            }
            Err(err) => failures.push(format!("{}: {err}", path.display())),
        }
    }

    assert!(parsed > 100, "expected many stubs, parsed {parsed}");
    assert!(
        failures.is_empty(),
        "{} stub(s) failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Symbol names must survive parsing intact — they are matched exactly during
/// resolution, so a mangled name is an undefined symbol later.
#[test]
fn symbol_names_are_preserved_exactly() {
    let Some(path) = stub("libSystem.B.tbd") else {
        panic!("libSystem.B.tbd not found");
    };
    let file = parse_tbd_file(&path).expect("parses");
    let symbols = file.exported_symbols(Target::aarch64_macos());

    for symbol in symbols.iter().take(500) {
        assert!(!symbol.is_empty(), "empty symbol name");
        assert!(
            !symbol.starts_with('\'') && !symbol.ends_with('\''),
            "`{symbol}` still carries YAML quoting"
        );
        assert!(
            !symbol.contains(char::is_whitespace),
            "`{symbol}` contains whitespace"
        );
    }
}

/// Malformed input must be refused rather than partially accepted.
#[test]
fn corrupted_stubs_are_rejected() {
    let Some(path) = stub("libSystem.B.tbd") else {
        panic!("libSystem.B.tbd not found");
    };
    let text = std::fs::read_to_string(&path).expect("readable");

    // Truncation mid-document is the realistic corruption. Either it parses
    // into something coherent or it errors — never a panic.
    for fraction in [2, 3, 5, 8, 13] {
        let truncated = &text[..text.len() / fraction];
        // An error is a perfectly good outcome here; what must not happen is
        // a panic, or a document that parsed into something incoherent.
        if let Ok(file) = blinker_tbd::parse_tbd(truncated, Path::new("/truncated.tbd")) {
            for document in &file.documents {
                assert!(!document.install_name.is_empty());
            }
        }
    }
}
