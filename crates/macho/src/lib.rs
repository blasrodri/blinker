//! Parsing ARM64 Mach-O object files into an owned, stable-ID representation.
//!
//! # Why this wraps `object` rather than parsing bytes itself
//!
//! Structural parsing and bounds checking come from the `object` crate, which
//! is fuzzed upstream and already validates every offset and count. What this
//! crate owns is the *representation*: an owned, serialisable form addressed by
//! stable integer IDs, which is what M4's cache and M5's dependency graph need
//! and what a borrowed `MachOFile<'data>` cannot provide. See `DECISIONS.md` D1.
//!
//! One rule makes the wrapping safe: **never consume `object`'s portable
//! abstractions.** Its cross-format `RelocationKind` enum reports most ARM64
//! Mach-O relocations as `Unknown`, which for a linker would be a silent
//! correctness hazard. We read the raw `r_type` / `r_pcrel` / `r_length` fields
//! and map them ourselves, so anything unrecognised is a structured error.
//!
//! # Stable IDs
//!
//! Sections, symbols, and relocations are addressed by index-derived IDs rather
//! than references. That keeps [`ParsedObject`] free of self-referential
//! structure — it serialises directly, and a cached copy stays valid without
//! the file it came from.

use std::path::{Path, PathBuf};

pub mod relocation;
pub use relocation::{Arm64RelocationKind, RelocationError, RelocationLength};

mod parse;
pub use parse::{parse_object, ParseError};

/// Identifier for one parsed object within a link.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ObjectId(pub u32);

/// Index of a section within its object.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct SectionId(pub u32);

/// Index of a symbol within its object.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct SymbolId(pub u32);

/// Index of a relocation within its object.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct RelocationId(pub u32);

/// Target architecture of an object file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Architecture {
    Arm64,
}

/// How strongly a symbol is defined, which decides what wins during resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SymbolStrength {
    /// An ordinary definition. Two strong definitions of one name is an error.
    Strong,
    /// A definition that yields to any strong one, and to which duplicates are
    /// not an error.
    Weak,
    /// A reference with no definition here.
    Undefined,
    /// A reference that is allowed to stay unresolved, resolving to zero.
    WeakUndefined,
    /// A tentative definition (a "common" symbol) sized but not yet placed.
    Common,
}

impl SymbolStrength {
    /// Whether this strength names a definition rather than a reference.
    pub fn is_definition(self) -> bool {
        matches!(
            self,
            SymbolStrength::Strong | SymbolStrength::Weak | SymbolStrength::Common
        )
    }
}

/// Whether a symbol participates in resolution beyond its own object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SymbolVisibility {
    /// Visible to other objects.
    Global,
    /// Visible only within this object; cannot satisfy another object's
    /// reference, and may share a name with locals elsewhere.
    Local,
    /// Global in the object but not exported from the final image.
    PrivateExternal,
}

/// One symbol as it appears in an object's symbol table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputSymbol {
    pub id: SymbolId,
    pub name: String,
    pub strength: SymbolStrength,
    pub visibility: SymbolVisibility,
    /// Section this symbol is defined in; `None` for undefined and common
    /// symbols, which have no home section yet.
    pub section: Option<SectionId>,
    /// Value as recorded in the file — an address within `section` for a
    /// definition, a size for a common symbol, and zero for a reference.
    pub value: u64,
    /// `N_NO_DEAD_STRIP`: this definition is a root, whatever refers to it.
    #[serde(default)]
    pub no_dead_strip: bool,
}

impl InputSymbol {
    /// Whether this symbol can satisfy another object's reference.
    pub fn can_satisfy_reference(&self) -> bool {
        self.strength.is_definition() && self.visibility != SymbolVisibility::Local
    }
}

/// What a relocation points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RelocationTarget {
    /// A named symbol, resolved globally.
    Symbol(SymbolId),
    /// A section in this same object, resolved locally.
    Section(SectionId),
}

/// One relocation, in the form the relocation engine will consume.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputRelocation {
    pub id: RelocationId,
    /// Section whose bytes this relocation patches.
    pub section: SectionId,
    /// Byte offset of the patched field within that section.
    pub offset: u64,
    pub target: RelocationTarget,
    pub kind: Arm64RelocationKind,
    pub length: RelocationLength,
    /// Whether the patched field is program-counter relative.
    pub pc_relative: bool,
    /// Addend read from the instruction stream, where the encoding carries one.
    pub addend: i64,
}

