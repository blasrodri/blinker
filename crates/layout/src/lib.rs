//! Assigning output addresses: which input sections go where, at what address.
//!
//! # Grounded in a real binary
//!
//! Every constant here was read off an actual Rust executable rather than a
//! specification, because the specification allows far more than the toolchain
//! actually does:
//!
//! ```text
//! __PAGEZERO    vmaddr 0x0            vmsize 0x100000000  no file bytes
//! __TEXT        vmaddr 0x100000000    fileoff 0           header lives inside
//! __DATA_CONST  vmaddr 0x100044000    fileoff 278528
//! __DATA        vmaddr 0x100048000    fileoff 294912
//! __LINKEDIT    vmaddr 0x10004c000    fileoff 311296
//! ```
//!
//! Three facts from that, each of which would be easy to get wrong:
//!
//! - **Pages are 16 KB on arm64**, not the 4 KB that x86 habits suggest. Every
//!   segment boundary is `0x4000`-aligned.
//! - **`__TEXT` starts at file offset 0** and contains the Mach-O header and
//!   load commands. Layout must reserve room for them before placing any
//!   content, and their size is not known until the load commands are built —
//!   so the reservation is an input to layout, not an output.
//! - **Debug information is not copied into the executable.** The real binary
//!   has no `__DWARF` segment. `ld64` leaves DWARF in the `.o` files and emits
//!   `N_OSO` debug-map stabs pointing back at them (10 in the sample), which is
//!   how LLDB finds source. Dropping debug sections here is therefore correct
//!   rather than a shortcut — but the stabs are then required for debugging to
//!   work at all, which is M3's problem.

use std::collections::BTreeMap;

use blinker_macho::{ObjectId, SectionId, SectionKind};

mod placement;
pub use placement::{OutputRegionId, RegionPlacement};

/// arm64 macOS page size. Segments begin on a multiple of this.
pub const PAGE_SIZE: u64 = 0x4000;

/// Size of the unmapped `__PAGEZERO` guard segment: the whole low 4 GiB, so
/// that dereferencing a truncated-to-32-bit pointer faults.
pub const PAGEZERO_SIZE: u64 = 0x1_0000_0000;

/// Where the image proper begins, immediately above `__PAGEZERO`.
pub const TEXT_BASE: u64 = PAGEZERO_SIZE;

/// Round `value` up to a multiple of `alignment`.
///
/// `alignment` is assumed to be a power of two, which every Mach-O alignment
/// is. Returns `value` unchanged when alignment is 0 or 1.
pub fn align_up(value: u64, alignment: u64) -> u64 {
    if alignment <= 1 {
        return value;
    }
    let mask = alignment - 1;
    (value + mask) & !mask
}

/// One input section's contribution to an output section.
///
/// This is the mapping relocation needs: given an object and one of its
/// sections, where did its bytes end up?
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Contribution {
    pub object: ObjectId,
    pub section: SectionId,
    /// Offset of this contribution within the output section.
    pub offset: u64,
    pub size: u64,
}

/// A section in the output image.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutputSection {
    pub segment: String,
    pub name: String,
    pub kind: SectionKind,
    pub vm_address: u64,
    /// `None` for zero-filled sections, which occupy address space but no file.
    pub file_offset: Option<u64>,
    pub size: u64,
    pub alignment: u64,
    /// Input sections merged into this one, in placement order.
    pub contributions: Vec<Contribution>,
}

impl OutputSection {
    pub fn qualified_name(&self) -> String {
        format!("{},{}", self.segment, self.name)
    }

    /// Where a given input section's bytes ended up, as a virtual address.
    pub fn address_of(&self, object: ObjectId, section: SectionId) -> Option<u64> {
        self.contributions
            .iter()
            .find(|c| c.object == object && c.section == section)
            .map(|c| self.vm_address + c.offset)
    }

    pub fn is_zero_filled(&self) -> bool {
        self.file_offset.is_none()
    }
}

/// A segment in the output image.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutputSegment {
    pub name: String,
    pub vm_address: u64,
    pub vm_size: u64,
    pub file_offset: u64,
    pub file_size: u64,
    /// Indices into [`Layout::sections`].
    pub sections: Vec<usize>,
    pub max_protection: Protection,
    pub init_protection: Protection,
}

