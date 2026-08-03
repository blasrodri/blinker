//! The symbol table and its string table.
//!
//! # Sorting is load-bearing
//!
//! `LC_DYSYMTAB` does not list which symbols are local, external, or undefined
//! — it records three *index ranges* into the symbol table. dyld and the
//! debugger then index by range. So the table must be sorted into those three
//! groups, in that order, or consumers silently read the wrong entries rather
//! than rejecting the file.
//!
//! A real Rust executable:
//!
//! ```text
//! ilocalsym    0     nlocalsym   2646
//! iextdefsym   2646  nextdefsym   262
//! iundefsym    2908  nundefsym     70
//! ```
//!
//! Contiguous, in order, covering every symbol. [`SymbolTableBuilder`]
//! guarantees that shape by construction rather than trusting callers to
//! insert in the right sequence.

use crate::commands::SymbolGroups;
use crate::format::Writer;

/// `n_type` bit fields from `<mach-o/nlist.h>`.
pub mod n_type {
    /// Mask selecting the debug-symbol bits. Any of these set means a stab.
    pub const N_STAB: u8 = 0xe0;
    /// Private external: visible to the link but not exported.
    pub const N_PEXT: u8 = 0x10;
    /// Mask selecting the type field.
    pub const N_TYPE: u8 = 0x0e;
    /// External — participates in dynamic linking.
    pub const N_EXT: u8 = 0x01;

    /// Undefined; no section.
    pub const N_UNDF: u8 = 0x0;
    /// Absolute; no section.
    pub const N_ABS: u8 = 0x2;
    /// Defined in the section given by `n_sect`.
    pub const N_SECT: u8 = 0xe;
}

/// `n_sect` value meaning "no section".
pub const NO_SECT: u8 = 0;

/// Which of the three groups a symbol belongs to.
///
/// The order of these variants *is* the required table order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolGroup {
    /// Not visible outside the image.
    Local,
    /// Defined here and exported.
    ExternalDefined,
    /// Referenced here, defined elsewhere.
    Undefined,
}

/// One symbol to place in the table.
///
/// # Why the name is borrowed
///
/// Almost every name here already exists, in the parsed object the symbol came
/// out of, and that outlives the link. Owning them meant 1,689,759 `String`
/// allocations and 82 MB of copying per link on a debug rust-analyzer image —
/// to hand the same bytes to a string table that copies them again (finding
/// 159). A `Cow` because the debug map also synthesises a few thousand names
/// that belong to nothing: the compilation unit's directory, file and object
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSymbol<'a> {
    pub name: std::borrow::Cow<'a, str>,
    /// An identity for `name`, when the caller already has one.
    ///
    /// The string table deduplicates names, and probing it by text meant
    /// hashing 1.69 million mangled names — most of them twice, since the
    /// debug map names every function again — for a comparison the caller
    /// could answer with an integer. A linker that interns its symbol names
    /// puts the interned id here.
    ///
    /// **Two symbols with the same key must have the same name.** Nothing
    /// checks it, and a wrong key points a symbol at another symbol's string.
    /// [`OutputSymbol::UNKEYED`] means "no identity" and falls back to the
    /// text, which is what a synthesised name — a source path, an empty stab —
    /// gets.
    pub key: u32,
    pub group: SymbolGroup,
    /// One-based output section number; `None` for undefined symbols.
    pub section: Option<u8>,
    /// Virtual address for a definition, zero for a reference.
    pub value: u64,
    /// Index of the dylib providing an undefined symbol.
    ///
    /// Under the two-level namespace this is not optional: dyld records *which*
    /// library a symbol came from, and an ordinal of zero would mean
    /// flat-namespace lookup instead.
    pub library_ordinal: u8,
    /// Private external — external to the link, not exported from the image.
    pub private_external: bool,
    /// A debug-map entry, carrying its own `n_type` and `n_desc`.
    ///
    /// Stabs do not follow the rules the other three groups do: `n_type` is a
    /// stab kind rather than a composition of `N_SECT`/`N_EXT`, and `n_desc`
    /// means something different for each kind. They are set here rather than
    /// derived. See [`crate::symtab::stab`].
    pub stab: Option<(u8, u16)>,
}

