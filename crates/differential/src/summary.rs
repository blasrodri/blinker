//! A normalized, comparable description of a linked Mach-O image.
//!
//! # Why not compare bytes
//!
//! Two correct linkers do not produce identical files, and neither do two runs
//! of the *same* linker. `LC_UUID` is derived from content that includes a
//! build timestamp; the code signature covers the file's own hash; string
//! table ordering is unconstrained. A byte comparison would report a
//! difference on every single link and tell us nothing.
//!
//! So the comparison happens over *facts*: which segments exist, where they
//! are, which symbols are exported, which libraries are loaded. These are the
//! properties dyld actually acts on, and they are the ones a linker bug would
//! get wrong.
//!
//! # What is deliberately excluded
//!
//! Anything that legitimately varies between two correct links is left out
//! rather than recorded and then ignored — a field that is present but never
//! compared invites someone to start comparing it:
//!
//! - `LC_UUID`'s bytes (varies per link by construction)
//! - the code signature blob
//! - the string table's internal layout
//! - `LC_SOURCE_VERSION` and build-tool version numbers

use object::macho;
use object::read::macho::{LoadCommandVariant, MachHeader, MachOFile64};
use object::{Endianness, LittleEndian, Object, ObjectSymbol};
use std::collections::BTreeSet;

#[derive(Debug)]
pub enum SummaryError {
    /// The file could not be parsed as a 64-bit Mach-O image.
    Malformed(String),
    Io(std::io::Error),
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummaryError::Malformed(d) => write!(f, "not a readable Mach-O image: {d}"),
            SummaryError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SummaryError {}

impl From<std::io::Error> for SummaryError {
    fn from(e: std::io::Error) -> Self {
        SummaryError::Io(e)
    }
}

/// One segment's placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentSummary {
    pub name: String,
    pub vm_address: u64,
    pub vm_size: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub max_protection: u32,
    pub init_protection: u32,
    pub section_count: u32,
}

/// One section's placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionSummary {
    pub segment: String,
    pub name: String,
    pub address: u64,
    pub size: u64,
    pub file_offset: u32,
    pub alignment: u32,
    pub flags: u32,
}

impl SectionSummary {
    pub fn qualified_name(&self) -> String {
        format!("{},{}", self.segment, self.name)
    }
}

/// Everything about a linked image that two linkers should agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSummary {
    pub cpu_type: u32,
    pub cpu_subtype: u32,
    pub file_type: u32,
    pub flags: u32,

    /// Load command names in file order. Order matters: dyld walks them
    /// sequentially, and some commands must precede others.
    pub load_commands: Vec<String>,

    pub segments: Vec<SegmentSummary>,
    pub sections: Vec<SectionSummary>,

    /// `LC_LOAD_DYLIB` paths, in order.
    pub dylibs: Vec<String>,
    /// The `LC_LOAD_DYLINKER` path.
    pub dynamic_linker: Option<String>,
    /// `LC_MAIN`'s entry offset, if present.
    pub entry_offset: Option<u64>,

    pub exported_symbols: BTreeSet<String>,
    pub undefined_symbols: BTreeSet<String>,
    /// Locals are counted rather than listed: their names are compiler-chosen
    /// and differ freely between toolchains, but a wildly different *count*
    /// still signals a problem.
    pub local_symbol_count: usize,

    /// Total file size. Informational — never a correctness property.
    pub file_size: u64,
}

/// Read a linked image from a file.
pub fn summarize_file(path: &std::path::Path) -> Result<ImageSummary, SummaryError> {
    let bytes = std::fs::read(path)?;
    summarize(&bytes)
}

