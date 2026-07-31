//! The debug map's structure, checked against the linker that defines it.
//!
//! `crates/cli/tests/backtraces_name_the_source_line.rs` checks that the
//! feature works end to end, which is the property that matters and the slow
//! one. This checks the shape, on a C fixture, against `ld`'s own output for
//! the same object — because "it works on my program" and "it is the table
//! every consumer expects" are different claims, and the second is what makes
//! the first keep being true.
//!
//! What `ld` emits, and what these assert blinker emits:
//!
//! ```text
//!   SO    "<dir>/"        opens the compilation unit
//!   SO    "<file>"
//!   OSO   "<path.o>"      n_desc 1, n_value the object's mtime
//!     BNSYM / FUN "_name" / FUN "" (size) / ENSYM     per function
//!     GSYM  "_global"                                 global data
//!     STSYM "_static"                                 static data
//!   SO    ""              closes it
//! ```

use blinker_link::{link_to_file, LinkRequest};
use blinker_test_support::Scratch;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

/// A program with one function of each kind the map distinguishes.
const PROGRAM: &str = r#"
static int helper(int x) { return x * 3; }
int global_data = 7;
static int static_data = 11;
int main(void) { return helper(global_data) + static_data; }
"#;

fn compile(scratch: &Scratch) -> PathBuf {
    let source = scratch.write("a.c", PROGRAM).expect("writable");
    let object = scratch.join("a.o");
    let status = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-g", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    assert!(status.success(), "cc failed");
    object
}

/// One stab, as the table holds it.
#[derive(Debug, PartialEq, Eq)]
struct Stab {
    kind: u8,
    name: String,
    section: u8,
    desc: u16,
    value: u64,
}

mod kind {
    pub const SO: u8 = 0x64;
    pub const OSO: u8 = 0x66;
    pub const FUN: u8 = 0x24;
    pub const GSYM: u8 = 0x20;
    pub const STSYM: u8 = 0x26;
    pub const BNSYM: u8 = 0x2e;
    pub const ENSYM: u8 = 0x4e;
}

/// Every stab in an image, in table order.
///
/// Read by walking `LC_SYMTAB` directly. `nm` would print them, but it sorts
/// by address, and the order these appear in *is* half of what makes them
/// readable — a consumer walks the table forwards and pairs each `OSO` with
/// the stabs that follow it.
fn stabs_of(path: &Path) -> Vec<Stab> {
    const LC_SYMTAB: u32 = 0x2;
    const N_STAB: u8 = 0xe0;
    let bytes = std::fs::read(path).expect("readable");
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("in range"));

    let mut at = 32;
    let (mut symbol_offset, mut count, mut string_offset) = (0usize, 0usize, 0usize);
    for _ in 0..u32_at(16) {
        if u32_at(at) == LC_SYMTAB {
            symbol_offset = u32_at(at + 8) as usize;
            count = u32_at(at + 12) as usize;
            string_offset = u32_at(at + 16) as usize;
        }
        at += u32_at(at + 4) as usize;
    }
    assert!(count > 0, "the image has no symbol table");

    let mut out = Vec::new();
    for index in 0..count {
        let entry = symbol_offset + index * 16;
        let type_byte = bytes[entry + 4];
        if type_byte & N_STAB == 0 {
            continue;
        }
        let name_at = string_offset + u32_at(entry) as usize;
        let end = bytes[name_at..]
            .iter()
            .position(|&b| b == 0)
            .expect("terminated");
        out.push(Stab {
            kind: type_byte,
            name: String::from_utf8_lossy(&bytes[name_at..name_at + end]).into_owned(),
            section: bytes[entry + 5],
            desc: u16::from_le_bytes(bytes[entry + 6..entry + 8].try_into().expect("in range")),
            value: u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().expect("in range")),
        });
    }
    out
}

/// Link the fixture both ways, so every assertion can be stated against what
/// `ld` did with the same object.
fn both_ways(tag: &str) -> (Vec<Stab>, Vec<Stab>) {
    let scratch = Scratch::dir(tag).expect("scratch");
    let object = compile(&scratch);

    let ours = scratch.join("ours");
    link_to_file(&LinkRequest::new(vec![object.clone()]), &ours).expect("the link succeeds");

    let theirs = scratch.join("theirs");
    let status = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-g"])
        .arg(&object)
        .arg("-o")
        .arg(&theirs)
        .status()
        .expect("cc runs");
    assert!(status.success(), "the system link failed");

    (stabs_of(&ours), stabs_of(&theirs))
}

/// The map exists, opens with a source and object file, and closes.
#[test]
fn the_map_brackets_each_object_with_so_and_oso() {
    let (ours, theirs) = both_ways("debugmap-shape");
    assert!(
        !ours.is_empty(),
        "no debug map at all; ld emitted {} stabs for the same object",
        theirs.len()
    );

    let kinds: Vec<u8> = ours.iter().map(|s| s.kind).collect();
    assert_eq!(
        &kinds[..3],
        &[kind::SO, kind::SO, kind::OSO],
        "the unit does not open with SO, SO, OSO"
    );
    assert_eq!(
        kinds.last(),
        Some(&kind::SO),
        "the unit is never closed with a terminating SO"
    );

    let oso = &ours[2];
    assert_eq!(oso.desc, 1, "OSO n_desc must be 1, the module count");
    assert!(
        oso.name.ends_with("a.o"),
        "the OSO does not name the object: {}",
        oso.name
    );
    assert!(
        oso.value > 0,
        "the OSO carries no timestamp, so a consumer cannot tell it is stale"
    );
    // Located rather than indexed: `ld` opens its table with an extra empty
    // `SO` before the first unit, so the OSO is not at the same index in both.
    let their_oso = theirs
        .iter()
        .find(|stab| stab.kind == kind::OSO)
        .expect("ld emitted no OSO for an object built with -g");
    assert_eq!(
        oso.value, their_oso.value,
        "blinker and ld disagree about the object's mtime"
    );
}