/// `n_type` values for the debug-map stabs, from `<mach-o/stab.h>`.
///
/// The debug map is how a Mach-O executable says where its debug information
/// *is* rather than carrying it: the DWARF stays in the `.o` files, and the
/// executable holds a table saying which object each function came from and
/// what address it ended up at. `dsymutil` reads it to build a `.dSYM`, and
/// `lldb` reads it directly.
pub mod stab {
    /// Source file. Emitted as a pair — directory then name — to open a
    /// compilation unit, and once more with an empty name to close it.
    pub const SO: u8 = 0x64;
    /// Object file: the path to the `.o` holding this unit's DWARF, with its
    /// modification time in `n_value` so a consumer can tell it has gone
    /// stale.
    pub const OSO: u8 = 0x66;
    /// Function. Emitted twice: once named, at its address, then once unnamed
    /// carrying its size.
    pub const FUN: u8 = 0x24;
    /// Global data, in the object that defines it.
    pub const GSYM: u8 = 0x20;
    /// Static data, at its address.
    pub const STSYM: u8 = 0x26;
    /// Begin and end of a function's range. Redundant with the `FUN` pair, and
    /// emitted because `ld` emits them.
    pub const BNSYM: u8 = 0x2e;
    pub const ENSYM: u8 = 0x4e;
}

impl<'a> OutputSymbol<'a> {
    /// No caller-supplied identity for this name; deduplicate it by text.
    ///
    /// A sentinel rather than `Option<u32>` because this struct is built
    /// 1.7 million times per link and sorted twice, and the niche would cost
    /// four bytes in every one of them.
    pub const UNKEYED: u32 = u32::MAX;

    /// A local definition.
    pub fn local(name: impl Into<std::borrow::Cow<'a, str>>, section: u8, value: u64) -> Self {
        OutputSymbol {
            name: name.into(),
            key: OutputSymbol::UNKEYED,
            group: SymbolGroup::Local,
            section: Some(section),
            value,
            library_ordinal: 0,
            private_external: false,
            stab: None,
        }
    }

    /// An exported definition.
    pub fn exported(name: impl Into<std::borrow::Cow<'a, str>>, section: u8, value: u64) -> Self {
        OutputSymbol {
            name: name.into(),
            key: OutputSymbol::UNKEYED,
            group: SymbolGroup::ExternalDefined,
            section: Some(section),
            value,
            library_ordinal: 0,
            private_external: false,
            stab: None,
        }
    }

    /// A reference satisfied by `library_ordinal`.
    pub fn undefined(name: impl Into<std::borrow::Cow<'a, str>>, library_ordinal: u8) -> Self {
        OutputSymbol {
            name: name.into(),
            key: OutputSymbol::UNKEYED,
            group: SymbolGroup::Undefined,
            section: None,
            value: 0,
            library_ordinal,
            private_external: false,
            stab: None,
        }
    }

    /// One debug-map entry.
    ///
    /// Grouped with the locals because that is where `LC_DYSYMTAB` expects
    /// them: a stab is not external and not undefined, so the only range it
    /// can live in is the first one. `ld` emits them after the ordinary
    /// locals, and [`SymbolTableBuilder::build`] preserves insertion order
    /// within a group, so callers control that by when they add them.
    pub fn stab(
        kind: u8,
        name: impl Into<std::borrow::Cow<'a, str>>,
        section: u8,
        desc: u16,
        value: u64,
    ) -> Self {
        OutputSymbol {
            name: name.into(),
            key: OutputSymbol::UNKEYED,
            group: SymbolGroup::Local,
            section: Some(section),
            value,
            library_ordinal: 0,
            private_external: false,
            stab: Some((kind, desc)),
        }
    }

    /// Give this symbol's name a caller-supplied identity. See [`Self::key`].
    pub fn keyed(mut self, key: u32) -> Self {
        self.key = key;
        self
    }

    /// The `n_type` byte for this symbol.
    fn type_byte(&self) -> u8 {
        // A stab's type is the stab kind itself, not a composition of the
        // N_SECT/N_EXT bits: the two encodings share the byte and are told
        // apart by whether any N_STAB bit is set.
        if let Some((kind, _)) = self.stab {
            return kind;
        }
        let mut byte = match self.group {
            SymbolGroup::Undefined => n_type::N_UNDF,
            _ => n_type::N_SECT,
        };
        if self.group != SymbolGroup::Local {
            byte |= n_type::N_EXT;
        }
        if self.private_external {
            byte |= n_type::N_PEXT;
        }
        byte
    }