/// Memory protection flags, matching Mach-O's `vm_prot_t` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Protection(pub u32);

impl Protection {
    pub const NONE: Protection = Protection(0);
    pub const READ: Protection = Protection(1);
    pub const WRITE: Protection = Protection(2);
    pub const EXECUTE: Protection = Protection(4);

    pub const READ_EXECUTE: Protection = Protection(1 | 4);
    pub const READ_WRITE: Protection = Protection(1 | 2);
}

/// The complete output layout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Layout {
    pub segments: Vec<OutputSegment>,
    pub sections: Vec<OutputSection>,
    /// Total size of the image on disk, before `__LINKEDIT` content is known.
    pub file_size: u64,
    /// Bytes reserved at the start of `__TEXT` for the header and load
    /// commands.
    pub header_reservation: u64,
}

impl Layout {
    pub fn section(&self, index: usize) -> Option<&OutputSection> {
        self.sections.get(index)
    }

    /// Find an output section by its qualified name.
    pub fn find(&self, segment: &str, name: &str) -> Option<&OutputSection> {
        self.sections
            .iter()
            .find(|s| s.segment == segment && s.name == name)
    }

    pub fn segment(&self, name: &str) -> Option<&OutputSegment> {
        self.segments.iter().find(|s| s.name == name)
    }

    /// Virtual address of an input section's bytes in the output.
    ///
    /// The core query for relocation: it turns an (object, section) pair into
    /// the address that section's bytes actually live at.
    pub fn address_of(&self, object: ObjectId, section: SectionId) -> Option<u64> {
        self.sections
            .iter()
            .find_map(|s| s.address_of(object, section))
    }

    /// File offset of an input section's bytes, if it occupies any.
    pub fn file_offset_of(&self, object: ObjectId, section: SectionId) -> Option<u64> {
        self.sections.iter().find_map(|s| {
            let file_offset = s.file_offset?;
            let contribution = s
                .contributions
                .iter()
                .find(|c| c.object == object && c.section == section)?;
            Some(file_offset + contribution.offset)
        })
    }

    /// Placements for every output section, for M6's stable-layout reuse.
    pub fn placements(&self) -> Vec<RegionPlacement> {
        self.sections
            .iter()
            .enumerate()
            .map(|(index, section)| RegionPlacement {
                region: OutputRegionId(index as u32),
                file_offset: section.file_offset.unwrap_or(0),
                virtual_address: section.vm_address,
                used_size: section.size,
                // Cold layout reserves no slack; M6 introduces it.
                reserved_size: section.size,
                alignment: section.alignment,
            })
            .collect()
    }
}

/// An input section awaiting placement.
#[derive(Debug, Clone)]
pub struct InputPlacement {
    pub object: ObjectId,
    pub section: SectionId,
    pub segment: String,
    pub name: String,
    pub kind: SectionKind,
    pub size: u64,
    pub alignment: u64,
}

/// Canonical segment order in the output image.
///
/// Taken from a real binary. `__PAGEZERO` must come first and `__LINKEDIT`
/// last; the middle order groups by protection so the kernel can map
/// contiguous ranges with one set of permissions.
const SEGMENT_ORDER: &[&str] = &[
    "__PAGEZERO",
    "__TEXT",
    "__DATA_CONST",
    "__DATA",
    "__LINKEDIT",
];

/// Canonical section order within each segment, again from a real binary.
///
/// Sections not listed sort after those listed, alphabetically, so the layout
/// is deterministic even for inputs we have not seen.
const SECTION_ORDER: &[(&str, &[&str])] = &[
    (
        "__TEXT",
        &[
            "__text",
            "__stubs",
            "__stub_helper",
            "__const",
            "__gcc_except_tab",
            "__cstring",
            "__unwind_info",
            "__eh_frame",
        ],
    ),
    ("__DATA_CONST", &["__const", "__got"]),
    (
        "__DATA",
        &[
            "__data",
            // The thread-local sections must end up **adjacent**, in this
            // order. dyld treats `__thread_data` followed by `__thread_bss` as
            // one block that it copies per thread, and a descriptor's offset
            // is relative to the start of that block. Leaving `__thread_bss`
            // out of this table sorted it to the end of the segment, behind
            // `__bss`, `__common` and everything unrecognised — so every
            // offset into the block was wrong by however much sat between
            // them, and panicking (which touches a thread-local panic count)
            // died on a wild address.
            "__thread_ptrs",
            "__thread_vars",
            "__thread_data",
            // Zero-filled sections carry no file bytes, so they come last;
            // `__thread_bss` is first among them to stay next to
            // `__thread_data`.
            "__thread_bss",
            "__common",
            "__bss",
        ],
    ),
];

