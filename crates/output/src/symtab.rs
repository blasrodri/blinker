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

    /// This symbol as a table entry, with its name placed in `strings`.
    ///
    /// The single symbol's worth of work, exposed so a caller can do it a run
    /// at a time. [`SymbolTableBuilder`] does the whole table in one pass and
    /// is the right thing for a link with nothing to reuse; a caller that
    /// knows which objects did not change resolves only theirs and keeps the
    /// rest, which it can only do if the entries come out grouped by object.
    pub fn entry(&self, strings: &mut StringTable) -> NlistEntry {
        NlistEntry {
            name_offset: strings.offset(self.key, self.name.as_ref()),
            type_byte: self.type_byte(),
            section: self.section.unwrap_or(NO_SECT),
            desc: self.desc(),
            value: self.value,
        }
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

/// The string table, kept across links so a name keeps its offset.
///
/// # Why this outlives one link
///
/// `nlist_64` refers to a name by its byte offset into this blob, so an offset
/// is a name's identity as far as the symbol table is concerned. Built fresh
/// each link the offsets are a running total, and a running total means one
/// added name shifts every name after it — which makes every retained answer
/// above it worthless, exactly as a dense atom numbering did for reachability
/// (finding 194). It is the same problem and it takes the same shape of fix:
/// give the thing a stable identity, and let what did not change stay put.
///
/// So this appends and never rewrites. A name interned by an earlier link
/// keeps the offset that link gave it, for as long as the table lives, which
/// is what makes a retained `NlistEntry` mean anything: its `name_offset`
/// still points at its name.
///
/// # What it costs
///
/// Names of symbols that have gone away stay in the blob. They are unreachable
/// — nothing indexes them — and a Mach-O string table containing bytes no
/// symbol points at is well formed, but they are still bytes in the output
/// file. So the table is discarded and rebuilt once the padding outweighs what
/// is live, on the same argument and with the same constant as the atom
/// numbering's `SPREAD`: a link that rebuilds pays what every link used to pay,
/// and it buys the next several links their stability back.
#[derive(Debug)]
pub struct StringTable {
    /// The blob itself, shared with the [`SymbolTable`] built from it so that
    /// emitting the image does not copy 82 MB of names.
    bytes: std::sync::Arc<Vec<u8>>,
    /// Interning id to offset, or [`Self::UNSET`]. A plain vector because an
    /// interning id is a dense integer from zero — see [`OutputSymbol::key`].
    by_key: Vec<u32>,
    /// Names with no caller-supplied identity: the debug map's synthesised
    /// directory, file and object paths. Three per object, so this is
    /// thousands of entries where `by_key` is millions.
    by_text: blinker_hashing::FastMap<Box<str>, u32>,
    /// How many times this table has been thrown away and started again.
    ///
    /// Anything retained against the offsets it used to hand out — a run of
    /// `NlistEntry` belonging to an object that has not changed — is valid only
    /// while this stands still. It survives a rebuild so that the rebuild is
    /// *visible*: a counter reset to zero would say "same table as before".
    rebuilds: u32,
}

impl Default for StringTable {
    fn default() -> Self {
        StringTable {
            // Opens with a NUL so offset 0 is the empty string, which is what
            // an unnamed symbol points at.
            bytes: std::sync::Arc::new(vec![0]),
            by_key: Vec::new(),
            by_text: blinker_hashing::FastMap::default(),
            rebuilds: 0,
        }
    }
}

impl StringTable {
    /// No offset recorded for this id yet.
    ///
    /// A sentinel rather than `Option<u32>` because this vector has one entry
    /// per interned name — millions on a debug link — and the niche would
    /// double it.
    const UNSET: u32 = u32::MAX;

    /// Past this ratio of blob to live names, start again.
    ///
    /// The same bound and the same reasoning as the atom numbering's `SPREAD`:
    /// stability is worth carrying padding for, and is not worth carrying
    /// unboundedly.
    const SPREAD: usize = 2;

    pub fn new() -> Self {
        Self::default()
    }

    /// Discard this table if `live` bytes are no longer most of it.
    ///
    /// # Why the caller says what is live
    ///
    /// This used to count it here, marking each name as the link asked for it.
    /// That works only while every name *is* asked for — and the point of a
    /// stable table is that a caller reusing an unchanged object's entries
    /// never asks. Names reached only through reused entries went uncounted,
    /// the table looked almost entirely stale on every link, and it would have
    /// rebuilt itself every time: the accounting would have destroyed the
    /// property it was there to protect.
    ///
    /// So the caller measures instead, and it measures the one thing it can
    /// know exactly — how many bytes each object's run *appended*, which is
    /// `len()` before and after resolving it. Summed over the runs still held,
    /// that is a lower bound on what is live, because a run that has since been
    /// replaced may have appended names another run still refers to. A lower
    /// bound errs towards rebuilding, which is the safe direction: a rebuild
    /// costs one link its reuse, and carrying garbage costs every link after
    /// it a larger file.
    pub fn rebuild_unless_live(&mut self, live: usize) {
        if self.bytes.len() <= Self::SPREAD * live.max(1) {
            return;
        }
        let rebuilds = self.rebuilds;
        *self = StringTable::new();
        self.rebuilds = rebuilds + 1;
    }

    /// An identity for the run of offsets this table is currently handing out.
    ///
    /// Changes exactly when the table starts again, so a caller holding entries
    /// resolved against it can tell in one comparison whether they still mean
    /// anything. Not a hash of the contents: appending leaves every offset
    /// already given out exactly where it was, which is the whole point.
    pub fn offsets_id(&self) -> u32 {
        self.rebuilds
    }

    /// Bytes handed out so far. Differenced across a run to learn what that
    /// run appended; see [`Self::rebuild_unless_live`].
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether this table was built by an earlier link.
    ///
    /// A fresh table gives out the same offsets a from-scratch build would, so
    /// nothing retained against a previous one may be reused across it.
    pub fn is_empty(&self) -> bool {
        self.bytes.len() == 1
    }

    pub fn bytes(&self) -> &std::sync::Arc<Vec<u8>> {
        &self.bytes
    }

    /// Roughly what holding this costs. The blob dominates — 82 MB against a
    /// few for the indexes on a debug rust-analyzer link.
    pub fn held_bytes(&self) -> usize {
        self.bytes.len()
            + self.by_key.len() * std::mem::size_of::<u32>()
            + self
                .by_text
                .keys()
                .map(|name| name.len() + std::mem::size_of::<(Box<str>, u32)>())
                .sum::<usize>()
    }

    /// Where `name` sits, appending it if this is the first time it is asked
    /// for. `key` is the caller's identity for the name, or
    /// [`OutputSymbol::UNKEYED`].
    pub fn offset(&mut self, key: u32, name: &str) -> u32 {
        if name.is_empty() {
            return 0;
        }
        if key == OutputSymbol::UNKEYED {
            if let Some(offset) = self.by_text.get(name) {
                return *offset;
            }
            let offset = self.append(name);
            self.by_text.insert(name.into(), offset);
            return offset;
        }
        let at = key as usize;
        if at >= self.by_key.len() {
            self.by_key.resize(at + 1, Self::UNSET);
        }
        if self.by_key[at] != Self::UNSET {
            return self.by_key[at];
        }
        let offset = self.append(name);
        self.by_key[at] = offset;
        offset
    }

    fn append(&mut self, name: &str) -> u32 {
        // Unique between links: the `SymbolTable` sharing it is dropped with
        // the image that was built from it. A clone here would be correct and
        // slow, which is the right way round for something that must not be
        // able to hand out a stale offset.
        let bytes = std::sync::Arc::make_mut(&mut self.bytes);
        let offset = bytes.len() as u32;
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        offset
    }
}

/// Builds a correctly grouped symbol table and its string table.
#[derive(Debug, Default)]
pub struct SymbolTableBuilder<'a> {
    symbols: Vec<OutputSymbol<'a>>,
    /// Entries whose names a caller already placed, one vector per group in
    /// [`SymbolGroup`] order. Set by [`Self::take_groups`], and then `symbols`
    /// is not consulted at all.
    groups: Option<[Vec<NlistEntry>; 3]>,
}