    /// The `n_desc` field.
    ///
    /// For an undefined symbol the library ordinal lives in the high byte,
    /// which is how the two-level namespace records the providing library.
    fn desc(&self) -> u16 {
        if let Some((_, desc)) = self.stab {
            return desc;
        }
        match self.group {
            SymbolGroup::Undefined => (self.library_ordinal as u16) << 8,
            _ => 0,
        }
    }
}

/// Builds a correctly grouped symbol table and its string table.
#[derive(Debug, Default)]
pub struct SymbolTableBuilder<'a> {
    symbols: Vec<OutputSymbol<'a>>,
}

impl<'a> SymbolTableBuilder<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, symbol: OutputSymbol<'a>) -> &mut Self {
        self.symbols.push(symbol);
        self
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Produce the symbol table, string table, and the group ranges.
    ///
    /// Sorting happens here rather than being the caller's responsibility,
    /// because a caller that inserted out of order would produce a file that
    /// loads and then misbehaves.
    pub fn build(self) -> SymbolTable {
        // The string table opens with a NUL so index 0 is the empty string,
        // which is what an unnamed symbol points at.
        //
        // Sized before it is filled. rust-analyzer's debug image comes to 82 MB
        // of names, and growing there from one byte is twenty-seven doublings
        // that copy about 160 MB on the way (135). The bound is the total
        // length of every name; interning brings the real size below it, and
        // the difference is transient.
        let bytes: usize = self
            .symbols
            .iter()
            .map(|symbol| symbol.name.len() + 1)
            .sum();
        let mut strings = Vec::with_capacity(bytes + 1);
        strings.push(0u8);
        let mut entries = Vec::with_capacity(self.symbols.len());
        // Names repeat: the debug map names every function a second time, so
        // half the string table would otherwise be a copy of the other half.
        // First occurrence wins, and insertion order is already deterministic,
        // so the offsets are too.
        // Fast-hashed, not `std`'s SipHash. This map is probed once per symbol,
        // and a debug build's symbol count is what makes `emit_linkedit` the
        // worst-scaling stage in the link — 7.2x when the work grew 3.7x
        // (finding 130). The reasoning is the one in `blinker_hashing`: every
        // key here comes from an object file the linker was told to read.
        // Sized too, for the same reason: it is probed once per symbol and ends
        // up holding one entry per distinct name, and 1.7 million inserts into
        // a table that starts empty rehash everything already in it once per
        // doubling.
        let mut interned: blinker_hashing::FastMap<&str, u32> =
            blinker_hashing::FastMap::with_capacity_and_hasher(
                self.symbols.len(),
                Default::default(),
            );
        // The same map, for names whose identity the caller already knows.
        // Separate rather than one map over an enum key: the whole point is
        // that this one hashes four bytes instead of a hundred.
        let mut by_key: blinker_hashing::FastMap<u32, u32> =
            blinker_hashing::FastMap::with_capacity_and_hasher(
                self.symbols.len(),
                Default::default(),
            );

        // The three groups, in the order the table requires them, without
        // moving anything to get there.
        //
        // This was a stable sort by `group` — 1,689,759 entries of 48 bytes,
        // 81 MB rearranged to put a three-valued key in order, and 10-17 ms of
        // every link. Walking the symbols once per group visits them in exactly
        // the order the sort produced (groups in variant order, insertion order
        // within a group, which is what made the sort stable) and reads them
        // where they already are. The counts the table header needs fall out of
        // the same walk instead of a fourth pass.
        let mut counts = [0u32; 3];
        for group in [
            SymbolGroup::Local,
            SymbolGroup::ExternalDefined,
            SymbolGroup::Undefined,
        ] {
            for symbol in self.symbols.iter().filter(|s| s.group == group) {
                counts[group as usize] += 1;
                let name_offset = if symbol.name.is_empty() {
                    0
                } else if symbol.key != OutputSymbol::UNKEYED {
                    match by_key.entry(symbol.key) {
                        std::collections::hash_map::Entry::Occupied(held) => *held.get(),
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            let offset = strings.len() as u32;
                            strings.extend_from_slice(symbol.name.as_bytes());
                            strings.push(0);
                            slot.insert(offset);
                            offset
                        }
                    }
                } else if let Some(offset) = interned.get(symbol.name.as_ref()) {
                    *offset
                } else {
                    let offset = strings.len() as u32;
                    strings.extend_from_slice(symbol.name.as_bytes());
                    strings.push(0);
                    interned.insert(symbol.name.as_ref(), offset);
                    offset
                };

                entries.push(NlistEntry {
                    name_offset,
                    type_byte: symbol.type_byte(),
                    section: symbol.section.unwrap_or(NO_SECT),
                    desc: symbol.desc(),
                    value: symbol.value,
                });
            }
        }
        let [locals, externals, undefined] = counts;