/// Broad category of a section's contents, which drives output placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SectionKind {
    /// Executable instructions.
    Code,
    /// Read-only initialised data.
    ReadOnlyData,
    /// Writable initialised data.
    Data,
    /// Zero-filled data, occupying address space but no file bytes.
    Bss,
    /// Thread-local data or its descriptors.
    ThreadLocal,
    /// Unwind tables — `__eh_frame` and `__compact_unwind`.
    Unwind,
    /// DWARF and related debug data.
    Debug,
    /// Anything else; carried through without interpretation.
    Other,
}

/// One section of an object file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputSection {
    pub id: SectionId,
    /// Mach-O segment name, e.g. `__TEXT`.
    pub segment: String,
    /// Mach-O section name, e.g. `__text`.
    pub name: String,
    pub kind: SectionKind,
    pub size: u64,
    /// Address of this section *within its own object file*.
    ///
    /// Relocatable objects are not laid out at a final address, but their
    /// sections still carry addresses in a per-object coordinate space, and
    /// symbol values are absolute in that space. Recovering a symbol's offset
    /// within its section needs this — assuming zero is right only for the
    /// first section, and silently misplaces every symbol after it.
    pub vm_address: u64,
    /// Required alignment in bytes; always a power of two.
    pub alignment: u64,
    /// Offset of this section's bytes within the object file.
    ///
    /// `None` for zero-filled sections, which occupy no file bytes. The bytes
    /// are addressed by offset rather than copied so that [`ParsedObject`]
    /// stays small enough to cache — a real link reads 65–200 MB of input, and
    /// holding all of it in the parsed form would defeat the purpose.
    pub file_offset: Option<u64>,
    /// `S_ATTR_NO_DEAD_STRIP`: this section must be kept whole even when
    /// nothing reaches it.
    ///
    /// Initialiser lists and Objective-C metadata carry it. They are reached by
    /// the runtime rather than by any relocation, so a reachability analysis
    /// cannot see the edge and would remove them.
    #[serde(default)]
    pub no_dead_strip: bool,
}

impl InputSection {
    /// Fully qualified `SEGMENT,section` name, as the toolchain spells it.
    pub fn qualified_name(&self) -> String {
        format!("{},{}", self.segment, self.name)
    }

    /// Whether this section occupies file bytes.
    pub fn has_file_bytes(&self) -> bool {
        self.file_offset.is_some()
    }
}

/// Facts about the object as a whole.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObjectMetadata {
    /// Path the object was read from. For an archive member this is the
    /// archive's path; `member` names the entry within it.
    pub path: PathBuf,
    /// Archive member name, when this object came from a `.a` or `.rlib`.
    pub member: Option<String>,
    pub file_size: u64,
    /// Whether the object carries any debug sections.
    pub has_debug_info: bool,
    /// Whether the object carries unwind tables.
    pub has_unwind_info: bool,
}

/// A fully parsed object file.
///
/// Free of self-referential structure: everything is addressed by stable ID, so
/// this serialises directly into M4's cache and stays valid without the file it
/// was parsed from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParsedObject {
    pub id: ObjectId,
    pub architecture: Architecture,
    pub sections: Vec<InputSection>,
    pub symbols: Vec<InputSymbol>,
    pub relocations: Vec<InputRelocation>,
    pub metadata: ObjectMetadata,
    /// `MH_SUBSECTIONS_VIA_SYMBOLS`: the compiler asserts that nothing in this
    /// object refers to anything except through a symbol.
    ///
    /// That assertion is what makes it legal to cut a section at its symbol
    /// boundaries and discard the pieces nothing reaches. Without it the
    /// object's sections have to be kept whole, because a reference computed
    /// as "the start of my section plus 40 bytes" would follow the bytes it
    /// meant only by accident.
    #[serde(default)]
    pub subsections_via_symbols: bool,
}

impl ParsedObject {
    pub fn section(&self, id: SectionId) -> Option<&InputSection> {
        self.sections.get(id.0 as usize)
    }

    pub fn symbol(&self, id: SymbolId) -> Option<&InputSymbol> {
        self.symbols.get(id.0 as usize)
    }

    /// Symbols this object defines that others can link against.
    pub fn exported_symbols(&self) -> impl Iterator<Item = &InputSymbol> {
        self.symbols.iter().filter(|s| s.can_satisfy_reference())
    }

    /// Symbols this object needs another object to define.
    pub fn undefined_symbols(&self) -> impl Iterator<Item = &InputSymbol> {
        self.symbols.iter().filter(|s| {
            matches!(
                s.strength,
                SymbolStrength::Undefined | SymbolStrength::WeakUndefined
            )
        })
    }