impl<'a> SymbolTableBuilder<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, symbol: OutputSymbol<'a>) -> &mut Self {
        self.symbols.push(symbol);
        self
    }

    /// Take a whole batch, in the order given.
    ///
    /// The first one is taken outright rather than appended to what is there.
    /// A caller with 1.7 million symbols has them in a vector already, and
    /// moving that vector in is the difference between naming the batch and
    /// copying 81 MB of it.
    /// Supply the table already grouped, with every name already placed.
    ///
    /// The three vectors are the three groups in [`SymbolGroup`] order, and
    /// their contents go into the table exactly as given — the contiguity
    /// `LC_DYSYMTAB` requires comes from the grouping, and the order within a
    /// group is the caller's to decide. Only for a caller that resolved names
    /// against the very [`StringTable`] this table will be built with; entries
    /// carrying offsets from any other one name the wrong symbols.
    pub fn take_groups(&mut self, groups: [Vec<NlistEntry>; 3]) -> &mut Self {
        self.groups = Some(groups);
        self
    }

    pub fn absorb(&mut self, symbols: Vec<OutputSymbol<'a>>) -> &mut Self {
        if self.symbols.is_empty() {
            self.symbols = symbols;
        } else {
            self.symbols.extend(symbols);
        }
        self
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Produce the symbol table and the group ranges, against a fresh string
    /// table. For callers with nothing to retain.
    pub fn build(self) -> SymbolTable {
        let mut strings = StringTable::new();
        self.build_into(&mut strings)
    }

    /// Produce the symbol table, its group ranges, and the offsets `strings`
    /// gives every name.
    ///
    /// Sorting happens here rather than being the caller's responsibility,
    /// because a caller that inserted out of order would produce a file that
    /// loads and then misbehaves.
    ///
    /// # Why the string table is the caller's
    ///
    /// It used to be built here, sized from a pass over every symbol and
    /// filled as the groups were walked. That produces the right bytes and the
    /// wrong offsets: correct for this link and meaningless against the last
    /// one, so nothing built on top of them — an `NlistEntry`, a run of them
    /// belonging to an object that did not change — could be carried forward.
    /// Handed one that outlives the link, the offsets become stable and the
    /// 82 MB of names a debug rust-analyzer link produces stop being rebuilt
    /// (see [`StringTable`]).
    pub fn build_into(self, strings: &mut StringTable) -> SymbolTable {
        if let Some(groups) = self.groups {
            return SymbolTable::grouped(groups, strings);
        }
        let mut entries = Vec::with_capacity(self.symbols.len());

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
                let name_offset = strings.offset(symbol.key, symbol.name.as_ref());

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
        SymbolTable::of(entries, [locals, externals, undefined], strings)
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
    /// Shared with the [`StringTable`] that produced it rather than copied out
    /// of it: on a debug rust-analyzer link this blob is 82 MB, and it is
    /// written to the image once and then dropped.
    pub strings: std::sync::Arc<Vec<u8>>,
    pub groups: SymbolGroups,
}

impl SymbolTable {
    /// One table from the three groups laid end to end, in the order
    /// `LC_DYSYMTAB` requires them.
    fn grouped(groups: [Vec<NlistEntry>; 3], strings: &StringTable) -> SymbolTable {
        let counts = [
            groups[0].len() as u32,
            groups[1].len() as u32,
            groups[2].len() as u32,
        ];
        let [locals, externals, undefined] = groups;
        let mut entries = locals;
        entries.extend(externals);
        entries.extend(undefined);
        SymbolTable::of(entries, counts, strings)
    }

    /// The table and the ranges that describe it. `counts` is per group, in
    /// [`SymbolGroup`] order, and must sum to `entries.len()` — the ranges are
    /// what dyld and the debugger index by, so a count that disagrees with the
    /// entries is a file that is read wrongly rather than rejected.
    fn of(entries: Vec<NlistEntry>, counts: [u32; 3], strings: &StringTable) -> SymbolTable {
        let [locals, externals, undefined] = counts;
        debug_assert_eq!(
            (locals + externals + undefined) as usize,
            entries.len(),
            "the group counts do not cover the symbol table"
        );
        SymbolTable {
            entries,
            strings: std::sync::Arc::clone(strings.bytes()),
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
        assert_eq!(*table.strings, vec![0]);
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