        SymbolTable {
            entries,
            strings,
            groups: SymbolGroups {
                local_index: 0,
                local_count: locals,
                external_index: locals,
                external_count: externals,
                undefined_index: locals + externals,
                undefined_count: undefined,
                indirect_offset: 0,
                indirect_count: 0,
            },
        }
    }
}

/// One `nlist_64` entry: 16 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NlistEntry {
    pub name_offset: u32,
    pub type_byte: u8,
    pub section: u8,
    pub desc: u16,
    pub value: u64,
}

impl NlistEntry {
    /// Size on disk.
    pub const SIZE: usize = 16;

    pub fn write(&self, writer: &mut Writer) {
        writer
            .u32(self.name_offset)
            .bytes(&[self.type_byte, self.section])
            .bytes(&self.desc.to_le_bytes())
            .u64(self.value);
    }
}

/// A built symbol table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTable {
    pub entries: Vec<NlistEntry>,
    pub strings: Vec<u8>,
    pub groups: SymbolGroups,
}

impl SymbolTable {
    /// Serialize the `nlist_64` array.
    pub fn write_entries(&self, writer: &mut Writer) {
        for entry in &self.entries {
            entry.write(writer);
        }
    }

    /// Total size of the symbol table on disk.
    pub fn entries_size(&self) -> usize {
        self.entries.len() * NlistEntry::SIZE
    }

    pub fn strings_size(&self) -> usize {
        self.strings.len()
    }

