//! Load command emission.
//!
//! Every load command is `(cmd, cmdsize, …)`, and `cmdsize` must be exact:
//! dyld walks the command list by adding `cmdsize` to its cursor, so a wrong
//! size does not produce a bad command — it desynchronises the walk and every
//! subsequent command is read from the wrong offset. Each writer here therefore
//! asserts its own emitted size in tests.

use blinker_layout::{Layout, OutputSection, OutputSegment};

use crate::format::*;

/// A load command's fixed sizes, from `<mach-o/loader.h>`.
pub mod sizes {
    /// `segment_command_64` without any `section_64` entries.
    pub const SEGMENT_64: usize = 72;
    /// One `section_64`.
    pub const SECTION_64: usize = 80;
    pub const SYMTAB: usize = 24;
    pub const DYSYMTAB: usize = 80;
    pub const DYLD_INFO: usize = 48;
    pub const UUID: usize = 24;
    pub const BUILD_VERSION: usize = 24;
    /// `build_version_command` plus one `build_tool_version`.
    pub const BUILD_VERSION_WITH_TOOL: usize = 32;
    pub const SOURCE_VERSION: usize = 16;
    pub const MAIN: usize = 24;
    pub const LINKEDIT_DATA: usize = 16;
}

/// Byte ranges of the `__LINKEDIT` content, which several commands point at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkEditLayout {
    pub rebase_offset: u32,
    pub rebase_size: u32,
    pub bind_offset: u32,
    pub bind_size: u32,
    pub lazy_bind_offset: u32,
    pub lazy_bind_size: u32,
    pub export_offset: u32,
    pub export_size: u32,
    pub symbol_offset: u32,
    pub symbol_count: u32,
    pub string_offset: u32,
    pub string_size: u32,
    pub function_starts_offset: u32,
    pub function_starts_size: u32,
    pub data_in_code_offset: u32,
    pub data_in_code_size: u32,
    pub code_signature_offset: u32,
    pub code_signature_size: u32,
}

/// Emit `LC_SEGMENT_64` and its `section_64` entries.
///
/// `cmdsize` covers the command *and* its sections, which is the size dyld
/// steps over.
pub fn write_segment(writer: &mut Writer, segment: &OutputSegment, sections: &[OutputSection]) {
    let members: Vec<&OutputSection> = segment
        .sections
        .iter()
        .filter_map(|&i| sections.get(i))
        .collect();
    let command_size = sizes::SEGMENT_64 + members.len() * sizes::SECTION_64;

    writer
        .u32(LC_SEGMENT_64)
        .u32(command_size as u32)
        .name16(&segment.name)
        .u64(segment.vm_address)
        .u64(segment.vm_size)
        .u64(segment.file_offset)
        .u64(segment.file_size)
        .u32(segment.max_protection.0)
        .u32(segment.init_protection.0)
        .u32(members.len() as u32)
        .u32(segment_flags(&segment.name));

    for section in members {
        write_section(writer, section, &segment.name);
    }
}

/// Segment flags.
///
/// `__DATA_CONST` must carry `SG_READ_ONLY`; dyld rejects the image outright
/// without it, which is a load failure rather than a warning.
fn segment_flags(name: &str) -> u32 {
    if name == "__DATA_CONST" {
        SG_READ_ONLY
    } else {
        0
    }
}

/// Emit one `section_64` entry.
fn write_section(writer: &mut Writer, section: &OutputSection, segment_name: &str) {
    writer
        .name16(&section.name)
        .name16(segment_name)
        .u64(section.vm_address)
        .u64(section.size)
        // A zero-filled section still records an offset field; it is ignored,
        // and zero is what the toolchain writes.
        .u32(section.file_offset.unwrap_or(0) as u32)
        .u32(alignment_exponent(section.alignment))
        // `reloff`/`nreloc`: a linked image carries no relocation entries.
        .u32(0)
        .u32(0)
        .u32(section_flags(section))
        // `reserved1` indexes the indirect symbol table for pointer and stub
        // sections; `reserved2` is the stub size. Both are zero until stubs
        // are synthesised.
        .u32(0)
        .u32(0)
        .u32(0);
}