    /// Relocations that patch bytes in `section`.
    /// This section's relocations, as a slice of the object's list.
    ///
    /// # The invariant this rests on
    ///
    /// `relocations` is grouped by section, in ascending section order,
    /// because [`crate::parse`] walks the sections in order and appends. That
    /// is what makes this a binary search instead of a scan.
    ///
    /// The difference is not small. Every caller that asks per section was
    /// walking *all* of the object's relocations to read one section's, which
    /// is quadratic in an object's size and invisible on a small one: finding
    /// where each function's FDE landed came to 95 ms of a 110 ms unwind stage
    /// on a debug rust-analyzer link (finding 160).
    ///
    /// A `ParsedObject` assembled some other way could break the grouping, and
    /// the failure would be silent — a subset of a section's relocations, so
    /// bytes left unpatched. Debug builds check the answer against the scan
    /// this replaces, which every test run exercises on real objects.
    pub fn relocations_for(&self, section: SectionId) -> &[InputRelocation] {
        let start = self
            .relocations
            .partition_point(|r| r.section.0 < section.0);
        let len = self.relocations[start..].partition_point(|r| r.section.0 == section.0);
        let found = &self.relocations[start..start + len];
        debug_assert_eq!(
            found.len(),
            self.relocations
                .iter()
                .filter(|r| r.section == section)
                .count(),
            "relocations are not grouped by section, so this returned a subset of them"
        );
        found
    }

    /// Total size of all sections that occupy file bytes.
    pub fn code_and_data_size(&self) -> u64 {
        self.sections
            .iter()
            .filter(|s| s.has_file_bytes())
            .map(|s| s.size)
            .sum()
    }
}

/// Classify a section from its Mach-O segment and section names.
///
/// Mach-O encodes a section's role partly in flags and partly by convention in
/// its name. The names are what the toolchain and every other linker key on, so
/// they are what we key on too.
pub fn classify_section(segment: &str, name: &str) -> SectionKind {
    match (segment, name) {
        ("__TEXT", "__text") => SectionKind::Code,
        ("__TEXT", "__eh_frame") | (_, "__compact_unwind") => SectionKind::Unwind,
        ("__TEXT", "__gcc_except_tab") => SectionKind::ReadOnlyData,
        ("__DATA", "__bss") | ("__DATA", "__common") => SectionKind::Bss,
        // Thread-local storage spans several section names, all prefixed
        // `__thread_`, plus the descriptor section `__tlv`.
        (_, n) if n.starts_with("__thread_") || n == "__tlv" => SectionKind::ThreadLocal,
        (_, n) if n.starts_with("__debug") || n.starts_with("__apple_") => SectionKind::Debug,
        ("__DWARF", _) => SectionKind::Debug,
        ("__TEXT", _) => SectionKind::ReadOnlyData,
        ("__DATA_CONST", _) => SectionKind::ReadOnlyData,
        ("__DATA", _) => SectionKind::Data,
        _ => SectionKind::Other,
    }
}