    /// Read a name back out of the string table, for tests and diagnostics.
    pub fn name_at(&self, offset: u32) -> Option<&str> {
        let start = offset as usize;
        let bytes = self.strings.get(start..)?;
        let end = bytes.iter().position(|&b| b == 0)?;
        std::str::from_utf8(&bytes[..end]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built() -> SymbolTable {
        let mut builder = SymbolTableBuilder::new();
        // Deliberately inserted out of order: the builder must sort.
        builder
            .add(OutputSymbol::undefined("_malloc", 1))
            .add(OutputSymbol::local("ltmp0", 1, 0x1_0000_1000))
            .add(OutputSymbol::exported("_main", 1, 0x1_0000_2000))
            .add(OutputSymbol::local("ltmp1", 1, 0x1_0000_1100))
            .add(OutputSymbol::undefined("_free", 1));
        builder.build()
    }

    /// Every symbol reads back the name it was given, whichever way its
    /// string-table offset was found.
    ///
    /// The keyed path exists so the debug map's repeat of a function's name
    /// costs an integer hash instead of hashing the mangled text again. Its
    /// precondition — equal keys mean equal names — is the caller's, and the
    /// thing that must hold here is weaker and more important: a symbol's
    /// offset points at *its own* name.
    #[test]
    fn a_keyed_name_and_a_text_matched_one_both_read_back() {
        let mut builder = SymbolTableBuilder::new();
        builder
            // Two entries sharing a key: one string between them.
            .add(OutputSymbol::local("_shared", 1, 0x1000).keyed(7))
            .add(OutputSymbol::stab(stab::FUN, "_shared", 1, 0, 0x1000).keyed(7))
            // The same text with no key at all — deduplicated by text, and
            // against the *other* unkeyed entry rather than the keyed one.
            .add(OutputSymbol::local("_shared", 1, 0x2000))
            .add(OutputSymbol::local("_shared", 1, 0x3000))
            // A different key must not share the first one's string.
            .add(OutputSymbol::local("_other", 1, 0x4000).keyed(8));
        let table = builder.build();

        for entry in &table.entries {
            let name = table.name_at(entry.name_offset).expect("a name");
            let expected = if entry.value == 0x4000 {
                "_other"
            } else {
                "_shared"
            };
            assert_eq!(name, expected, "at {:#x}", entry.value);
        }

        // And the keyed pair really did share, which is the point.
        let keyed: Vec<u32> = table
            .entries
            .iter()
            .filter(|e| e.value == 0x1000)
            .map(|e| e.name_offset)
            .collect();
        assert_eq!(keyed.len(), 2);
        assert_eq!(keyed[0], keyed[1], "a shared key wrote the name twice");
    }

    /// The property `LC_DYSYMTAB` depends on: three contiguous ranges, in
    /// order, covering every symbol.
    #[test]
    fn symbols_are_sorted_into_three_contiguous_groups() {
        let table = built();
        let groups = table.groups;

        assert_eq!(groups.local_index, 0);
        assert_eq!(groups.local_count, 2);
        assert_eq!(groups.external_index, 2);
        assert_eq!(groups.external_count, 1);
        assert_eq!(groups.undefined_index, 3);
        assert_eq!(groups.undefined_count, 2);

        // Contiguous and complete.
        assert_eq!(
            groups.undefined_index + groups.undefined_count,
            table.entries.len() as u32
        );
    }

    #[test]
    fn the_groups_appear_in_the_required_order() {
        let table = built();
        let names: Vec<&str> = table
            .entries
            .iter()
            .map(|e| table.name_at(e.name_offset).expect("named"))
            .collect();
        assert_eq!(names, vec!["ltmp0", "ltmp1", "_main", "_malloc", "_free"]);
    }

    #[test]
    fn insertion_order_is_preserved_within_a_group() {
        // A stable sort keeps the output deterministic run to run.
        let table = built();
        assert_eq!(table.name_at(table.entries[0].name_offset), Some("ltmp0"));
        assert_eq!(table.name_at(table.entries[1].name_offset), Some("ltmp1"));
    }

    #[test]
    fn locals_are_not_marked_external() {
        let table = built();
        let local = &table.entries[0];
        assert_eq!(local.type_byte & n_type::N_EXT, 0);
        assert_eq!(local.type_byte & n_type::N_TYPE, n_type::N_SECT);
    }

    #[test]
    fn exported_definitions_are_marked_external_and_in_a_section() {
        let table = built();
        let exported = &table.entries[2];
        assert_eq!(exported.type_byte & n_type::N_EXT, n_type::N_EXT);
        assert_eq!(exported.type_byte & n_type::N_TYPE, n_type::N_SECT);
        assert_eq!(exported.section, 1);
        assert_eq!(exported.value, 0x1_0000_2000);
    }

    #[test]
    fn undefined_symbols_have_no_section_and_no_value() {
        let table = built();
        let undefined = &table.entries[3];
        assert_eq!(undefined.type_byte & n_type::N_TYPE, n_type::N_UNDF);
        assert_eq!(undefined.section, NO_SECT);
        assert_eq!(undefined.value, 0);
    }

    /// Under the two-level namespace dyld records which library provides each
    /// import. An ordinal of zero would mean flat-namespace lookup instead.
    #[test]
    fn the_library_ordinal_is_carried_in_the_high_byte_of_desc() {
        let mut builder = SymbolTableBuilder::new();
        builder.add(OutputSymbol::undefined("_printf", 3));
        let table = builder.build();
        assert_eq!(table.entries[0].desc >> 8, 3);
    }

    #[test]
    fn private_external_symbols_carry_the_pext_bit() {
        let mut builder = SymbolTableBuilder::new();
        let mut symbol = OutputSymbol::exported("_hidden", 1, 0x1000);
        symbol.private_external = true;
        builder.add(symbol);
        let table = builder.build();
        assert_eq!(table.entries[0].type_byte & n_type::N_PEXT, n_type::N_PEXT);
    }

    #[test]
    fn the_string_table_starts_with_a_nul_so_offset_zero_is_empty() {
        let table = built();
        assert_eq!(table.strings[0], 0);
        assert_eq!(table.name_at(0), Some(""));
    }

    #[test]
    fn every_name_round_trips_through_the_string_table() {
        let table = built();
        for entry in &table.entries {
            let name = table.name_at(entry.name_offset).expect("resolvable");
            assert!(!name.is_empty(), "name lost from the string table");
        }
    }

    #[test]
    fn an_unnamed_symbol_points_at_the_empty_string() {
        let mut builder = SymbolTableBuilder::new();
        builder.add(OutputSymbol::local("", 1, 0));
        let table = builder.build();
        assert_eq!(table.entries[0].name_offset, 0);
        // And the table is not grown by an empty name.
        assert_eq!(table.strings.len(), 1);
    }

    #[test]
    fn names_are_not_deduplicated_but_each_resolves_correctly() {
        // Two symbols may legitimately share a name across groups.
        let mut builder = SymbolTableBuilder::new();
        builder
            .add(OutputSymbol::local("shared", 1, 0x1000))
            .add(OutputSymbol::exported("shared", 1, 0x2000));
        let table = builder.build();
        for entry in &table.entries {
            assert_eq!(table.name_at(entry.name_offset), Some("shared"));
        }
    }

    #[test]
    fn an_nlist_entry_is_exactly_sixteen_bytes() {
        let mut writer = Writer::new();
        NlistEntry {
            name_offset: 1,
            type_byte: n_type::N_SECT | n_type::N_EXT,
            section: 1,
            desc: 0,
            value: 0x1_0000_0000,
        }
        .write(&mut writer);
        assert_eq!(writer.len(), NlistEntry::SIZE);
    }

    #[test]
    fn nlist_fields_are_written_in_the_documented_order() {
        let mut writer = Writer::new();
        NlistEntry {
            name_offset: 0x0102_0304,
            type_byte: 0x0e,
            section: 0x02,
            desc: 0x0300,
            value: 0x0A0B_0C0D_0E0F_1011,
        }
        .write(&mut writer);

        let bytes = writer.as_slice();
        assert_eq!(&bytes[0..4], &0x0102_0304u32.to_le_bytes(), "n_strx");
        assert_eq!(bytes[4], 0x0e, "n_type");
        assert_eq!(bytes[5], 0x02, "n_sect");
        assert_eq!(&bytes[6..8], &0x0300u16.to_le_bytes(), "n_desc");
        assert_eq!(&bytes[8..16], &0x0A0B_0C0D_0E0F_1011u64.to_le_bytes());
    }

    #[test]
    fn sizes_match_what_was_written() {
        let table = built();
        let mut writer = Writer::new();
        table.write_entries(&mut writer);
        assert_eq!(writer.len(), table.entries_size());
        assert_eq!(table.strings_size(), table.strings.len());
    }

    #[test]
    fn an_empty_table_is_valid() {
        let table = SymbolTableBuilder::new().build();
        assert!(table.entries.is_empty());
        assert_eq!(table.groups.local_count, 0);
        assert_eq!(table.groups.undefined_count, 0);
        // The leading NUL is still present.
        assert_eq!(table.strings, vec![0]);
    }

    #[test]
    fn handles_the_scale_of_a_real_binary() {
        // 2646 locals, 262 exported, 70 undefined — the real proportions.
        let mut builder = SymbolTableBuilder::new();
        for i in 0..70 {
            builder.add(OutputSymbol::undefined(format!("_undef{i}"), 1));
        }
        for i in 0..2646 {
            builder.add(OutputSymbol::local(format!("_local{i}"), 1, i as u64));
        }
        for i in 0..262 {
            builder.add(OutputSymbol::exported(format!("_ext{i}"), 1, i as u64));
        }
        let table = builder.build();

        assert_eq!(table.groups.local_count, 2646);
        assert_eq!(table.groups.external_count, 262);
        assert_eq!(table.groups.undefined_count, 70);
        assert_eq!(table.entries.len(), 2978);
        assert_eq!(table.groups.external_index, 2646);
        assert_eq!(table.groups.undefined_index, 2908);
    }
}