/// Mach-O stores alignment as a power-of-two exponent, not a byte count.
///
/// Writing the byte count directly would ask for an alignment of 2^16 when 16
/// was meant.
pub fn alignment_exponent(alignment: u64) -> u32 {
    if alignment <= 1 {
        return 0;
    }
    alignment.trailing_zeros()
}

/// Section type and attribute bits for an output section.
fn section_flags(section: &OutputSection) -> u32 {
    use blinker_macho::SectionKind;
    match section.kind {
        SectionKind::Code => S_REGULAR | S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS,
        SectionKind::Bss => S_ZEROFILL,
        // Every thread-local section needs its own *type*, not just placement
        // in __DATA. dyld computes the size of the per-thread block from the
        // sections typed as thread-local data; with them left S_REGULAR it
        // computes zero and rejects the image with "malformed thread-local,
        // offset=… is larger than total size=0x0".
        SectionKind::ThreadLocal if section.name == "__thread_vars" => S_THREAD_LOCAL_VARIABLES,
        SectionKind::ThreadLocal if section.name == "__thread_bss" => S_THREAD_LOCAL_ZEROFILL,
        SectionKind::ThreadLocal if section.name == "__thread_ptrs" => {
            S_THREAD_LOCAL_VARIABLE_POINTERS
        }
        SectionKind::ThreadLocal => S_THREAD_LOCAL_REGULAR,
        // Initialiser and terminator tables. dyld finds these by section
        // *type*, not by name, so leaving them `S_REGULAR` produces a program
        // whose static constructors never run — it links, loads, and returns
        // the right exit code with none of its globals constructed, which is
        // the worst way for this to be wrong.
        _ if section.name == "__mod_init_func" => S_MOD_INIT_FUNC_POINTERS,
        _ if section.name == "__mod_term_func" => S_MOD_TERM_FUNC_POINTERS,
        _ if section.name == "__cstring" => S_CSTRING_LITERALS,
        _ if section.name == "__got" => S_NON_LAZY_SYMBOL_POINTERS,
        _ if section.name == "__la_symbol_ptr" => S_LAZY_SYMBOL_POINTERS,
        _ if section.name == "__stubs" => S_SYMBOL_STUBS | S_ATTR_PURE_INSTRUCTIONS,
        _ => S_REGULAR,
    }
}

/// Emit `LC_DYLD_INFO_ONLY`.
///
/// The classic opcode-stream strategy. Verified as what the toolchain emits at
/// a macOS 11 deployment target — chained fixups only appear at 12 and above.
pub fn write_dyld_info(writer: &mut Writer, link_edit: &LinkEditLayout) {
    writer
        .u32(LC_DYLD_INFO_ONLY)
        .u32(sizes::DYLD_INFO as u32)
        .u32(link_edit.rebase_offset)
        .u32(link_edit.rebase_size)
        .u32(link_edit.bind_offset)
        .u32(link_edit.bind_size)
        // Weak binding is unused: nothing in a Rust executable needs it.
        .u32(0)
        .u32(0)
        .u32(link_edit.lazy_bind_offset)
        .u32(link_edit.lazy_bind_size)
        .u32(link_edit.export_offset)
        .u32(link_edit.export_size);
}

/// Emit `LC_SYMTAB`.
pub fn write_symtab(writer: &mut Writer, link_edit: &LinkEditLayout) {
    writer
        .u32(LC_SYMTAB)
        .u32(sizes::SYMTAB as u32)
        .u32(link_edit.symbol_offset)
        .u32(link_edit.symbol_count)
        .u32(link_edit.string_offset)
        .u32(link_edit.string_size);
}

