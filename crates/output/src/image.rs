//! Assembling a complete Mach-O image.
//!
//! # The circularity, and how it is broken
//!
//! Layout needs to know how many bytes to reserve at the front of `__TEXT` for
//! the header and load commands. But the load commands describe the segments,
//! so their size is not known until layout has run. Each needs the other.
//!
//! [`commands::command_size_for`] breaks it by computing the command size from
//! the layout's *shape* — how many segments, how many sections, how many
//! dylibs — rather than by emitting and measuring. The caller lays out once to
//! discover the shape, sizes the commands, then lays out again with the real
//! reservation. [`ImageBuilder::build`] does both passes internally so callers
//! cannot get the order wrong.
//!
//! # `__LINKEDIT`
//!
//! Everything dyld reads but the CPU never executes lives in one trailing
//! segment: the dyld opcode streams, the symbol table, the string table, and
//! the code signature. Its contents are produced after layout, so the segment
//! is reserved during layout and filled here.

use blinker_layout::{align_up, compute_layout, InputPlacement, Layout, PAGE_SIZE};

use crate::commands::{self, LinkEditLayout};
use crate::dyld_info::{encode_bind, encode_rebase, Bind, Rebase};
use crate::format::*;
use crate::signature::{sign, signature_size, SignatureRequest};
use crate::symtab::{SymbolTable, SymbolTableBuilder};

/// A dynamic library the image loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dylib {
    pub install_name: String,
    pub timestamp: u32,
    pub current_version: u32,
    pub compatibility_version: u32,
}

impl Dylib {
    /// libSystem, which every Rust executable links against.
    pub fn lib_system() -> Self {
        Dylib {
            install_name: "/usr/lib/libSystem.B.dylib".to_string(),
            timestamp: 2,
            current_version: commands::pack_version((1356, 0, 0)),
            compatibility_version: commands::pack_version((1, 0, 0)),
        }
    }
}

/// Bytes for one output section, keyed by its index in the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionContent {
    pub section: usize,
    pub bytes: Vec<u8>,
}

/// Everything needed to produce an image.
#[derive(Debug)]
pub struct ImageBuilder {
    inputs: Vec<InputPlacement>,
    contents: Vec<SectionContent>,
    symbols: SymbolTableBuilder,
    dylibs: Vec<Dylib>,
    rebases: Vec<Rebase>,
    binds: Vec<Bind>,
    entry_offset: u64,
    min_os: (u16, u8, u8),
    sdk: (u16, u8, u8),
    uuid: [u8; 16],
    /// The identifier embedded in the ad-hoc signature.
    ///
    /// `codesign` derives this from the output file's name. blinker does not
    /// know its own output path here, so the caller sets it; the default is
    /// what an unnamed link produces.
    identifier: String,
}

#[derive(Debug)]
pub enum ImageError {
    /// A section's content does not match the size layout assigned it.
    ///
    /// Refused rather than padded or truncated: either would produce a file
    /// whose load commands disagree with its bytes, which loads and then
    /// misbehaves.
    ContentSizeMismatch {
        section: String,
        expected: u64,
        actual: usize,
    },
    /// Content was supplied for a section index that does not exist.
    UnknownSection { section: usize },
    /// The signature would have been written over emitted content.
    ///
    /// Unreachable if `__LINKEDIT` is placed correctly, which is exactly why
    /// it is checked: the failure mode when it is not is a file whose header
    /// has been overwritten, and that is unreadable rather than diagnosable.
    SignatureOverlapsContent {
        signature_start: u64,
        content_end: u64,
    },
    /// The emitted load commands did not fit the reservation layout made.
    ///
    /// Only reachable if the size prediction and the emitter disagree, which
    /// the tests pin — but it must be an error rather than an overwrite.
    CommandsOverflowedReservation { reserved: u64, emitted: usize },
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::ContentSizeMismatch {
                section,
                expected,
                actual,
            } => write!(
                f,
                "section {section} was laid out as {expected} bytes but {actual} were supplied"
            ),
            ImageError::UnknownSection { section } => {
                write!(f, "content supplied for unknown section index {section}")
            }
            ImageError::SignatureOverlapsContent {
                signature_start,
                content_end,
            } => write!(
                f,
                "the signature would start at {signature_start:#x}, inside content ending at {content_end:#x}"
            ),
            ImageError::CommandsOverflowedReservation { reserved, emitted } => write!(
                f,
                "load commands need {emitted} bytes but only {reserved} were reserved"
            ),
        }
    }
}

