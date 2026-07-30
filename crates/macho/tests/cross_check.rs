//! Cross-checks against `nm` and `otool`.
//!
//! A parser that is confidently wrong is worse than one that fails loudly, and
//! nothing inside this crate can tell the difference. These tests compare what
//! blinker parsed against tools that were right first — the M1 acceptance
//! criterion that parsed symbol and relocation counts agree with trusted
//! inspection tools.
//!
//! Objects come from a real `cargo build` rather than checked-in fixtures, so
//! the tests track whatever the current toolchain actually emits.

use blinker_macho::{parse_object_file, Arm64RelocationKind, ObjectId, SectionKind};
use blinker_test_support::{catalog, RustFixture};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a fixture and return the object files it produced.
fn objects_from_fixture(tag: &str) -> (RustFixture, Vec<PathBuf>) {
    let fixture = catalog()
        .into_iter()
        .find(|k| k.tag == tag)
        .unwrap_or_else(|| panic!("fixture `{tag}` exists"))
        .build()
        .expect("fixture is creatable");

    let build = fixture.build_with_system_linker().expect("cargo runs");
    assert!(build.success, "fixture build failed:\n{}", build.stderr);

    let deps = fixture
        .path()
        .join("target/aarch64-apple-darwin/debug/deps");
    let mut objects: Vec<PathBuf> = std::fs::read_dir(&deps)
        .expect("deps directory exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("o"))
        .collect();
    objects.sort();

    assert!(
        !objects.is_empty(),
        "no object files found in {}",
        deps.display()
    );
    (fixture, objects)
}

/// Count lines of a command's stdout matching a predicate.
fn count_lines(program: &str, args: &[&str], keep: impl Fn(&str) -> bool) -> usize {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program} runs: {e}"));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| keep(l))
        .count()
}

/// `otool -l` prints one bare `Section` line per section.
fn otool_section_count(path: &Path) -> usize {
    count_lines("otool", &["-l", &path.to_string_lossy()], |l| {
        l.trim() == "Section"
    })
}

/// `otool -r` prints one row per relocation, each starting with an 8-digit
/// hex address.
fn otool_relocation_count(path: &Path) -> usize {
    count_lines("otool", &["-r", &path.to_string_lossy()], |l| {
        let first = l.split_whitespace().next().unwrap_or("");
        first.len() == 8 && first.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// `nm -a` lists every symbol table entry; `nm -u` lists the undefined ones.
fn nm_symbol_count(path: &Path, undefined_only: bool) -> usize {
    let flag = if undefined_only { "-u" } else { "-a" };
    count_lines("nm", &[flag, &path.to_string_lossy()], |l| {
        !l.trim().is_empty()
    })
}

#[test]
fn section_symbol_and_relocation_counts_match_the_system_tools() {
    let (_fixture, objects) = objects_from_fixture("multimod");
    let mut checked = 0;

    for path in &objects {
        let object = parse_object_file(path, ObjectId(0))
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));

        let name = path
            .file_name()
            .expect("object has a name")
            .to_string_lossy();

        assert_eq!(
            object.sections.len(),
            otool_section_count(path),
            "section count disagrees with otool for {name}"
        );
        assert_eq!(
            object.symbols.len(),
            nm_symbol_count(path, false),
            "symbol count disagrees with nm for {name}"
        );
        assert_eq!(
            object.undefined_symbols().count(),
            nm_symbol_count(path, true),
            "undefined symbol count disagrees with nm -u for {name}"
        );
        assert_eq!(
            object.relocations.len(),
            otool_relocation_count(path),
            "relocation count disagrees with otool -r for {name}"
        );
        checked += 1;
    }

    assert!(checked > 0, "no objects were checked");
}

/// Every object a real Rust build produces must parse. A failure here means we
/// have hit a Mach-O feature the parser does not model — which must surface as
/// an error rather than as a silently degraded parse.
#[test]
fn every_object_from_a_real_build_parses() {
    for tag in ["multimod", "generics", "cdep"] {
        let (_fixture, objects) = objects_from_fixture(tag);
        for path in &objects {
            if let Err(err) = parse_object_file(path, ObjectId(0)) {
                panic!("fixture `{tag}`: {err}");
            }
        }
    }
}

/// The relocation set must stay within what the census established. A new kind
/// appearing means the toolchain changed and M2's relocation engine needs to
/// grow to match — a signal worth failing on rather than absorbing.
#[test]
fn relocations_stay_within_the_censused_set() {
    let (_fixture, objects) = objects_from_fixture("generics");
    let mut seen = std::collections::BTreeSet::new();

    for path in &objects {
        let object = parse_object_file(path, ObjectId(0)).expect("parses");
        for relocation in &object.relocations {
            seen.insert(relocation.kind);
        }
    }

    let censused = [
        Arm64RelocationKind::Unsigned,
        Arm64RelocationKind::Subtractor,
        Arm64RelocationKind::Branch26,
        Arm64RelocationKind::Page21,
        Arm64RelocationKind::PageOff12,
        Arm64RelocationKind::GotLoadPage21,
        Arm64RelocationKind::GotLoadPageOff12,
        Arm64RelocationKind::PointerToGot,
        Arm64RelocationKind::TlvpLoadPage21,
        Arm64RelocationKind::TlvpLoadPageOff12,
    ];
    for kind in &seen {
        assert!(
            censused.contains(kind),
            "{kind} appeared but is outside the censused set"
        );
    }
    assert!(!seen.is_empty(), "expected some relocations");
}

/// Debug and unwind sections must be recognised as such rather than falling
/// into a generic bucket: both need distinct treatment during layout, and
/// misclassifying them would not surface until output is being written.
#[test]
fn debug_and_unwind_sections_are_classified() {
    let (_fixture, objects) = objects_from_fixture("multimod");

    let mut saw_code = false;
    let mut saw_debug = false;
    for path in &objects {
        let object = parse_object_file(path, ObjectId(0)).expect("parses");
        for section in &object.sections {
            match section.kind {
                SectionKind::Code => saw_code = true,
                SectionKind::Debug => saw_debug = true,
                _ => {}
            }
            // Whatever the kind, a section claiming file bytes must be
            // consistent about it.
            if section.kind == SectionKind::Bss {
                assert!(
                    !section.has_file_bytes(),
                    "{} is zero-filled but claims file bytes",
                    section.qualified_name()
                );
            }
        }
        if object.metadata.has_debug_info {
            assert!(
                object.sections.iter().any(|s| s.kind == SectionKind::Debug),
                "metadata claims debug info but no debug section was classified"
            );
        }
    }

    assert!(saw_code, "expected at least one code section");
    // Debug builds carry DWARF; its absence would mean the classifier missed it.
    assert!(saw_debug, "expected debug sections in a debug build");
}

/// Symbols that name a section must name one that exists — an off-by-one in
/// Mach-O's one-based section numbering would show up here.
#[test]
fn symbol_section_references_are_in_range() {
    let (_fixture, objects) = objects_from_fixture("multimod");

    for path in &objects {
        let object = parse_object_file(path, ObjectId(0)).expect("parses");
        for symbol in &object.symbols {
            if let Some(section_id) = symbol.section {
                assert!(
                    object.section(section_id).is_some(),
                    "symbol `{}` in {} references section {:?}, which does not exist",
                    symbol.name,
                    path.display(),
                    section_id
                );
            }
        }
        for relocation in &object.relocations {
            assert!(
                object.section(relocation.section).is_some(),
                "relocation {:?} references a section that does not exist",
                relocation.id
            );
        }
    }
}