/// Ranges of the symbol table by category, for `LC_DYSYMTAB`.
///
/// The symbol table must be *sorted* into these three groups: dyld and the
/// debugger index into it by range, so an unsorted table is misread rather
/// than rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SymbolGroups {
    pub local_index: u32,
    pub local_count: u32,
    pub external_index: u32,
    pub external_count: u32,
    pub undefined_index: u32,
    pub undefined_count: u32,
    pub indirect_offset: u32,
    pub indirect_count: u32,
}

/// Emit `LC_DYSYMTAB`.
pub fn write_dysymtab(writer: &mut Writer, groups: &SymbolGroups) {
    writer
        .u32(LC_DYSYMTAB)
        .u32(sizes::DYSYMTAB as u32)
        .u32(groups.local_index)
        .u32(groups.local_count)
        .u32(groups.external_index)
        .u32(groups.external_count)
        .u32(groups.undefined_index)
        .u32(groups.undefined_count)
        // Table of contents, module table, external reference table: all
        // unused for an executable.
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(groups.indirect_offset)
        .u32(groups.indirect_count)
        // External and local relocation entries: none in a linked image.
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0);
}

/// Emit `LC_LOAD_DYLINKER`.
pub fn write_load_dylinker(writer: &mut Writer, path: &str) {
    // 12 bytes of fixed fields, then the path padded so the whole command is
    // 8-byte aligned.
    let command_size = command_size_with_path(12, path);
    let start = writer.len();
    writer
        .u32(LC_LOAD_DYLINKER)
        .u32(command_size as u32)
        // Offset of the path within the command.
        .u32(12)
        .bytes(path.as_bytes())
        .bytes(&[0]);
    writer.pad_to(start + command_size);
}

/// Emit `LC_LOAD_DYLIB`.
pub fn write_load_dylib(
    writer: &mut Writer,
    path: &str,
    timestamp: u32,
    current_version: u32,
    compatibility_version: u32,
) {
    write_dylib_command(
        writer,
        LC_LOAD_DYLIB,
        path,
        timestamp,
        current_version,
        compatibility_version,
    );
}

/// Emit `LC_ID_DYLIB` — the name a dylib records for itself.
///
/// Whatever links against this library copies `path` into its own
/// `LC_LOAD_DYLIB` verbatim, so this is the string dyld will search for at
/// runtime, not the path the file happens to sit at now. `ld64` defaults it to
/// the output path and `-install_name` overrides it; the two differ for
/// anything that will be installed somewhere else, which is why the flag
/// exists.
pub fn write_id_dylib(
    writer: &mut Writer,
    path: &str,
    timestamp: u32,
    current_version: u32,
    compatibility_version: u32,
) {
    write_dylib_command(
        writer,
        LC_ID_DYLIB,
        path,
        timestamp,
        current_version,
        compatibility_version,
    );
}

/// The `dylib_command` body both of the above share.
fn write_dylib_command(
    writer: &mut Writer,
    command: u32,
    path: &str,
    timestamp: u32,
    current_version: u32,
    compatibility_version: u32,
) {
    let command_size = command_size_with_path(24, path);
    let start = writer.len();
    writer
        .u32(command)
        .u32(command_size as u32)
        .u32(24)
        .u32(timestamp)
        .u32(current_version)
        .u32(compatibility_version)
        .bytes(path.as_bytes())
        .bytes(&[0]);
    writer.pad_to(start + command_size);
}

/// Size of a path-carrying load command, rounded to the alignment 64-bit
/// Mach-O requires.
///
/// **Load commands in a 64-bit image must be 8-byte aligned, not 4.** Emitting
/// a 4-aligned `LC_LOAD_DYLINKER` produces a file `otool -l` walks happily but
/// `nm` rejects outright:
///
/// ```text
/// truncated or malformed object (load command 7 cmdsize not a multiple of 8)
/// ```
///
/// The real toolchain agrees: its `LC_LOAD_DYLINKER` for `/usr/lib/dyld`
/// (13 characters) is `cmdsize 32`, not the 28 a 4-byte rounding would give.
pub fn command_size_with_path(fixed: usize, path: &str) -> usize {
    (fixed + path.len() + 1).div_ceil(8) * 8
}

