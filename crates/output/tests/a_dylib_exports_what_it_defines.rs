//! What is actually in the export trie a dylib carries.
//!
//! The trie is the only way anything finds a symbol in this image: `dlsym`, a
//! later link against it, and dyld's own two-level bindings all walk it. The
//! symbol table is not consulted for any of that, so a dylib whose `nm` output
//! looks perfect and whose trie is empty exports nothing at all.
//!
//! Apple's `dyld_info -exports` cannot answer this question: given an image
//! with no trie it prints the symbol table instead, so it reports the same
//! names either way. This file therefore reads the load command, finds the
//! bytes, and walks them — a decoder written against the format rather than
//! against the encoder, in a different file from it.

use blinker_layout::InputPlacement;
use blinker_macho::{ObjectId, SectionId, SectionKind};
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Image, ImageBuilder};

fn section(index: u32, segment: &str, name: &str, kind: SectionKind, size: u64) -> InputPlacement {
    InputPlacement {
        object: ObjectId(0),
        section: SectionId(index),
        segment: segment.into(),
        name: name.into(),
        kind,
        size,
        alignment: 4,
    }
}

/// Three exports sharing prefixes, because that is what the trie compresses:
/// `_answer`, `_answer_twice` and `_and_again` force a branch, a terminal that
/// is also an interior node, and an edge that splits mid-label.
fn sample_dylib() -> Image {
    let mut builder = ImageBuilder::new();
    builder.dylib_output("/usr/local/lib/libsample.dylib");
    builder.input(section(0, "__TEXT", "__text", SectionKind::Code, 64));
    builder
        .symbols()
        .add(OutputSymbol::exported("_answer", 1, 0x4000))
        .add(OutputSymbol::exported("_answer_twice", 1, 0x4010))
        .add(OutputSymbol::exported("_and_again", 1, 0x4020))
        .add(OutputSymbol::local("_private", 1, 0x4030))
        .add(OutputSymbol::undefined("_malloc", 1));
    builder.content(0, vec![0x1F; 64]);
    builder.build().expect("dylib builds")
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// The `export_off`/`export_size` pair from `LC_DYLD_INFO_ONLY`, by walking the
/// load commands the way dyld does.
fn export_range(image: &[u8]) -> (u32, u32) {
    const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
    let count = read_u32(image, 16);
    let mut at = 32;
    for _ in 0..count {
        let command = read_u32(image, at);
        let size = read_u32(image, at + 4) as usize;
        if command == LC_DYLD_INFO_ONLY {
            // Words 10 and 11 of the command: export_off, export_size.
            return (read_u32(image, at + 40), read_u32(image, at + 44));
        }
        at += size;
    }
    panic!("no LC_DYLD_INFO_ONLY in the image");
}

fn uleb(bytes: &[u8], at: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

/// Every symbol in the trie, as `(name, flags, address)`.
///
/// A node is a ULEB terminal size, that many payload bytes, a child count, and
/// then per child a NUL-terminated edge label and the *absolute* offset of the
/// child node. Recursion here mirrors the walk dyld performs.
fn walk(trie: &[u8], at: usize, prefix: &str, found: &mut Vec<(String, u64, u64)>) {
    let mut cursor = at;
    let terminal = uleb(trie, &mut cursor) as usize;
    if terminal > 0 {
        let mut payload = cursor;
        let flags = uleb(trie, &mut payload);
        let address = uleb(trie, &mut payload);
        found.push((prefix.to_string(), flags, address));
    }
    cursor += terminal;
    let children = trie[cursor];
    cursor += 1;
    for _ in 0..children {
        let end = cursor
            + trie[cursor..]
                .iter()
                .position(|&b| b == 0)
                .expect("a label is NUL-terminated");
        let label = std::str::from_utf8(&trie[cursor..end]).expect("a label is UTF-8");
        cursor = end + 1;
        let child = uleb(trie, &mut cursor) as usize;
        walk(trie, child, &format!("{prefix}{label}"), found);
    }
}

fn exports_of(image: &[u8]) -> Vec<(String, u64, u64)> {
    let (offset, size) = export_range(image);
    if size == 0 {
        return Vec::new();
    }
    let trie = &image[offset as usize..(offset + size) as usize];
    let mut found = Vec::new();
    walk(trie, 0, "", &mut found);
    found.sort();
    found
}

/// The property: every externally defined symbol is in the trie, at the offset
/// from the header that dyld will add its slide to — and nothing else is.
#[test]
fn every_exported_symbol_is_in_the_trie_and_nothing_else_is() {
    let image = sample_dylib();
    let found = exports_of(&image.bytes);

    let names: Vec<&str> = found.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, ["_and_again", "_answer", "_answer_twice"]);

    let address = |wanted: &str| {
        found
            .iter()
            .find(|(name, _, _)| name == wanted)
            .map(|(_, _, address)| *address)
            .expect("present")
    };
    // A dylib is laid out from zero, so the symbol table's value *is* the
    // offset from the header. This is the assertion that would fail if the
    // image were ever based above __PAGEZERO again.
    assert_eq!(address("_answer"), 0x4000);
    assert_eq!(address("_answer_twice"), 0x4010);
    assert_eq!(address("_and_again"), 0x4020);

    // Regular definitions, none of them weak or absolute.
    for (name, flags, _) in &found {
        assert_eq!(
            *flags, 0,
            "{name} carries unexpected export flags {flags:#x}"
        );
    }
}

/// The trie is reachable: inside `__LINKEDIT`, and pointed at by the load
/// command dyld reads. An encoder whose output nothing points at is a buffer.
#[test]
fn the_trie_is_where_the_load_command_says_it_is() {
    let image = sample_dylib();
    let (offset, size) = export_range(&image.bytes);
    assert!(size > 0, "the dylib carries no export trie");

    let link_edit = image
        .layout
        .segment("__LINKEDIT")
        .expect("every image has one");
    // The segment's recorded size is the reservation layout made; the emitted
    // one is larger, so the lower bound is what can be asserted here.
    assert!(
        u64::from(offset) >= link_edit.file_offset,
        "the trie is below __LINKEDIT, in mapped content"
    );
    assert!(
        (offset + size) as usize <= image.bytes.len(),
        "the trie runs past the end of the image"
    );
    // Eight-byte aligned, like every other stream dyld reads through
    // pointer-sized loads.
    assert_eq!(offset % 8, 0, "the trie is mis-aligned");
}

/// An executable carries no trie, and must keep carrying none: blinker has
/// never written one, and starting to would change every executable it
/// produces — including the byte-identity the incremental cache rests on.
#[test]
fn an_executable_carries_no_trie() {
    let mut builder = ImageBuilder::new();
    builder.input(section(0, "__TEXT", "__text", SectionKind::Code, 64));
    builder.entry_offset(0x4000);
    builder
        .symbols()
        .add(OutputSymbol::exported("_main", 1, 0x1_0000_4000));
    builder.content(0, vec![0x1F; 64]);
    let image = builder.build().expect("image builds");

    assert_eq!(
        export_range(&image.bytes),
        (0, 0),
        "the executable grew an export trie"
    );
}