/// Which output segment an input section belongs in.
///
/// Mostly identity, with the remappings the toolchain performs. Returning
/// `None` means the section is deliberately not carried into the output.
pub fn output_segment_for(segment: &str, name: &str, kind: SectionKind) -> Option<&'static str> {
    // Bitcode and the recorded command line are inputs to further tooling, not
    // parts of the image. ld64 drops them; carrying them through put
    // `__bitcode` and `__cmdline` in the middle of `__DATA`, between sections
    // that have to be adjacent.
    if segment == "__LLVM" {
        return None;
    }

    match kind {
        // Debug data stays in the object files; the executable references it
        // through N_OSO debug-map stabs instead. Verified: a real Rust binary
        // has no __DWARF segment.
        SectionKind::Debug => None,
        SectionKind::Code => Some("__TEXT"),
        SectionKind::Bss | SectionKind::ThreadLocal => Some("__DATA"),
        SectionKind::Unwind => Some("__TEXT"),
        SectionKind::ReadOnlyData => match segment {
            // `__DATA,__const` is relocated read-only data: writable while dyld
            // binds it, read-only afterwards. That is what __DATA_CONST is for.
            "__DATA" | "__DATA_CONST" => Some("__DATA_CONST"),
            _ => Some("__TEXT"),
        },
        SectionKind::Data => match name {
            // Both hold pointers dyld writes once and never again, which is
            // exactly what __DATA_CONST is for. Leaving `__got` in `__DATA`
            // also sorted it behind the zero-filled sections, putting a
            // section with file content after ones that have none.
            "__const" | "__got" => Some("__DATA_CONST"),
            _ => Some("__DATA"),
        },
        SectionKind::Other => Some("__DATA"),
    }
}

/// Sort key for a segment: its index in the canonical order.
fn segment_rank(name: &str) -> usize {
    SEGMENT_ORDER
        .iter()
        .position(|s| *s == name)
        .unwrap_or(SEGMENT_ORDER.len())
}

/// Sort key for a section within its segment.
fn section_rank(segment: &str, name: &str) -> usize {
    SECTION_ORDER
        .iter()
        .find(|(s, _)| *s == segment)
        .and_then(|(_, names)| names.iter().position(|n| *n == name))
        .unwrap_or(usize::MAX)
}

/// Protection flags for a segment.
fn protection_for(segment: &str) -> (Protection, Protection) {
    match segment {
        "__PAGEZERO" => (Protection::NONE, Protection::NONE),
        "__TEXT" => (Protection::READ_EXECUTE, Protection::READ_EXECUTE),
        // __DATA_CONST is writable at load time so dyld can bind it, then
        // marked read-only. Both slots say read-write; the const-ness comes
        // from the SG_READ_ONLY segment flag, not from these.
        "__DATA_CONST" | "__DATA" => (Protection::READ_WRITE, Protection::READ_WRITE),
        "__LINKEDIT" => (Protection::READ, Protection::READ),
        _ => (Protection::READ, Protection::READ),
    }
}