/// Emit `LC_UUID`.
pub fn write_uuid(writer: &mut Writer, uuid: [u8; 16]) {
    writer.u32(LC_UUID).u32(sizes::UUID as u32).bytes(&uuid);
}

/// Emit `LC_BUILD_VERSION` with one tool entry.
///
/// Versions are packed as `xxxx.yy.zz` in `X.Y.Z` nibble form.
pub fn write_build_version(
    writer: &mut Writer,
    platform: u32,
    min_os: (u16, u8, u8),
    sdk: (u16, u8, u8),
) {
    writer
        .u32(LC_BUILD_VERSION)
        .u32(sizes::BUILD_VERSION_WITH_TOOL as u32)
        .u32(platform)
        .u32(pack_version(min_os))
        .u32(pack_version(sdk))
        // One tool entry.
        .u32(1)
        // TOOL_LD.
        .u32(3)
        .u32(pack_version((1, 0, 0)));
}

/// Pack `X.Y.Z` into Mach-O's `xxxx.yy.zz` form.
pub fn pack_version(version: (u16, u8, u8)) -> u32 {
    let (major, minor, patch) = version;
    ((major as u32) << 16) | ((minor as u32) << 8) | (patch as u32)
}

/// Emit `LC_SOURCE_VERSION`.
pub fn write_source_version(writer: &mut Writer, version: u64) {
    writer
        .u32(LC_SOURCE_VERSION)
        .u32(sizes::SOURCE_VERSION as u32)
        .u64(version);
}

/// Emit `LC_CODE_SIGNATURE`.
///
/// A `linkedit_data_command` pointing at the signature blob. It must be
/// emitted *before* the signature is computed, because the load commands are
/// inside the region the signature covers — a command added afterwards would
/// invalidate every page hash.
pub fn write_code_signature(writer: &mut Writer, link_edit: &LinkEditLayout) {
    writer
        .u32(LC_CODE_SIGNATURE)
        .u32(sizes::LINKEDIT_DATA as u32)
        .u32(link_edit.code_signature_offset)
        .u32(link_edit.code_signature_size);
}

/// Emit `LC_MAIN`.
///
/// `entry_offset` is a **file offset**, not a virtual address — the difference
/// matters, and it is why no `crt1.o` is needed: dyld computes the entry
/// address itself and calls `main` directly.
pub fn write_main(writer: &mut Writer, entry_offset: u64, stack_size: u64) {
    writer
        .u32(LC_MAIN)
        .u32(sizes::MAIN as u32)
        .u64(entry_offset)
        .u64(stack_size);
}

/// Emit one of the `linkedit_data_command` family.
pub fn write_linkedit_data(writer: &mut Writer, command: u32, offset: u32, size: u32) {
    writer
        .u32(command)
        .u32(sizes::LINKEDIT_DATA as u32)
        .u32(offset)
        .u32(size);
}

/// Total size of the load commands for a layout, for the header reservation.
///
/// Layout needs this before the commands exist, so it is computed from shape
/// rather than by emitting and measuring.
///
/// `dylib_command_bytes` is the summed size of every `LC_LOAD_DYLIB`, fixed
/// fields included — use [`command_size_with_path`] to compute each, so the
/// prediction and the emission cannot disagree about the alignment rule.
pub fn command_size_for(layout: &Layout, dylib_command_bytes: usize) -> usize {
    let shape: Vec<usize> = layout.segments.iter().map(|s| s.sections.len()).collect();
    command_size_for_shape(&shape, dylib_command_bytes)
}