/// A function is described by its address *and* its size, and the size is the
/// part that has to be computed rather than copied.
///
/// Checked against `ld`'s numbers for the same object: `main` and `helper` are
/// the same machine code laid out by two linkers, so the sizes must agree even
/// though the addresses need not.
#[test]
fn each_function_carries_its_address_and_its_size() {
    let (ours, theirs) = both_ways("debugmap-functions");

    // FUN pairs: a named one at the address, then an unnamed one holding size.
    let sizes = |stabs: &[Stab]| -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for (at, stab) in stabs.iter().enumerate() {
            if stab.kind == kind::FUN && !stab.name.is_empty() {
                let size = stabs
                    .get(at + 1)
                    .filter(|next| next.kind == kind::FUN && next.name.is_empty())
                    .map(|next| next.value)
                    .unwrap_or_else(|| panic!("{} has no size stab after it", stab.name));
                out.push((stab.name.clone(), size));
            }
        }
        out.sort();
        out
    };

    let ours_sizes = sizes(&ours);
    assert_eq!(
        ours_sizes,
        sizes(&theirs),
        "blinker and ld disagree about the functions or their sizes"
    );
    assert!(
        ours_sizes.iter().any(|(name, _)| name == "_helper"),
        "a static function is missing from the map: {ours_sizes:?}"
    );
    assert!(
        ours_sizes.iter().all(|(_, size)| *size > 0),
        "a function was given a size of zero: {ours_sizes:?}"
    );

    // Each function is bracketed: BNSYM, FUN, FUN(size), ENSYM. `dsymutil`
    // recovers a function's extent from the brackets, so a missing one shows
    // up as a size of zero rather than as a rejected file.
    for (at, stab) in ours.iter().enumerate() {
        if stab.kind != kind::FUN || stab.name.is_empty() {
            continue;
        }
        assert_eq!(
            ours[at - 1].kind,
            kind::BNSYM,
            "{} is not preceded by BNSYM",
            stab.name
        );
        assert_eq!(
            ours[at + 2].kind,
            kind::ENSYM,
            "{} is not followed by ENSYM",
            stab.name
        );
    }

    // And the address a FUN carries must be where the symbol actually is.
    for stab in ours
        .iter()
        .filter(|s| s.kind == kind::FUN && !s.name.is_empty())
    {
        assert!(
            stab.value >= 0x1_0000_0000,
            "{} is at {:#x}, which is not an address in the image",
            stab.name,
            stab.value
        );
        assert_ne!(stab.section, 0, "{} claims no section", stab.name);
    }
}

/// Data is described too, and the two kinds are distinguished: a global gets a
/// `GSYM` with no address, a static gets an `STSYM` at its address.
///
/// Emitting one where the other belongs is invisible until a debugger prints
/// the wrong variable, which is why both are named here rather than counted.
#[test]
fn global_and_static_data_are_told_apart() {
    let (ours, _) = both_ways("debugmap-data");
    let find = |kind: u8, name: &str| ours.iter().find(|s| s.kind == kind && s.name == name);

    let global = find(kind::GSYM, "_global_data").expect("no GSYM for the global");
    assert_eq!(
        global.value, 0,
        "a GSYM carries no address; the symbol table has it"
    );

    let static_data = find(kind::STSYM, "_static_data").expect("no STSYM for the static");
    assert!(
        static_data.value >= 0x1_0000_0000,
        "the STSYM is not at an address in the image: {:#x}",
        static_data.value
    );
    assert_ne!(static_data.section, 0, "the STSYM claims no section");

    assert!(
        find(kind::GSYM, "_static_data").is_none() && find(kind::STSYM, "_global_data").is_none(),
        "the two kinds of data were swapped"
    );
}

/// `dsymutil` is the consumer that defines the format, and it reads the map
/// back rather than trusting it. This is the check that the table is not
/// merely shaped right but usable.
#[test]
fn dsymutil_reads_the_map_back() {
    let scratch = Scratch::dir("debugmap-dsymutil").expect("scratch");
    let object = compile(&scratch);
    let out = scratch.join("program");
    link_to_file(&LinkRequest::new(vec![object]), &out).expect("the link succeeds");

    let dumped = Command::new("dsymutil")
        .arg("-dump-debug-map")
        .arg(&out)
        .output()
        .expect("dsymutil runs");
    assert!(
        dumped.status.success(),
        "dsymutil rejected the image:\n{}",
        String::from_utf8_lossy(&dumped.stderr)
    );
    let map = String::from_utf8_lossy(&dumped.stdout);
    for symbol in ["_main", "_helper", "_global_data", "_static_data"] {
        assert!(
            map.contains(symbol),
            "dsymutil's debug map is missing {symbol}:\n{map}"
        );
    }
    // BNSYM/ENSYM brackets are what let dsymutil recover a size; if they were
    // wrong it would report zero for functions.
    assert!(
        !map.contains("sym: _main, objAddr: 0x0, binAddr: 0x100000000, size: 0x0"),
        "dsymutil recovered no size for main:\n{map}"
    );
}