/// Read a linked image from bytes.
pub fn summarize(data: &[u8]) -> Result<ImageSummary, SummaryError> {
    let file = MachOFile64::<LittleEndian, _>::parse(data)
        .map_err(|e| SummaryError::Malformed(e.to_string()))?;
    let endian = LittleEndian;
    let header = file.macho_header();

    let mut summary = ImageSummary {
        cpu_type: header.cputype(endian),
        cpu_subtype: header.cpusubtype(endian),
        file_type: header.filetype(endian),
        flags: header.flags(endian),
        load_commands: Vec::new(),
        segments: Vec::new(),
        sections: Vec::new(),
        dylibs: Vec::new(),
        dynamic_linker: None,
        entry_offset: None,
        exported_symbols: BTreeSet::new(),
        undefined_symbols: BTreeSet::new(),
        local_symbol_count: 0,
        file_size: data.len() as u64,
    };

    let mut commands = header
        .load_commands(endian, data, 0)
        .map_err(|e| SummaryError::Malformed(e.to_string()))?;

    while let Some(command) = commands
        .next()
        .map_err(|e| SummaryError::Malformed(e.to_string()))?
    {
        summary.load_commands.push(command_name(command.cmd()));

        let variant = command
            .variant()
            .map_err(|e| SummaryError::Malformed(e.to_string()))?;

        match variant {
            LoadCommandVariant::Segment64(segment, section_data) => {
                summary.segments.push(SegmentSummary {
                    name: fixed_name(&segment.segname),
                    vm_address: segment.vmaddr.get(endian),
                    vm_size: segment.vmsize.get(endian),
                    file_offset: segment.fileoff.get(endian),
                    file_size: segment.filesize.get(endian),
                    max_protection: segment.maxprot.get(endian),
                    init_protection: segment.initprot.get(endian),
                    section_count: segment.nsects.get(endian),
                });

                // Sections live in the load command's trailing bytes rather
                // than in a command of their own.
                let count = segment.nsects.get(endian) as usize;
                let size = std::mem::size_of::<macho::Section64<LittleEndian>>();
                for index in 0..count {
                    let start = index * size;
                    let Some(chunk) = section_data.get(start..start + size) else {
                        return Err(SummaryError::Malformed(format!(
                            "segment {} declares {count} sections but the command is too short",
                            fixed_name(&segment.segname)
                        )));
                    };
                    let section: &macho::Section64<LittleEndian> = object::pod::from_bytes(chunk)
                        .map_err(|_| SummaryError::Malformed("unaligned section_64".into()))?
                        .0;
                    summary.sections.push(SectionSummary {
                        segment: fixed_name(&section.segname),
                        name: fixed_name(&section.sectname),
                        address: section.addr.get(endian),
                        size: section.size.get(endian),
                        file_offset: section.offset.get(endian),
                        alignment: section.align.get(endian),
                        flags: section.flags.get(endian),
                    });
                }
            }
            LoadCommandVariant::Dylib(dylib) => {
                if let Ok(name) = command.string(endian, dylib.dylib.name) {
                    summary
                        .dylibs
                        .push(String::from_utf8_lossy(name).into_owned());
                }
            }
            LoadCommandVariant::LoadDylinker(dylinker) => {
                if let Ok(name) = command.string(endian, dylinker.name) {
                    summary.dynamic_linker = Some(String::from_utf8_lossy(name).into_owned());
                }
            }
            LoadCommandVariant::EntryPoint(entry) => {
                summary.entry_offset = Some(entry.entryoff.get(endian));
            }
            _ => {}
        }
    }

    for symbol in file.symbols() {
        let Ok(name) = symbol.name() else { continue };
        if name.is_empty() {
            continue;
        }
        if symbol.is_undefined() {
            summary.undefined_symbols.insert(name.to_string());
        } else if symbol.is_global() {
            summary.exported_symbols.insert(name.to_string());
        } else {
            summary.local_symbol_count += 1;
        }
    }

    Ok(summary)
}