/// The same size, from the shape alone.
///
/// `shape` is one entry per output segment, holding its section count — what
/// [`blinker_layout::output_shape`] produces without laying anything out.
pub fn command_size_for_shape(shape: &[usize], dylib_command_bytes: usize) -> usize {
    let segment_bytes: usize = shape
        .iter()
        .map(|sections| sizes::SEGMENT_64 + sections * sizes::SECTION_64)
        .sum();

    segment_bytes
        + sizes::DYLD_INFO
        + sizes::SYMTAB
        + sizes::DYSYMTAB
        + command_size_with_path(12, DYLD_PATH)
        + sizes::UUID
        + sizes::BUILD_VERSION_WITH_TOOL
        + sizes::SOURCE_VERSION
        + sizes::MAIN
        + dylib_command_bytes
        // LC_FUNCTION_STARTS, LC_DATA_IN_CODE, LC_CODE_SIGNATURE.
        + sizes::LINKEDIT_DATA * 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_layout::{compute_layout, InputPlacement, Protection};
    use blinker_macho::{ObjectId, SectionId, SectionKind};

    fn writer() -> Writer {
        Writer::new()
    }

    /// Mach-O stores an exponent; writing the byte count would ask for 2^16.
    #[test]
    fn alignment_is_written_as_a_power_of_two_exponent() {
        assert_eq!(alignment_exponent(1), 0);
        assert_eq!(alignment_exponent(4), 2);
        assert_eq!(alignment_exponent(16), 4);
        assert_eq!(alignment_exponent(0x4000), 14);
    }

    #[test]
    fn alignment_of_zero_is_treated_as_unaligned() {
        assert_eq!(alignment_exponent(0), 0);
    }

    /// `cmdsize` is what dyld adds to its cursor. Every command must report
    /// exactly what it wrote, or the walk desynchronises.
    #[test]
    fn every_fixed_size_command_writes_exactly_its_declared_size() {
        type Emit = Box<dyn Fn(&mut Writer)>;
        let cases: Vec<(&str, usize, Emit)> = vec![
            (
                "LC_DYLD_INFO_ONLY",
                sizes::DYLD_INFO,
                Box::new(|w: &mut Writer| write_dyld_info(w, &LinkEditLayout::default())),
            ),
            (
                "LC_SYMTAB",
                sizes::SYMTAB,
                Box::new(|w: &mut Writer| write_symtab(w, &LinkEditLayout::default())),
            ),
            (
                "LC_DYSYMTAB",
                sizes::DYSYMTAB,
                Box::new(|w: &mut Writer| write_dysymtab(w, &SymbolGroups::default())),
            ),
            (
                "LC_UUID",
                sizes::UUID,
                Box::new(|w: &mut Writer| write_uuid(w, [0; 16])),
            ),
            (
                "LC_BUILD_VERSION",
                sizes::BUILD_VERSION_WITH_TOOL,
                Box::new(|w: &mut Writer| {
                    write_build_version(w, PLATFORM_MACOS, (11, 0, 0), (26, 5, 0))
                }),
            ),
            (
                "LC_SOURCE_VERSION",
                sizes::SOURCE_VERSION,
                Box::new(|w: &mut Writer| write_source_version(w, 0)),
            ),
            (
                "LC_MAIN",
                sizes::MAIN,
                Box::new(|w: &mut Writer| write_main(w, 0x1000, 0)),
            ),
            (
                "LC_FUNCTION_STARTS",
                sizes::LINKEDIT_DATA,
                Box::new(|w: &mut Writer| write_linkedit_data(w, LC_FUNCTION_STARTS, 0, 0)),
            ),
        ];

        for (name, expected, write) in cases {
            let mut w = writer();
            write(&mut w);
            assert_eq!(w.len(), expected, "{name} wrote the wrong number of bytes");

            // And the size it *declares* must match what it wrote.
            let declared = u32::from_le_bytes(w.as_slice()[4..8].try_into().expect("4 bytes"));
            assert_eq!(
                declared as usize, expected,
                "{name} declared a wrong cmdsize"
            );
        }
    }

    #[test]
    fn variable_length_commands_declare_the_size_they_wrote() {
        let mut w = writer();
        write_load_dylinker(&mut w, DYLD_PATH);
        let declared = u32::from_le_bytes(w.as_slice()[4..8].try_into().expect("4 bytes"));
        assert_eq!(declared as usize, w.len());
        // 12 fixed + 13 chars + NUL = 26, rounded up to the 8-byte alignment
        // a 64-bit image requires → 32. This is the value the real toolchain
        // emits, and a 4-byte rounding to 28 makes `nm` reject the file.
        assert_eq!(w.len(), 32);

        let mut w = writer();
        write_load_dylib(
            &mut w,
            "/usr/lib/libSystem.B.dylib",
            2,
            0x054c_0000,
            0x0001_0000,
        );
        let declared = u32::from_le_bytes(w.as_slice()[4..8].try_into().expect("4 bytes"));
        assert_eq!(declared as usize, w.len());
    }

    /// 64-bit Mach-O requires 8-byte-aligned load commands. `otool -l` walks a
    /// 4-aligned file happily; `nm` rejects it outright with "cmdsize not a
    /// multiple of 8". Checking against only one tool would have missed this.
    #[test]
    fn command_sizes_are_always_eight_byte_aligned() {
        for path in ["/a", "/ab", "/abc", "/abcd", "/abcde", "/abcdef", DYLD_PATH] {
            let mut w = writer();
            write_load_dylinker(&mut w, path);
            assert_eq!(w.len() % 8, 0, "{path} produced an unaligned command");

            let mut w = writer();
            write_load_dylib(&mut w, path, 2, 0, 0);
            assert_eq!(w.len() % 8, 0, "{path} produced an unaligned dylib command");
        }
    }

    #[test]
    fn a_path_carrying_command_is_nul_terminated_within_its_padding() {
        let mut w = writer();
        write_load_dylinker(&mut w, DYLD_PATH);
        let offset = u32::from_le_bytes(w.as_slice()[8..12].try_into().expect("4 bytes")) as usize;
        let bytes = &w.as_slice()[offset..];
        assert_eq!(&bytes[..DYLD_PATH.len()], DYLD_PATH.as_bytes());
        assert_eq!(bytes[DYLD_PATH.len()], 0, "path must be NUL-terminated");
    }

    #[test]
    fn version_packing_matches_the_documented_form() {
        assert_eq!(pack_version((11, 0, 0)), 0x000B_0000);
        assert_eq!(pack_version((26, 5, 0)), 0x001A_0500);
        assert_eq!(pack_version((1, 2, 3)), 0x0001_0203);
    }

    #[test]
    fn build_version_carries_the_platform_and_versions() {
        let mut w = writer();
        write_build_version(&mut w, PLATFORM_MACOS, (11, 0, 0), (26, 5, 0));
        let words: Vec<u32> = w
            .as_slice()
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
            .collect();

        assert_eq!(words[0], LC_BUILD_VERSION);
        assert_eq!(words[2], PLATFORM_MACOS);
        assert_eq!(words[3], pack_version((11, 0, 0)), "minos");
        assert_eq!(words[4], pack_version((26, 5, 0)), "sdk");
        assert_eq!(words[5], 1, "one tool entry");
    }

    /// `LC_MAIN` carries a file offset, not an address — the distinction that
    /// removes the need for a startup object.
    #[test]
    fn main_carries_a_file_offset() {
        let mut w = writer();
        write_main(&mut w, 2272, 0);
        let entry = u64::from_le_bytes(w.as_slice()[8..16].try_into().expect("8 bytes"));
        assert_eq!(entry, 2272);
    }

    fn text_input(size: u64) -> InputPlacement {
        InputPlacement {
            object: ObjectId(0),
            section: SectionId(0),
            segment: "__TEXT".into(),
            name: "__text".into(),
            kind: SectionKind::Code,
            size,
            alignment: 4,
        }
    }

    #[test]
    fn a_segment_command_declares_the_size_of_itself_and_its_sections() {
        let layout = compute_layout(&[text_input(100)], 0x1000);
        let segment = layout.segment("__TEXT").expect("present");

        let mut w = writer();
        write_segment(&mut w, segment, &layout.sections);

        let declared = u32::from_le_bytes(w.as_slice()[4..8].try_into().expect("4 bytes"));
        assert_eq!(declared as usize, w.len());
        assert_eq!(w.len(), sizes::SEGMENT_64 + sizes::SECTION_64);
    }

    #[test]
    fn a_segment_with_no_sections_is_just_the_command() {
        let layout = compute_layout(&[text_input(100)], 0x1000);
        let pagezero = layout.segment("__PAGEZERO").expect("present");

        let mut w = writer();
        write_segment(&mut w, pagezero, &layout.sections);
        assert_eq!(w.len(), sizes::SEGMENT_64);
    }

    #[test]
    fn segment_fields_round_trip_from_the_layout() {
        let layout = compute_layout(&[text_input(100)], 0x1000);
        let segment = layout.segment("__TEXT").expect("present");

        let mut w = writer();
        write_segment(&mut w, segment, &layout.sections);
        let bytes = w.as_slice();

        assert_eq!(&bytes[8..14], b"__TEXT");
        assert_eq!(
            u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes")),
            segment.vm_address
        );
        assert_eq!(
            u32::from_le_bytes(bytes[56..60].try_into().expect("4 bytes")),
            Protection::READ_EXECUTE.0
        );
    }

    #[test]
    fn code_sections_are_marked_as_instructions() {
        let layout = compute_layout(&[text_input(100)], 0x1000);
        let section = layout.find("__TEXT", "__text").expect("present");
        let flags = section_flags(section);
        assert!(flags & S_ATTR_PURE_INSTRUCTIONS != 0);
    }

    #[test]
    fn zero_filled_sections_are_marked_zerofill() {
        let inputs = vec![
            text_input(100),
            InputPlacement {
                object: ObjectId(0),
                section: SectionId(1),
                segment: "__DATA".into(),
                name: "__bss".into(),
                kind: SectionKind::Bss,
                size: 4096,
                alignment: 8,
            },
        ];
        let layout = compute_layout(&inputs, 0x1000);
        let bss = layout.find("__DATA", "__bss").expect("present");
        assert_eq!(section_flags(bss) & 0xFF, S_ZEROFILL);
    }

    /// The reservation layout needs must match what the commands actually
    /// occupy; underestimating would let section content overwrite them.
    #[test]
    fn the_predicted_command_size_covers_what_is_emitted() {
        let inputs = vec![
            text_input(100),
            InputPlacement {
                object: ObjectId(0),
                section: SectionId(1),
                segment: "__DATA".into(),
                name: "__data".into(),
                kind: SectionKind::Data,
                size: 50,
                alignment: 8,
            },
        ];
        let layout = compute_layout(&inputs, 0x1000);
        let dylib = "/usr/lib/libSystem.B.dylib";
        let predicted = command_size_for(&layout, command_size_with_path(24, dylib));

        // Emit everything the prediction accounts for and compare.
        let mut w = writer();
        for segment in &layout.segments {
            write_segment(&mut w, segment, &layout.sections);
        }
        write_dyld_info(&mut w, &LinkEditLayout::default());
        write_symtab(&mut w, &LinkEditLayout::default());
        write_dysymtab(&mut w, &SymbolGroups::default());
        write_load_dylinker(&mut w, DYLD_PATH);
        write_uuid(&mut w, [0; 16]);
        write_build_version(&mut w, PLATFORM_MACOS, (11, 0, 0), (26, 5, 0));
        write_source_version(&mut w, 0);
        write_main(&mut w, 0x1000, 0);
        write_load_dylib(&mut w, dylib, 2, 0, 0);
        write_linkedit_data(&mut w, LC_FUNCTION_STARTS, 0, 0);
        write_linkedit_data(&mut w, LC_DATA_IN_CODE, 0, 0);
        write_linkedit_data(&mut w, LC_CODE_SIGNATURE, 0, 0);

        assert_eq!(
            predicted,
            w.len(),
            "predicted command size does not match what was emitted"
        );
    }
}