impl std::error::Error for ImageError {}

/// A finished image, and the layout it was built from.
#[derive(Debug)]
pub struct Image {
    pub bytes: Vec<u8>,
    pub layout: Layout,
    pub symbols: SymbolTable,
}

impl ImageBuilder {
    pub fn new() -> Self {
        ImageBuilder {
            inputs: Vec::new(),
            contents: Vec::new(),
            symbols: SymbolTableBuilder::new(),
            dylibs: Vec::new(),
            rebases: Vec::new(),
            binds: Vec::new(),
            entry_offset: 0,
            min_os: (11, 0, 0),
            sdk: (26, 5, 0),
            // Zero until content hashing is implemented; a real UUID is
            // derived from the image's own bytes, which is a later step.
            uuid: [0; 16],
            identifier: "a.out".to_string(),
        }
    }

    /// Set the identifier embedded in the ad-hoc signature.
    ///
    /// Conventionally the output file's base name, matching `codesign`.
    pub fn identifier(&mut self, identifier: &str) -> &mut Self {
        self.identifier = identifier.to_string();
        self
    }

    pub fn input(&mut self, placement: InputPlacement) -> &mut Self {
        self.inputs.push(placement);
        self
    }

    pub fn content(&mut self, section: usize, bytes: Vec<u8>) -> &mut Self {
        self.contents.push(SectionContent { section, bytes });
        self
    }

    pub fn symbols(&mut self) -> &mut SymbolTableBuilder {
        &mut self.symbols
    }

    pub fn dylib(&mut self, dylib: Dylib) -> &mut Self {
        self.dylibs.push(dylib);
        self
    }

    pub fn rebase(&mut self, rebase: Rebase) -> &mut Self {
        self.rebases.push(rebase);
        self
    }

    pub fn bind(&mut self, bind: Bind) -> &mut Self {
        self.binds.push(bind);
        self
    }

    /// Entry point, as a **file offset** — see `LC_MAIN`.
    pub fn entry_offset(&mut self, offset: u64) -> &mut Self {
        self.entry_offset = offset;
        self
    }

    pub fn versions(&mut self, min_os: (u16, u8, u8), sdk: (u16, u8, u8)) -> &mut Self {
        self.min_os = min_os;
        self.sdk = sdk;
        self
    }