/// Read and parse an object file from disk.
pub fn parse_object_file(path: &Path, id: ObjectId) -> Result<ParsedObject, ParseError> {
    let data = std::fs::read(path).map_err(|source| ParseError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_object(&data, path, None, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_code_and_data_sections() {
        assert_eq!(classify_section("__TEXT", "__text"), SectionKind::Code);
        assert_eq!(classify_section("__DATA", "__data"), SectionKind::Data);
        assert_eq!(classify_section("__DATA", "__bss"), SectionKind::Bss);
        assert_eq!(
            classify_section("__DATA_CONST", "__const"),
            SectionKind::ReadOnlyData
        );
    }

    #[test]
    fn classifies_unwind_sections() {
        // These must not fall into ReadOnlyData: unwind tables need their
        // addresses fixed up when code moves, so they are tracked separately.
        assert_eq!(
            classify_section("__TEXT", "__eh_frame"),
            SectionKind::Unwind
        );
        assert_eq!(
            classify_section("__LD", "__compact_unwind"),
            SectionKind::Unwind
        );
    }

    #[test]
    fn classifies_thread_local_sections() {
        for name in ["__thread_data", "__thread_bss", "__thread_vars", "__tlv"] {
            assert_eq!(
                classify_section("__DATA", name),
                SectionKind::ThreadLocal,
                "{name} should be thread-local"
            );
        }
    }

    #[test]
    fn classifies_debug_sections() {
        for name in ["__debug_info", "__debug_str", "__apple_names"] {
            assert_eq!(classify_section("__DWARF", name), SectionKind::Debug);
        }
        assert_eq!(
            classify_section("__DWARF", "__anything"),
            SectionKind::Debug
        );
    }

    #[test]
    fn text_sections_other_than_code_are_read_only_data() {
        assert_eq!(
            classify_section("__TEXT", "__const"),
            SectionKind::ReadOnlyData
        );
        assert_eq!(
            classify_section("__TEXT", "__cstring"),
            SectionKind::ReadOnlyData
        );
    }

    #[test]
    fn unknown_segments_are_other_rather_than_guessed() {
        assert_eq!(classify_section("__CUSTOM", "__mine"), SectionKind::Other);
    }

    #[test]
    fn qualified_name_uses_toolchain_spelling() {
        let section = InputSection {
            id: SectionId(0),
            vm_address: 0,
            segment: "__TEXT".into(),
            name: "__text".into(),
            kind: SectionKind::Code,
            size: 100,
            alignment: 4,
            file_offset: Some(0),
            no_dead_strip: false,
        };
        assert_eq!(section.qualified_name(), "__TEXT,__text");
        assert!(section.has_file_bytes());
    }

    #[test]
    fn zero_filled_sections_have_no_file_bytes() {
        let bss = InputSection {
            id: SectionId(1),
            vm_address: 0,
            segment: "__DATA".into(),
            name: "__bss".into(),
            kind: SectionKind::Bss,
            size: 4096,
            alignment: 8,
            file_offset: None,
            no_dead_strip: false,
        };
        assert!(!bss.has_file_bytes());
    }

    fn symbol(strength: SymbolStrength, visibility: SymbolVisibility) -> InputSymbol {
        InputSymbol {
            id: SymbolId(0),
            name: "_sym".into(),
            strength,
            visibility,
            section: None,
            value: 0,
            no_dead_strip: false,
        }
    }

    #[test]
    fn only_non_local_definitions_can_satisfy_a_reference() {
        use SymbolStrength::*;
        use SymbolVisibility::*;

        assert!(symbol(Strong, Global).can_satisfy_reference());
        assert!(symbol(Weak, Global).can_satisfy_reference());
        assert!(symbol(Common, Global).can_satisfy_reference());

        // A local definition cannot satisfy another object's reference, even
        // though it is a definition — this is what keeps same-named locals in
        // different objects from colliding.
        assert!(!symbol(Strong, Local).can_satisfy_reference());
        // References are not definitions.
        assert!(!symbol(Undefined, Global).can_satisfy_reference());
        assert!(!symbol(WeakUndefined, Global).can_satisfy_reference());
    }

    #[test]
    fn definition_strengths_are_distinguished_from_references() {
        use SymbolStrength::*;
        assert!(Strong.is_definition());
        assert!(Weak.is_definition());
        assert!(Common.is_definition());
        assert!(!Undefined.is_definition());
        assert!(!WeakUndefined.is_definition());
    }

    #[test]
    fn parsed_object_round_trips_through_serde() {
        // The whole point of the owned representation: it must survive being
        // cached and read back without the file it came from.
        let object = ParsedObject {
            id: ObjectId(7),
            architecture: Architecture::Arm64,
            sections: vec![InputSection {
                id: SectionId(0),
                vm_address: 0,
                segment: "__TEXT".into(),
                name: "__text".into(),
                kind: SectionKind::Code,
                size: 64,
                alignment: 4,
                file_offset: Some(512),
                no_dead_strip: false,
            }],
            symbols: vec![InputSymbol {
                id: SymbolId(0),
                name: "_main".into(),
                strength: SymbolStrength::Strong,
                visibility: SymbolVisibility::Global,
                section: Some(SectionId(0)),
                value: 0,
                no_dead_strip: false,
            }],
            relocations: vec![InputRelocation {
                id: RelocationId(0),
                section: SectionId(0),
                offset: 8,
                target: RelocationTarget::Symbol(SymbolId(0)),
                kind: Arm64RelocationKind::Branch26,
                length: RelocationLength::Word,
                pc_relative: true,
                addend: 0,
            }],
            subsections_via_symbols: true,
            metadata: ObjectMetadata {
                path: PathBuf::from("/tmp/a.o"),
                member: None,
                file_size: 1024,
                has_debug_info: false,
                has_unwind_info: false,
            },
        };

        let json = serde_json::to_string(&object).expect("serializes");
        let back: ParsedObject = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(object, back);
    }

    #[test]
    fn lookups_by_id_return_the_indexed_element() {
        let json = r#"{"id":0,"architecture":"Arm64","sections":[],"symbols":[],
            "relocations":[],"metadata":{"path":"/x","member":null,"file_size":0,
            "has_debug_info":false,"has_unwind_info":false}}"#;
        let object: ParsedObject = serde_json::from_str(json).expect("deserializes");
        assert!(object.section(SectionId(0)).is_none());
        assert!(object.symbol(SymbolId(0)).is_none());
    }
}