/// Compute a cold layout for a set of input sections.
///
/// `header_reservation` is how many bytes to leave at the start of `__TEXT` for
/// the Mach-O header and load commands. It is a parameter rather than something
/// computed here because the load commands cannot be sized until the segments
/// are known — the caller resolves that circularity by laying out once to
/// discover the shape, sizing the commands, then laying out again.
pub fn compute_layout(inputs: &[InputPlacement], header_reservation: u64) -> Layout {
    // Group inputs by output section, preserving input order within each group
    // so the layout is deterministic and reproducible.
    let mut groups: BTreeMap<(usize, usize, String, String), Vec<&InputPlacement>> =
        BTreeMap::new();

    for input in inputs {
        let Some(segment) = output_segment_for(&input.segment, &input.name, input.kind) else {
            continue;
        };
        let name = output_section_name(&input.name, input.kind);
        let key = (
            segment_rank(segment),
            section_rank(segment, &name),
            segment.to_string(),
            name,
        );
        groups.entry(key).or_default().push(input);
    }

    let mut sections: Vec<OutputSection> = Vec::new();
    let mut segment_members: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for ((_, _, segment, name), members) in groups {
        let kind = members[0].kind;
        let zero_filled = kind == SectionKind::Bss;

        let mut offset = 0u64;
        let mut alignment = 1u64;
        let mut contributions = Vec::with_capacity(members.len());

        for member in members {
            let member_alignment = member.alignment.max(1);
            alignment = alignment.max(member_alignment);
            offset = align_up(offset, member_alignment);
            contributions.push(Contribution {
                object: member.object,
                section: member.section,
                offset,
                size: member.size,
            });
            offset += member.size;
        }

        segment_members
            .entry(segment.clone())
            .or_default()
            .push(sections.len());
        sections.push(OutputSection {
            segment,
            name,
            kind,
            // Addresses are assigned in the second pass, once section sizes
            // are known.
            vm_address: 0,
            file_offset: (!zero_filled).then_some(0),
            size: offset,
            alignment,
            contributions,
        });
    }

    let segments = assign_addresses(&mut sections, &segment_members, header_reservation);
    let file_size = segments
        .iter()
        .map(|s| s.file_offset + s.file_size)
        .max()
        .unwrap_or(0);

    Layout {
        segments,
        sections,
        file_size,
        header_reservation,
    }
}

/// Output section name for an input section.
fn output_section_name(name: &str, kind: SectionKind) -> String {
    match kind {
        // Thread-local sections keep their distinct names: dyld needs to tell
        // the descriptor section from the data it describes.
        SectionKind::ThreadLocal => name.to_string(),
        _ => name.to_string(),
    }
}

