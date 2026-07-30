//! Archive parsing against real `.rlib` and `.a` files.
//!
//! Rust `.rlib` files are the bulk of a link's inputs — 186 of ~320 in the
//! projects measured during M0 — so this exercises the toolchain's own
//! libraries rather than constructed archives.

use blinker_archive::{index_archive_file, member_data, MemberKind};
use blinker_macho::{parse_object, ObjectId};
use blinker_test_support::catalog;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate an rlib from the active Rust toolchain's sysroot.
fn toolchain_rlib(prefix: &str) -> Option<PathBuf> {
    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let lib_dir = Path::new(&sysroot).join("lib/rustlib/aarch64-apple-darwin/lib");

    std::fs::read_dir(lib_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("rlib")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
}

/// Member names according to `ar t`, the tool that was right first.
fn ar_member_names(path: &Path) -> Vec<String> {
    let output = Command::new("ar")
        .arg("t")
        .arg(path)
        .output()
        .expect("ar runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

#[test]
fn member_list_matches_ar() {
    let Some(rlib) = toolchain_rlib("libstd-") else {
        panic!("no libstd rlib found in the toolchain sysroot");
    };

    let (index, _data) = index_archive_file(&rlib).expect("libstd rlib indexes");
    let ours: Vec<&str> = index.members.iter().map(|m| m.name.as_str()).collect();

    // `ar t` lists the symbol table as a member; the underlying reader treats
    // it as an index and does not surface it. Compare against `ar` minus that
    // entry, so the two agree on what an actual member is.
    let theirs: Vec<String> = ar_member_names(&rlib)
        .into_iter()
        .filter(|n| !n.starts_with("__.SYMDEF"))
        .collect();

    assert_eq!(
        ours, theirs,
        "member list disagrees with `ar t`\nours: {ours:?}\nar:   {theirs:?}"
    );
}

/// The finding that motivates deliberate skipping: an rlib carries Rust
/// metadata that is not an object, and handing it to the Mach-O parser would
/// produce a confusing error.
#[test]
fn rust_metadata_members_are_identified_and_skipped() {
    let Some(rlib) = toolchain_rlib("libstd-") else {
        panic!("no libstd rlib found");
    };

    let (index, data) = index_archive_file(&rlib).expect("indexes");
    assert!(index.is_rlib(), "libstd should be recognised as an rlib");

    let metadata: Vec<&str> = index
        .members
        .iter()
        .filter(|m| m.kind == MemberKind::RustMetadata)
        .map(|m| m.name.as_str())
        .collect();
    assert!(
        !metadata.is_empty(),
        "expected metadata members, found none"
    );

    // The decisive check, and the reason skipping must be name-based.
    //
    // `lib.rmeta` is a *genuine* Mach-O arm64 object — rustc wraps crate
    // metadata in an object container so that `ar` and linkers handle it like
    // any other member. It parses cleanly, so content-based detection would
    // accept it and link the whole metadata blob into the binary as a section.
    // Only the name distinguishes it.
    for member in index
        .members
        .iter()
        .filter(|m| m.kind == MemberKind::RustMetadata)
    {
        let bytes = member_data(&data, member, &rlib).expect("member is in range");
        let parsed = parse_object(bytes, &rlib, Some(&member.name), ObjectId(0))
            .expect("rustc wraps metadata in a real Mach-O object, so it parses");

        // It carries no code — it is a container, not compiled output.
        assert!(
            parsed
                .sections
                .iter()
                .all(|s| s.kind != blinker_macho::SectionKind::Code),
            "`{}` contains code; it may not be metadata after all",
            member.name
        );
        assert!(
            parsed.sections.iter().any(|s| s.name.contains("rmeta")),
            "expected an .rmeta section in `{}`",
            member.name
        );

        // And the classifier must exclude it regardless of it being parseable.
        assert!(
            !member.kind.is_linkable(),
            "`{}` must be skipped by name",
            member.name
        );
    }
}

/// Every member classified as an object must actually be one — the converse of
/// the check above, and the one that would catch an over-eager skip rule.
#[test]
fn linkable_members_really_are_mach_o_objects() {
    let mut checked = 0;

    for prefix in ["libstd-", "libcore-", "liballoc-"] {
        let Some(rlib) = toolchain_rlib(prefix) else {
            continue;
        };
        let (index, data) = index_archive_file(&rlib).expect("indexes");

        for member in index.linkable_members() {
            let bytes = member_data(&data, member, &rlib).expect("member is in range");
            let object = parse_object(bytes, &rlib, Some(&member.name), ObjectId(0))
                .unwrap_or_else(|e| panic!("`{}` failed to parse: {e}", member.name));

            assert_eq!(
                object.metadata.member.as_deref(),
                Some(member.name.as_str())
            );
            checked += 1;
        }
    }

    assert!(checked > 0, "no linkable members were checked");
}

/// The archive symbol table is what makes resolution fast; when present, its
/// claims must be true.
#[test]
fn symbol_table_entries_point_at_members_that_define_them() {
    let Some(rlib) = toolchain_rlib("libcore-") else {
        panic!("no libcore rlib found");
    };
    let (index, data) = index_archive_file(&rlib).expect("indexes");

    if index.symbol_map.is_empty() {
        // Legal — resolution then falls back to scanning members.
        return;
    }

    // Spot-check a sample rather than the whole table, which is large.
    for (symbol, member_id) in index.symbol_map.iter().take(50) {
        let member = index
            .member(*member_id)
            .unwrap_or_else(|| panic!("symbol `{symbol}` names a member that does not exist"));
        assert!(
            member.kind.is_linkable(),
            "symbol `{symbol}` resolves to non-linkable member `{}`",
            member.name
        );

        let bytes = member_data(&data, member, &rlib).expect("in range");
        let object = parse_object(bytes, &rlib, Some(&member.name), ObjectId(0)).expect("parses");
        assert!(
            object.exported_symbols().any(|s| s.name == *symbol),
            "member `{}` does not define `{symbol}` as the symbol table claims",
            member.name
        );
    }
}

/// A `.a` built by a build script — the plain-archive path, distinct from
/// rlibs, which arrives via `cargo:rustc-link-lib=static=`.
#[test]
fn plain_static_archives_from_a_build_script_are_indexed() {
    let fixture = catalog()
        .into_iter()
        .find(|k| k.tag == "cdep")
        .expect("cdep fixture exists")
        .build()
        .expect("fixture is creatable");

    let build = fixture.build_with_system_linker().expect("cargo runs");
    assert!(build.success, "fixture build failed:\n{}", build.stderr);

    // The build script archives its C object into OUT_DIR.
    let mut found = false;
    let build_dir = fixture
        .path()
        .join("target/aarch64-apple-darwin/debug/build");
    for entry in walk(&build_dir) {
        if entry.extension().and_then(|e| e.to_str()) != Some("a") {
            continue;
        }
        let (index, data) = index_archive_file(&entry).expect("indexes");
        assert!(!index.is_rlib(), "a C archive should not look like an rlib");

        for member in index.linkable_members() {
            let bytes = member_data(&data, member, &entry).expect("in range");
            parse_object(bytes, &entry, Some(&member.name), ObjectId(0))
                .unwrap_or_else(|e| panic!("`{}` failed to parse: {e}", member.name));
            found = true;
        }
    }

    assert!(found, "no static archive members were found or parsed");
}

/// Recursively list files under a directory.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn malformed_archives_are_rejected_rather_than_partially_read() {
    let Some(rlib) = toolchain_rlib("libcore-") else {
        panic!("no libcore rlib found");
    };
    let data = std::fs::read(&rlib).expect("readable");

    // A truncated archive may still index — the headers near the front survive
    // — but any member whose data was cut off must be refused by `member_data`
    // rather than returning a short slice. Handing the Mach-O parser a
    // truncated object would turn a clear error into a confusing one.
    for fraction in [1, 2, 4, 8, 16, 64] {
        let truncated = &data[..data.len() / fraction];
        let Ok(index) = blinker_archive::index_archive(truncated, &rlib) else {
            continue;
        };
        for member in &index.members {
            match member_data(truncated, member, &rlib) {
                Ok(bytes) => assert_eq!(
                    bytes.len() as u64,
                    member.size,
                    "`{}` returned a short slice instead of an error",
                    member.name
                ),
                Err(err) => assert!(
                    matches!(err, blinker_archive::ArchiveError::MemberOutOfBounds { .. }),
                    "unexpected error for `{}`: {err}",
                    member.name
                ),
            }
        }
    }
}