    /// Lay out and emit the image.
    pub fn build(mut self) -> Result<Image, ImageError> {
        // Pass one: discover the shape with a nominal reservation.
        let probe = compute_layout(&self.inputs, PAGE_SIZE);
        let dylib_bytes: usize = self
            .dylibs
            .iter()
            .map(|d| commands::command_size_with_path(24, &d.install_name))
            .sum();
        // Over-reserves by three `linkedit_data_command`s: LC_FUNCTION_STARTS,
        // LC_DATA_IN_CODE and LC_CODE_SIGNATURE are not emitted yet but will
        // be. Reserving space we do not use is harmless; running out is not.
        let command_size = commands::command_size_for(&probe, dylib_bytes);

        // Pass two: lay out for real, with room for the header and commands.
        let reservation = align_up((MachHeader::SIZE + command_size) as u64, 16);
        let layout = compute_layout(&self.inputs, reservation);

        for content in &self.contents {
            let Some(section) = layout.section(content.section) else {
                return Err(ImageError::UnknownSection {
                    section: content.section,
                });
            };
            if section.size != content.bytes.len() as u64 {
                return Err(ImageError::ContentSizeMismatch {
                    section: section.qualified_name(),
                    expected: section.size,
                    actual: content.bytes.len(),
                });
            }
        }

        let symbols = std::mem::take(&mut self.symbols).build();
        let rebase_stream = encode_rebase(&self.rebases);
        let bind_stream = encode_bind(&self.binds);

        // __LINKEDIT sits above every mapped segment; its contents are laid
        // out here because they did not exist during layout.
        //
        // The `max(reservation)` is not belt-and-braces: an image with no
        // sections at all has no mapped segment to sit above, and falling back
        // to zero put `__LINKEDIT` — and therefore the signature — on top of
        // the header and load commands. That produced a file whose `ncmds`
        // read as 3222068986. `__LINKEDIT` must always begin past the commands,
        // whether or not anything else was laid out.
        let link_edit_start = layout
            .segments
            .iter()
            .filter(|s| s.name != "__LINKEDIT" && s.name != "__PAGEZERO")
            .map(|s| s.file_offset + s.file_size)
            .max()
            .unwrap_or(0)
            .max(reservation);
        let link_edit_start = align_up(link_edit_start, PAGE_SIZE);

        let mut link_edit = LinkEditLayout::default();
        let mut cursor = link_edit_start;

        // Each opcode stream must start 8-byte aligned. dyld reads them
        // through pointer-sized loads, and an unaligned start makes it reject
        // the whole stream — `dyld_info` reports "mis-aligned LINKEDIT content
        // 'bind opcodes'" and every bind in it is silently never applied. The
        // symptom is a crash on the first use of anything that needed binding,
        // nowhere near the linker.
        cursor = align_up(cursor, 8);
        link_edit.rebase_offset = cursor as u32;
        link_edit.rebase_size = rebase_stream.len() as u32;
        cursor += rebase_stream.len() as u64;

        cursor = align_up(cursor, 8);
        link_edit.bind_offset = cursor as u32;
        link_edit.bind_size = bind_stream.len() as u32;
        cursor += bind_stream.len() as u64;

        // The symbol table must be 8-byte aligned: each nlist_64 ends with a
        // u64 value field, and an unaligned table is read incorrectly.
        cursor = align_up(cursor, 8);
        link_edit.symbol_offset = cursor as u32;
        link_edit.symbol_count = symbols.entries.len() as u32;
        cursor += symbols.entries_size() as u64;

        link_edit.string_offset = cursor as u32;
        link_edit.string_size = symbols.strings_size() as u32;
        cursor += symbols.strings_size() as u64;

        // The signature goes last, and covers everything before it. Its size
        // has to be known now, before any bytes exist, because it determines
        // where `__LINKEDIT` ends — and that value goes into a load command
        // that the signature itself hashes.
        let signature_start = align_up(cursor, 16);
        let text_segment = layout.segment("__TEXT");
        let request = SignatureRequest {
            identifier: self.identifier.clone(),
            code_limit: signature_start,
            exec_segment_base: text_segment.map(|s| s.file_offset).unwrap_or(0),
            exec_segment_limit: text_segment.map(|s| s.file_size).unwrap_or(0),
        };
        let signature_len = signature_size(&request);
        link_edit.code_signature_offset = signature_start as u32;
        link_edit.code_signature_size = signature_len as u32;

        // No page padding: `__LINKEDIT` ends exactly where the signature does,
        // which is what ld64 emits and what keeps `code_limit` honest.
        let link_edit_end = signature_start + signature_len as u64;

        let mut bytes = self.emit(
            &layout,
            &symbols,
            &link_edit,
            &rebase_stream,
            &bind_stream,
            link_edit_start,
            link_edit_end,
            reservation,
        )?;

        // Sign last. Everything the signature covers is now final, including
        // the LC_CODE_SIGNATURE command that points at where it is about to go.
        // Truncation removes only the space reserved for the signature. If
        // `signature_start` ever landed inside real content this would silently
        // corrupt the image, so it is checked rather than assumed.
        if (signature_start as usize) < link_edit_start as usize {
            return Err(ImageError::SignatureOverlapsContent {
                signature_start,
                content_end: link_edit_start,
            });
        }
        bytes.truncate(signature_start as usize);
        let blob = sign(&bytes, &request);
        debug_assert_eq!(blob.len(), signature_len);
        bytes.extend_from_slice(&blob);

        Ok(Image {
            bytes,
            layout,
            symbols,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        layout: &Layout,
        symbols: &SymbolTable,
        link_edit: &LinkEditLayout,
        rebase_stream: &[u8],
        bind_stream: &[u8],
        link_edit_start: u64,
        link_edit_end: u64,
        reservation: u64,
    ) -> Result<Vec<u8>, ImageError> {
        let mut writer = Writer::with_capacity(link_edit_end as usize);

        // The thread-local flag is a property of what was laid out, so it is
        // decided here rather than baked into the header constant.
        let has_thread_locals = layout
            .sections
            .iter()
            .any(|s| s.kind == blinker_macho::SectionKind::ThreadLocal);
        let header = MachHeader::executable().with_thread_local_variables(has_thread_locals);
        header.write(&mut writer);

        let mut command_count = 0u32;
        let commands_start = writer.len();

        for segment in &layout.segments {
            if segment.name == "__LINKEDIT" {
                // Emitted below with its real, now-known size.
                continue;
            }
            commands::write_segment(&mut writer, segment, &layout.sections);
            command_count += 1;
        }

        // __LINKEDIT, with the size its contents actually came to.
        let mut link_edit_segment = layout
            .segment("__LINKEDIT")
            .expect("layout always reserves __LINKEDIT")
            .clone();
        link_edit_segment.file_offset = link_edit_start;
        link_edit_segment.file_size = link_edit_end - link_edit_start;
        link_edit_segment.vm_size = align_up(link_edit_segment.file_size, PAGE_SIZE);
        commands::write_segment(&mut writer, &link_edit_segment, &layout.sections);
        command_count += 1;

        // Counted per emission rather than in a batch: a hand-maintained
        // total drifts the moment a command is added, and the header would
        // then claim a count the walk disagrees with.
        commands::write_dyld_info(&mut writer, link_edit);
        command_count += 1;
        commands::write_symtab(&mut writer, link_edit);
        command_count += 1;
        commands::write_dysymtab(&mut writer, &symbols.groups);
        command_count += 1;
        commands::write_load_dylinker(&mut writer, DYLD_PATH);
        command_count += 1;
        commands::write_uuid(&mut writer, self.uuid);
        command_count += 1;
        commands::write_build_version(&mut writer, PLATFORM_MACOS, self.min_os, self.sdk);
        command_count += 1;
        commands::write_source_version(&mut writer, 0);
        command_count += 1;
        commands::write_main(&mut writer, self.entry_offset, 0);
        command_count += 1;

        for dylib in &self.dylibs {
            commands::write_load_dylib(
                &mut writer,
                &dylib.install_name,
                dylib.timestamp,
                dylib.current_version,
                dylib.compatibility_version,
            );
            command_count += 1;
        }

        commands::write_code_signature(&mut writer, link_edit);
        command_count += 1;

        let command_size = writer.len() - commands_start;

        if writer.len() as u64 > reservation {
            return Err(ImageError::CommandsOverflowedReservation {
                reserved: reservation,
                emitted: writer.len(),
            });
        }

        // Now that the count and size are known, patch them into the header.
        writer.patch_u32(MachHeader::COMMAND_COUNT_OFFSET, command_count);
        writer.patch_u32(MachHeader::COMMAND_SIZE_OFFSET, command_size as u32);

        // Section content, each at the file offset layout assigned it.
        for content in &self.contents {
            let section = layout
                .section(content.section)
                .expect("validated during build");
            let Some(offset) = section.file_offset else {
                // Zero-filled: occupies address space but no file bytes.
                continue;
            };
            if !writer.pad_to(offset as usize) {
                return Err(ImageError::ContentSizeMismatch {
                    section: section.qualified_name(),
                    expected: offset,
                    actual: writer.len(),
                });
            }
            writer.bytes(&content.bytes);
        }

        // __LINKEDIT content, in the order the offsets were assigned.
        writer.pad_to(link_edit.rebase_offset as usize);
        writer.bytes(rebase_stream);
        // Padded to the offset the layout assigned rather than written back to
        // back: the two streams are independently aligned, so appending the
        // bind stream directly would put it wherever the rebase stream
        // happened to end.
        writer.pad_to(link_edit.bind_offset as usize);
        writer.bytes(bind_stream);
        writer.pad_to(link_edit.symbol_offset as usize);
        symbols.write_entries(&mut writer);
        writer.bytes(&symbols.strings);
        writer.pad_to(link_edit_end as usize);

        Ok(writer.finish())
    }
}

impl Default for ImageBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symtab::OutputSymbol;
    use blinker_macho::{ObjectId, SectionId, SectionKind};

    fn code(size: u64) -> InputPlacement {
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

    /// A minimal but complete image: some code, one symbol, one dylib.
    fn minimal() -> Image {
        let mut builder = ImageBuilder::new();
        builder.input(code(16));
        builder.dylib(Dylib::lib_system());
        builder.entry_offset(0x4000);
        builder
            .symbols()
            .add(OutputSymbol::exported("_main", 1, 0x1_0000_4000))
            .add(OutputSymbol::undefined("_exit", 1));
        builder.content(0, vec![0u8; 16]);
        builder.build().expect("builds")
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
    }

    #[test]
    fn the_image_starts_with_a_valid_mach_header() {
        let image = minimal();
        assert_eq!(read_u32(&image.bytes, 0), MH_MAGIC_64);
        assert_eq!(read_u32(&image.bytes, 4), CPU_TYPE_ARM64);
        assert_eq!(read_u32(&image.bytes, 12), MH_EXECUTE);
        // No thread-local sections in a minimal image, so the TLV bit is
        // absent — the flags are the unconditional base set.
        assert_eq!(read_u32(&image.bytes, 24), BASE_EXECUTABLE_FLAGS);
        assert_eq!(read_u32(&image.bytes, 24) & MH_HAS_TLV_DESCRIPTORS, 0);
    }

    /// The header's `ncmds` and `sizeofcmds` are reserved during emission and
    /// patched afterwards; if the patch were missed they would stay zero and
    /// dyld would see an image with no load commands.
    #[test]
    fn the_header_records_the_commands_that_were_emitted() {
        let image = minimal();
        let count = read_u32(&image.bytes, MachHeader::COMMAND_COUNT_OFFSET);
        let size = read_u32(&image.bytes, MachHeader::COMMAND_SIZE_OFFSET);

        assert!(count > 0, "ncmds was never patched");
        assert!(size > 0, "sizeofcmds was never patched");

        // Walking the list must consume exactly `sizeofcmds` and land on the
        // count the header claims — the same walk dyld performs.
        let mut cursor = MachHeader::SIZE;
        let end = MachHeader::SIZE + size as usize;
        let mut walked = 0u32;
        while cursor < end {
            let command_size = read_u32(&image.bytes, cursor + 4) as usize;
            assert!(command_size > 0, "a command declared a zero size");
            assert_eq!(command_size % 4, 0, "a command size was not aligned");
            cursor += command_size;
            walked += 1;
        }
        assert_eq!(cursor, end, "the command walk overran sizeofcmds");
        assert_eq!(
            walked, count,
            "the walk found a different number of commands"
        );
    }

    #[test]
    fn section_content_lands_at_the_offset_layout_assigned() {
        let mut builder = ImageBuilder::new();
        builder.input(code(8));
        builder.dylib(Dylib::lib_system());
        // A recognisable pattern to find in the output.
        builder.content(0, vec![0xAB; 8]);
        let image = builder.build().expect("builds");

        let section = image.layout.find("__TEXT", "__text").expect("present");
        let offset = section.file_offset.expect("has bytes") as usize;
        assert_eq!(&image.bytes[offset..offset + 8], &[0xAB; 8]);
    }

    #[test]
    fn content_does_not_overlap_the_load_commands() {
        let image = minimal();
        let size = read_u32(&image.bytes, MachHeader::COMMAND_SIZE_OFFSET) as u64;
        let commands_end = MachHeader::SIZE as u64 + size;

        for section in &image.layout.sections {
            if let Some(offset) = section.file_offset {
                assert!(
                    offset >= commands_end,
                    "{} at {offset} overlaps the load commands ending at {commands_end}",
                    section.qualified_name()
                );
            }
        }
    }

    #[test]
    fn the_symbol_table_lands_where_lc_symtab_says() {
        let image = minimal();

        // Find LC_SYMTAB by walking, then read back what it points at.
        let size = read_u32(&image.bytes, MachHeader::COMMAND_SIZE_OFFSET) as usize;
        let mut cursor = MachHeader::SIZE;
        let mut symtab = None;
        while cursor < MachHeader::SIZE + size {
            if read_u32(&image.bytes, cursor) == LC_SYMTAB {
                symtab = Some(cursor);
                break;
            }
            cursor += read_u32(&image.bytes, cursor + 4) as usize;
        }

        let symtab = symtab.expect("LC_SYMTAB is present");
        let symbol_offset = read_u32(&image.bytes, symtab + 8) as usize;
        let symbol_count = read_u32(&image.bytes, symtab + 12);
        let string_offset = read_u32(&image.bytes, symtab + 16) as usize;

        assert_eq!(symbol_count, 2);
        assert!(
            symbol_offset + (symbol_count as usize * 16) <= image.bytes.len(),
            "the symbol table runs past the end of the file"
        );
        // The string table opens with a NUL.
        assert_eq!(image.bytes[string_offset], 0);
    }

    #[test]
    fn the_symbol_table_is_eight_byte_aligned() {
        // Each nlist_64 ends in a u64; an unaligned table is misread.
        let image = minimal();
        let size = read_u32(&image.bytes, MachHeader::COMMAND_SIZE_OFFSET) as usize;
        let mut cursor = MachHeader::SIZE;
        while cursor < MachHeader::SIZE + size {
            if read_u32(&image.bytes, cursor) == LC_SYMTAB {
                let offset = read_u32(&image.bytes, cursor + 8);
                assert_eq!(offset % 8, 0, "symbol table is not 8-byte aligned");
                return;
            }
            cursor += read_u32(&image.bytes, cursor + 4) as usize;
        }
        panic!("LC_SYMTAB not found");
    }

    #[test]
    fn every_segments_file_range_lies_within_the_image() {
        let image = minimal();
        let size = read_u32(&image.bytes, MachHeader::COMMAND_SIZE_OFFSET) as usize;
        let mut cursor = MachHeader::SIZE;
        let mut segments = 0;

        while cursor < MachHeader::SIZE + size {
            if read_u32(&image.bytes, cursor) == LC_SEGMENT_64 {
                let file_offset =
                    u64::from_le_bytes(image.bytes[cursor + 40..cursor + 48].try_into().unwrap());
                let file_size =
                    u64::from_le_bytes(image.bytes[cursor + 48..cursor + 56].try_into().unwrap());
                assert!(
                    file_offset + file_size <= image.bytes.len() as u64,
                    "a segment claims bytes past the end of the file"
                );
                segments += 1;
            }
            cursor += read_u32(&image.bytes, cursor + 4) as usize;
        }
        assert!(segments >= 3, "expected several segments, found {segments}");
    }

    /// Padding or truncating would produce a file whose commands disagree with
    /// its bytes — it would load and then misbehave.
    #[test]
    fn content_of_the_wrong_size_is_refused() {
        let mut builder = ImageBuilder::new();
        builder.input(code(16));
        builder.content(0, vec![0u8; 8]);
        assert!(matches!(
            builder.build(),
            Err(ImageError::ContentSizeMismatch { .. })
        ));
    }

    #[test]
    fn content_for_a_nonexistent_section_is_refused() {
        let mut builder = ImageBuilder::new();
        builder.input(code(16));
        builder.content(99, vec![0u8; 16]);
        assert!(matches!(
            builder.build(),
            Err(ImageError::UnknownSection { .. })
        ));
    }

    #[test]
    fn a_dylib_is_recorded_as_a_load_command() {
        let image = minimal();
        let size = read_u32(&image.bytes, MachHeader::COMMAND_SIZE_OFFSET) as usize;
        let mut cursor = MachHeader::SIZE;
        let mut found = false;
        while cursor < MachHeader::SIZE + size {
            if read_u32(&image.bytes, cursor) == LC_LOAD_DYLIB {
                let name_offset = read_u32(&image.bytes, cursor + 8) as usize;
                let start = cursor + name_offset;
                let name = &image.bytes[start..start + 25];
                assert_eq!(name, b"/usr/lib/libSystem.B.dyli");
                found = true;
            }
            cursor += read_u32(&image.bytes, cursor + 4) as usize;
        }
        assert!(found, "LC_LOAD_DYLIB was not emitted");
    }

    #[test]
    fn an_image_with_no_inputs_is_still_structurally_valid() {
        let image = ImageBuilder::new().build().expect("builds");
        assert_eq!(read_u32(&image.bytes, 0), MH_MAGIC_64);
        assert!(read_u32(&image.bytes, MachHeader::COMMAND_COUNT_OFFSET) > 0);
    }

    #[test]
    fn building_the_same_inputs_twice_produces_identical_bytes() {
        // Reproducibility is a spec requirement.
        assert_eq!(minimal().bytes, minimal().bytes);
    }
}