/// Second pass: assign virtual addresses and file offsets.
fn assign_addresses(
    sections: &mut [OutputSection],
    segment_members: &BTreeMap<String, Vec<usize>>,
    header_reservation: u64,
) -> Vec<OutputSegment> {
    let mut segments = Vec::new();

    // __PAGEZERO occupies address space but nothing else: no sections, no file
    // bytes. It exists so that a null or truncated pointer dereference faults.
    segments.push(OutputSegment {
        name: "__PAGEZERO".to_string(),
        vm_address: 0,
        vm_size: PAGEZERO_SIZE,
        file_offset: 0,
        file_size: 0,
        sections: Vec::new(),
        max_protection: Protection::NONE,
        init_protection: Protection::NONE,
    });

    let mut names: Vec<&String> = segment_members.keys().collect();
    names.sort_by_key(|n| segment_rank(n));

    let mut vm_address = TEXT_BASE;
    let mut file_offset = 0u64;

    for name in names {
        let members = &segment_members[name];
        let is_text = name == "__TEXT";

        let segment_vm_start = vm_address;
        let segment_file_start = file_offset;

        // __TEXT begins at file offset 0 and carries the Mach-O header and
        // load commands ahead of any section content.
        let mut cursor = if is_text { header_reservation } else { 0 };

        for &index in members {
            let section = &mut sections[index];
            cursor = align_up(cursor, section.alignment.max(1));
            section.vm_address = segment_vm_start + cursor;
            section.file_offset = if section.is_zero_filled() {
                None
            } else {
                Some(segment_file_start + cursor)
            };
            cursor += section.size;
        }

        let content_size = cursor;
        // Zero-filled sections extend the segment's virtual size but not its
        // file size — the distinction that makes __bss cost nothing on disk.
        let file_size = members
            .iter()
            .filter(|&&i| !sections[i].is_zero_filled())
            .map(|&i| {
                let s = &sections[i];
                s.file_offset.unwrap_or(0) - segment_file_start + s.size
            })
            .max()
            .unwrap_or(if is_text { header_reservation } else { 0 });

        let (max_protection, init_protection) = protection_for(name);
        segments.push(OutputSegment {
            name: name.clone(),
            vm_address: segment_vm_start,
            vm_size: align_up(content_size, PAGE_SIZE),
            file_offset: segment_file_start,
            file_size,
            sections: members.clone(),
            max_protection,
            init_protection,
        });

        vm_address = align_up(segment_vm_start + content_size, PAGE_SIZE);
        file_offset = align_up(segment_file_start + file_size, PAGE_SIZE);
    }

    // __LINKEDIT holds the symbol table, dyld metadata, and the code
    // signature. Its content is produced after layout, so it is reserved here
    // with zero size and filled in by the writer.
    segments.push(OutputSegment {
        name: "__LINKEDIT".to_string(),
        vm_address,
        vm_size: 0,
        file_offset,
        file_size: 0,
        sections: Vec::new(),
        max_protection: Protection::READ,
        init_protection: Protection::READ,
    });

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        object: u32,
        section: u32,
        segment: &str,
        name: &str,
        kind: SectionKind,
        size: u64,
        alignment: u64,
    ) -> InputPlacement {
        InputPlacement {
            object: ObjectId(object),
            section: SectionId(section),
            segment: segment.to_string(),
            name: name.to_string(),
            kind,
            size,
            alignment,
        }
    }

    fn text(object: u32, size: u64) -> InputPlacement {
        input(object, 0, "__TEXT", "__text", SectionKind::Code, size, 4)
    }

    #[test]
    fn aligns_up_to_powers_of_two() {
        assert_eq!(align_up(0, 16), 0);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(16, 16), 16);
        assert_eq!(align_up(17, 16), 32);
        assert_eq!(align_up(0x3fff, PAGE_SIZE), PAGE_SIZE);
    }

    #[test]
    fn alignment_of_zero_or_one_is_a_no_op() {
        assert_eq!(align_up(7, 0), 7);
        assert_eq!(align_up(7, 1), 7);
    }

    #[test]
    fn pagezero_guards_the_low_four_gigabytes() {
        // Its whole purpose: make a truncated-to-32-bit pointer fault.
        let layout = compute_layout(&[text(0, 100)], 0x1000);
        let pagezero = layout.segment("__PAGEZERO").expect("present");
        assert_eq!(pagezero.vm_address, 0);
        assert_eq!(pagezero.vm_size, PAGEZERO_SIZE);
        assert_eq!(pagezero.file_size, 0, "__PAGEZERO occupies no file bytes");
    }

    #[test]
    fn text_starts_above_pagezero_at_file_offset_zero() {
        let layout = compute_layout(&[text(0, 100)], 0x1000);
        let text_segment = layout.segment("__TEXT").expect("present");
        assert_eq!(text_segment.vm_address, TEXT_BASE);
        assert_eq!(
            text_segment.file_offset, 0,
            "the Mach-O header lives at the start of __TEXT"
        );
    }

    #[test]
    fn the_header_reservation_pushes_content_down() {
        // Load commands occupy the front of __TEXT; no section may overlap them.
        let reservation = 0x1000;
        let layout = compute_layout(&[text(0, 100)], reservation);
        let section = layout.find("__TEXT", "__text").expect("present");
        assert!(
            section.vm_address >= TEXT_BASE + reservation,
            "section content overlaps the header reservation"
        );
        assert!(section.file_offset.expect("has bytes") >= reservation);
    }

    #[test]
    fn segments_are_emitted_in_canonical_order() {
        let inputs = vec![
            input(0, 0, "__DATA", "__data", SectionKind::Data, 10, 8),
            text(0, 100),
            input(0, 2, "__DATA", "__const", SectionKind::ReadOnlyData, 20, 8),
        ];
        let layout = compute_layout(&inputs, 0x1000);
        let order: Vec<&str> = layout.segments.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "__PAGEZERO",
                "__TEXT",
                "__DATA_CONST",
                "__DATA",
                "__LINKEDIT"
            ]
        );
    }

    #[test]
    fn segments_are_page_aligned_and_do_not_overlap() {
        let inputs = vec![
            text(0, 5000),
            input(0, 1, "__DATA", "__data", SectionKind::Data, 3000, 8),
        ];
        let layout = compute_layout(&inputs, 0x1000);

        // Skip __PAGEZERO, whose 4 GiB size is its own rule.
        let mapped: Vec<&OutputSegment> = layout
            .segments
            .iter()
            .filter(|s| s.name != "__PAGEZERO")
            .collect();

        for segment in &mapped {
            assert_eq!(
                segment.vm_address % PAGE_SIZE,
                0,
                "{} is not page-aligned",
                segment.name
            );
        }
        for pair in mapped.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                a.vm_address + a.vm_size <= b.vm_address,
                "{} overlaps {}",
                a.name,
                b.name
            );
        }
    }

    #[test]
    fn contributions_are_placed_in_order_and_respect_alignment() {
        let inputs = vec![
            input(0, 0, "__TEXT", "__text", SectionKind::Code, 10, 4),
            input(1, 0, "__TEXT", "__text", SectionKind::Code, 10, 16),
        ];
        let layout = compute_layout(&inputs, 0x1000);
        let section = layout.find("__TEXT", "__text").expect("present");

        assert_eq!(section.contributions[0].offset, 0);
        // The second needs 16-byte alignment, so it cannot start at 10.
        assert_eq!(section.contributions[1].offset, 16);
        // The section takes the strictest alignment its members require.
        assert_eq!(section.alignment, 16);
        assert_eq!(section.size, 26);
    }

    #[test]
    fn an_input_sections_address_can_be_looked_up() {
        // The core query relocation depends on.
        let inputs = vec![text(0, 10), text(1, 10)];
        let layout = compute_layout(&inputs, 0x1000);

        let first = layout
            .address_of(ObjectId(0), SectionId(0))
            .expect("placed");
        let second = layout
            .address_of(ObjectId(1), SectionId(0))
            .expect("placed");
        assert!(
            second > first,
            "second contribution should follow the first"
        );
        assert_eq!(
            second - first,
            12,
            "10 bytes rounded up to 4-byte alignment"
        );
    }

    #[test]
    fn an_unplaced_section_has_no_address() {
        let layout = compute_layout(&[text(0, 10)], 0x1000);
        assert_eq!(layout.address_of(ObjectId(99), SectionId(0)), None);
    }

    /// Verified against a real binary: the executable carries no `__DWARF`
    /// segment. Debug info stays in the objects, referenced by debug-map stabs.
    #[test]
    fn debug_sections_are_not_carried_into_the_output() {
        let inputs = vec![
            text(0, 100),
            input(0, 1, "__DWARF", "__debug_info", SectionKind::Debug, 5000, 1),
            input(0, 2, "__DWARF", "__debug_str", SectionKind::Debug, 9000, 1),
        ];
        let layout = compute_layout(&inputs, 0x1000);

        assert!(layout.segments.iter().all(|s| s.name != "__DWARF"));
        assert!(layout.sections.iter().all(|s| s.kind != SectionKind::Debug));
        assert_eq!(layout.address_of(ObjectId(0), SectionId(1)), None);
    }

    #[test]
    fn zero_filled_sections_occupy_address_space_but_no_file_bytes() {
        let inputs = vec![
            input(0, 0, "__DATA", "__data", SectionKind::Data, 100, 8),
            input(0, 1, "__DATA", "__bss", SectionKind::Bss, 100_000, 8),
        ];
        let layout = compute_layout(&inputs, 0x1000);

        let bss = layout.find("__DATA", "__bss").expect("present");
        assert!(bss.is_zero_filled());
        assert_eq!(bss.file_offset, None);
        assert_eq!(bss.size, 100_000);

        // The 100 KB of bss must not inflate the file.
        let data = layout.segment("__DATA").expect("present");
        assert!(
            data.file_size < 1000,
            "zero-filled content leaked into the file: {}",
            data.file_size
        );
    }

    #[test]
    fn relocated_read_only_data_goes_to_data_const() {
        // `__DATA,__const` is writable while dyld binds it, read-only after —
        // which is exactly what __DATA_CONST exists for.
        let inputs = vec![
            text(0, 10),
            input(0, 1, "__DATA", "__const", SectionKind::ReadOnlyData, 50, 8),
        ];
        let layout = compute_layout(&inputs, 0x1000);
        assert!(layout.find("__DATA_CONST", "__const").is_some());
        assert!(layout.find("__DATA", "__const").is_none());
    }

    #[test]
    fn thread_local_sections_keep_their_distinct_names() {
        // dyld must tell the descriptor section from the data it describes.
        let inputs = vec![
            input(
                0,
                0,
                "__DATA",
                "__thread_vars",
                SectionKind::ThreadLocal,
                24,
                8,
            ),
            input(
                0,
                1,
                "__DATA",
                "__thread_data",
                SectionKind::ThreadLocal,
                16,
                8,
            ),
        ];
        let layout = compute_layout(&inputs, 0x1000);
        assert!(layout.find("__DATA", "__thread_vars").is_some());
        assert!(layout.find("__DATA", "__thread_data").is_some());
    }

    #[test]
    fn segment_protections_match_their_contents() {
        let inputs = vec![
            text(0, 10),
            input(0, 1, "__DATA", "__data", SectionKind::Data, 10, 8),
        ];
        let layout = compute_layout(&inputs, 0x1000);

        assert_eq!(
            layout.segment("__TEXT").expect("present").init_protection,
            Protection::READ_EXECUTE
        );
        assert_eq!(
            layout.segment("__DATA").expect("present").init_protection,
            Protection::READ_WRITE
        );
        assert_eq!(
            layout
                .segment("__PAGEZERO")
                .expect("present")
                .init_protection,
            Protection::NONE
        );
    }

    #[test]
    fn sections_are_ordered_canonically_within_a_segment() {
        // Order comes from a real binary; deterministic output depends on it.
        let inputs = vec![
            input(
                0,
                0,
                "__TEXT",
                "__cstring",
                SectionKind::ReadOnlyData,
                10,
                1,
            ),
            input(0, 1, "__TEXT", "__text", SectionKind::Code, 10, 4),
            input(0, 2, "__TEXT", "__const", SectionKind::ReadOnlyData, 10, 8),
        ];
        let layout = compute_layout(&inputs, 0x1000);

        let names: Vec<&str> = layout
            .sections
            .iter()
            .filter(|s| s.segment == "__TEXT")
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["__text", "__const", "__cstring"]);
    }

    #[test]
    fn layout_is_deterministic_for_the_same_inputs() {
        // Reproducibility is a spec requirement; a BTreeMap iteration order
        // change or an unstable sort would break it silently.
        let inputs = vec![
            text(0, 100),
            input(1, 1, "__DATA", "__data", SectionKind::Data, 50, 8),
            input(
                2,
                2,
                "__TEXT",
                "__cstring",
                SectionKind::ReadOnlyData,
                25,
                1,
            ),
        ];
        assert_eq!(
            compute_layout(&inputs, 0x1000),
            compute_layout(&inputs, 0x1000)
        );
    }

    #[test]
    fn an_empty_link_still_produces_a_valid_skeleton() {
        let layout = compute_layout(&[], 0x1000);
        assert!(layout.segment("__PAGEZERO").is_some());
        assert!(layout.segment("__LINKEDIT").is_some());
        assert!(layout.sections.is_empty());
    }

    #[test]
    fn placements_are_produced_for_every_section() {
        let inputs = vec![
            text(0, 100),
            input(0, 1, "__DATA", "__data", SectionKind::Data, 50, 8),
        ];
        let layout = compute_layout(&inputs, 0x1000);
        let placements = layout.placements();
        assert_eq!(placements.len(), layout.sections.len());
        for (placement, section) in placements.iter().zip(&layout.sections) {
            assert_eq!(placement.virtual_address, section.vm_address);
            assert_eq!(placement.used_size, section.size);
        }
    }

    #[test]
    fn layout_round_trips_through_serde() {
        let layout = compute_layout(&[text(0, 100)], 0x1000);
        let json = serde_json::to_string(&layout).expect("serializes");
        let back: Layout = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(layout, back);
    }
}