/// A segment or section name, trimmed of its NUL padding.
///
/// These fields are fixed 16-byte arrays that are *not* required to be
/// NUL-terminated — a name of exactly 16 characters fills the array. Using
/// `CStr` here would read past the end.
fn fixed_name(raw: &[u8; 16]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// The `LC_*` name for a load command number.
///
/// Unknown commands render as their hex value rather than being dropped: a
/// command we do not recognise still has to appear in the comparison, or a
/// linker emitting something unexpected would look identical to one that
/// emitted nothing.
fn command_name(cmd: u32) -> String {
    let name = match cmd {
        macho::LC_SEGMENT => "LC_SEGMENT",
        macho::LC_SYMTAB => "LC_SYMTAB",
        macho::LC_SYMSEG => "LC_SYMSEG",
        macho::LC_THREAD => "LC_THREAD",
        macho::LC_UNIXTHREAD => "LC_UNIXTHREAD",
        macho::LC_DYSYMTAB => "LC_DYSYMTAB",
        macho::LC_LOAD_DYLIB => "LC_LOAD_DYLIB",
        macho::LC_ID_DYLIB => "LC_ID_DYLIB",
        macho::LC_LOAD_DYLINKER => "LC_LOAD_DYLINKER",
        macho::LC_ID_DYLINKER => "LC_ID_DYLINKER",
        macho::LC_PREBOUND_DYLIB => "LC_PREBOUND_DYLIB",
        macho::LC_ROUTINES => "LC_ROUTINES",
        macho::LC_SUB_FRAMEWORK => "LC_SUB_FRAMEWORK",
        macho::LC_SUB_UMBRELLA => "LC_SUB_UMBRELLA",
        macho::LC_SUB_CLIENT => "LC_SUB_CLIENT",
        macho::LC_SUB_LIBRARY => "LC_SUB_LIBRARY",
        macho::LC_TWOLEVEL_HINTS => "LC_TWOLEVEL_HINTS",
        macho::LC_PREBIND_CKSUM => "LC_PREBIND_CKSUM",
        macho::LC_LOAD_WEAK_DYLIB => "LC_LOAD_WEAK_DYLIB",
        macho::LC_SEGMENT_64 => "LC_SEGMENT_64",
        macho::LC_ROUTINES_64 => "LC_ROUTINES_64",
        macho::LC_UUID => "LC_UUID",
        macho::LC_RPATH => "LC_RPATH",
        macho::LC_CODE_SIGNATURE => "LC_CODE_SIGNATURE",
        macho::LC_SEGMENT_SPLIT_INFO => "LC_SEGMENT_SPLIT_INFO",
        macho::LC_REEXPORT_DYLIB => "LC_REEXPORT_DYLIB",
        macho::LC_LAZY_LOAD_DYLIB => "LC_LAZY_LOAD_DYLIB",
        macho::LC_ENCRYPTION_INFO => "LC_ENCRYPTION_INFO",
        macho::LC_DYLD_INFO => "LC_DYLD_INFO",
        macho::LC_DYLD_INFO_ONLY => "LC_DYLD_INFO_ONLY",
        macho::LC_LOAD_UPWARD_DYLIB => "LC_LOAD_UPWARD_DYLIB",
        macho::LC_VERSION_MIN_MACOSX => "LC_VERSION_MIN_MACOSX",
        macho::LC_VERSION_MIN_IPHONEOS => "LC_VERSION_MIN_IPHONEOS",
        macho::LC_FUNCTION_STARTS => "LC_FUNCTION_STARTS",
        macho::LC_DYLD_ENVIRONMENT => "LC_DYLD_ENVIRONMENT",
        macho::LC_MAIN => "LC_MAIN",
        macho::LC_DATA_IN_CODE => "LC_DATA_IN_CODE",
        macho::LC_SOURCE_VERSION => "LC_SOURCE_VERSION",
        macho::LC_DYLIB_CODE_SIGN_DRS => "LC_DYLIB_CODE_SIGN_DRS",
        macho::LC_ENCRYPTION_INFO_64 => "LC_ENCRYPTION_INFO_64",
        macho::LC_LINKER_OPTION => "LC_LINKER_OPTION",
        macho::LC_LINKER_OPTIMIZATION_HINT => "LC_LINKER_OPTIMIZATION_HINT",
        macho::LC_VERSION_MIN_TVOS => "LC_VERSION_MIN_TVOS",
        macho::LC_VERSION_MIN_WATCHOS => "LC_VERSION_MIN_WATCHOS",
        macho::LC_NOTE => "LC_NOTE",
        macho::LC_BUILD_VERSION => "LC_BUILD_VERSION",
        macho::LC_DYLD_EXPORTS_TRIE => "LC_DYLD_EXPORTS_TRIE",
        macho::LC_DYLD_CHAINED_FIXUPS => "LC_DYLD_CHAINED_FIXUPS",
        _ => return format!("LC_UNKNOWN({cmd:#x})"),
    };
    name.to_string()
}

/// Whether `Endianness` is needed at all — kept to document that these images
/// are little-endian by construction on this target.
#[allow(dead_code)]
const _ENDIAN: Endianness = Endianness::Little;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sixteen_character_name_is_not_truncated() {
        // These fields are not NUL-terminated when full. Reading them as a
        // C string would run past the array.
        let raw = *b"__sixteen_chars_";
        assert_eq!(fixed_name(&raw), "__sixteen_chars_");
    }

    #[test]
    fn a_padded_name_drops_its_nuls() {
        let raw = *b"__TEXT\0\0\0\0\0\0\0\0\0\0";
        assert_eq!(fixed_name(&raw), "__TEXT");
    }

    #[test]
    fn unknown_load_commands_are_named_not_dropped() {
        // A linker emitting a command we do not know about must not look the
        // same as one emitting nothing.
        assert_eq!(command_name(0x7fff_0001), "LC_UNKNOWN(0x7fff0001)");
        assert_eq!(command_name(macho::LC_MAIN), "LC_MAIN");
    }

    #[test]
    fn garbage_is_rejected_rather_than_summarized_as_empty() {
        let err = summarize(&[0u8; 64]).unwrap_err();
        assert!(matches!(err, SummaryError::Malformed(_)));
    }
}
