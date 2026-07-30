//! The conversion boundary: `object`'s borrowed view in, our owned form out.
//!
//! Everything that could silently lose fidelity happens here, so the rules are
//! concentrated in one file:
//!
//! - Raw Mach-O fields only. `object`'s portable `RelocationKind` reports most
//!   ARM64 relocations as `Unknown`; we read `r_type` / `r_pcrel` / `r_length`
//!   and map them ourselves.
//! - Refuse rather than default. An unrecognised relocation type, a
//!   non-ARM64 architecture, or a symbol we cannot classify becomes a
//!   structured error. Spec §5: never silently ignore unknown input.

use std::path::{Path, PathBuf};

use object::read::macho::MachOFile64;
use object::LittleEndian;
use object::{
    Object, ObjectSection, ObjectSymbol, RelocationFlags, RelocationTarget as ObjectRelocTarget,
    SectionIndex, SymbolIndex, SymbolSection,
};

use crate::{
    classify_section, Architecture, InputRelocation, InputSection, InputSymbol, ObjectId,
    ObjectMetadata, ParsedObject, RelocationError, RelocationId, RelocationLength,
    RelocationTarget, SectionId, SectionKind, SymbolId, SymbolStrength, SymbolVisibility,
};

#[derive(Debug)]
pub enum ParseError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not a Mach-O object we can read.
    Malformed { path: PathBuf, detail: String },
    /// A valid Mach-O object, but not for the architecture we support.
    UnsupportedArchitecture { path: PathBuf, found: String },
    /// A relocation we will not guess at.
    UnsupportedRelocation {
        path: PathBuf,
        section: String,
        offset: u64,
        source: RelocationError,
    },
    /// A relocation whose target we cannot resolve to a symbol or section.
    UnresolvableRelocationTarget {
        path: PathBuf,
        section: String,
        offset: u64,
    },
    /// More sections, symbols, or relocations than a `u32` ID can address.
    ///
    /// Real objects are nowhere near this; hitting it means the input is
    /// malformed or hostile.
    TooManyElements {
        path: PathBuf,
        what: &'static str,
        count: usize,
    },
    /// An index naming an element that does not exist.
    ///
    /// Mach-O stores section numbers one-based, so a corrupted `n_sect` field
    /// naming section 200 in a 14-section object is entirely representable.
    /// Accepting it would produce a `ParsedObject` whose IDs dangle — a parse
    /// that "succeeded" but cannot be used, which is worse than a clean
    /// failure. Found by the mutation-robustness tests.
    IndexOutOfRange {
        path: PathBuf,
        what: &'static str,
        index: usize,
        available: usize,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Io { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            ParseError::Malformed { path, detail } => {
                write!(f, "malformed Mach-O object {}: {detail}", path.display())
            }
            ParseError::UnsupportedArchitecture { path, found } => {
                write!(f, "{} is {found}; only arm64 is supported", path.display())
            }
            ParseError::UnsupportedRelocation {
                path,
                section,
                offset,
                source,
            } => write!(
                f,
                "{} in {section}+{offset:#x} of {}",
                source,
                path.display()
            ),
            ParseError::UnresolvableRelocationTarget {
                path,
                section,
                offset,
            } => write!(
                f,
                "relocation at {section}+{offset:#x} of {} has no resolvable target",
                path.display()
            ),
            ParseError::TooManyElements { path, what, count } => write!(
                f,
                "{} has {count} {what}, more than can be addressed",
                path.display()
            ),
            ParseError::IndexOutOfRange {
                path,
                what,
                index,
                available,
            } => write!(
                f,
                "{} references {what} {index}, but only {available} exist",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse an in-memory Mach-O object.
///
/// `path` and `member` are recorded for diagnostics and cache identity; they do
/// not affect parsing.
pub fn parse_object(
    data: &[u8],
    path: &Path,
    member: Option<&str>,
    id: ObjectId,
) -> Result<ParsedObject, ParseError> {
    let file = MachOFile64::<LittleEndian, _>::parse(data).map_err(|e| ParseError::Malformed {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;

    let architecture = match file.architecture() {
        object::Architecture::Aarch64 => Architecture::Arm64,
        other => {
            return Err(ParseError::UnsupportedArchitecture {
                path: path.to_path_buf(),
                found: format!("{other:?}"),
            })
        }
    };

    let sections = parse_sections(&file, path)?;
    let symbols = parse_symbols(&file, path, sections.len())?;
    let relocations = parse_relocations(&file, path, &sections, symbols.len())?;

    let has_debug_info = sections.iter().any(|s| s.kind == SectionKind::Debug);
    let has_unwind_info = sections.iter().any(|s| s.kind == SectionKind::Unwind);

    Ok(ParsedObject {
        id,
        architecture,
        sections,
        symbols,
        relocations,
        metadata: ObjectMetadata {
            path: path.to_path_buf(),
            member: member.map(str::to_string),
            file_size: data.len() as u64,
            has_debug_info,
            has_unwind_info,
        },
    })
}

/// Guard the `usize` → `u32` narrowing that stable IDs depend on.
fn check_count(count: usize, what: &'static str, path: &Path) -> Result<(), ParseError> {
    if count > u32::MAX as usize {
        return Err(ParseError::TooManyElements {
            path: path.to_path_buf(),
            what,
            count,
        });
    }
    Ok(())
}

/// Validate a zero-based index against the table it refers into.
///
/// Every ID stored in a `ParsedObject` passes through here. Downstream code
/// treats IDs as indices, so an unchecked one turns a successful parse into a
/// structure that panics or silently misbehaves on first use.
fn check_index(
    index: usize,
    available: usize,
    what: &'static str,
    path: &Path,
) -> Result<(), ParseError> {
    if index >= available {
        return Err(ParseError::IndexOutOfRange {
            path: path.to_path_buf(),
            what,
            index,
            available,
        });
    }
    Ok(())
}

fn parse_sections<'d>(
    file: &MachOFile64<'d, LittleEndian, &'d [u8]>,
    path: &Path,
) -> Result<Vec<InputSection>, ParseError> {
    let raw: Vec<_> = file.sections().collect();
    check_count(raw.len(), "sections", path)?;

    let mut sections = Vec::with_capacity(raw.len());
    for (index, section) in raw.iter().enumerate() {
        // A section with an unreadable name is malformed rather than nameless:
        // classification and diagnostics both key on it.
        let name = section.name().map_err(|e| ParseError::Malformed {
            path: path.to_path_buf(),
            detail: format!("section {index} has an unreadable name: {e}"),
        })?;
        let segment = section
            .segment_name()
            .map_err(|e| ParseError::Malformed {
                path: path.to_path_buf(),
                detail: format!("section {index} has an unreadable segment name: {e}"),
            })?
            .unwrap_or("");

        sections.push(InputSection {
            id: SectionId(index as u32),
            segment: segment.to_string(),
            name: name.to_string(),
            kind: classify_section(segment, name),
            size: section.size(),
            // `object` reports alignment as a byte count already.
            alignment: section.align().max(1),
            file_offset: section.file_range().map(|(offset, _)| offset),
        });
    }
    Ok(sections)
}

fn parse_symbols<'d>(
    file: &MachOFile64<'d, LittleEndian, &'d [u8]>,
    path: &Path,
    section_count: usize,
) -> Result<Vec<InputSymbol>, ParseError> {
    let raw: Vec<_> = file.symbols().collect();
    check_count(raw.len(), "symbols", path)?;

    let mut symbols = Vec::with_capacity(raw.len());
    for (index, symbol) in raw.iter().enumerate() {
        let name = symbol.name().map_err(|e| ParseError::Malformed {
            path: path.to_path_buf(),
            detail: format!("symbol {index} has an unreadable name: {e}"),
        })?;

        // Order matters: a symbol can be both undefined and weak, and the
        // undefined case must be decided first or a weak reference would be
        // misread as a weak definition.
        let strength = if symbol.is_undefined() {
            if symbol.is_weak() {
                SymbolStrength::WeakUndefined
            } else {
                SymbolStrength::Undefined
            }
        } else if symbol.is_common() {
            SymbolStrength::Common
        } else if symbol.is_weak() {
            SymbolStrength::Weak
        } else {
            SymbolStrength::Strong
        };

        let visibility = if !symbol.is_global() {
            SymbolVisibility::Local
        } else if symbol.scope() == object::SymbolScope::Linkage {
            // Global within the link unit, but not exported from the image.
            SymbolVisibility::PrivateExternal
        } else {
            SymbolVisibility::Global
        };

        let section = match symbol.section() {
            SymbolSection::Section(SectionIndex(i)) => {
                // Mach-O numbers sections from 1; our IDs are indices. A
                // corrupted `n_sect` can name a section that does not exist,
                // so the result is range-checked rather than trusted.
                match i.checked_sub(1) {
                    Some(zero_based) => {
                        check_index(zero_based, section_count, "section", path)?;
                        Some(SectionId(zero_based as u32))
                    }
                    None => None,
                }
            }
            _ => None,
        };

        symbols.push(InputSymbol {
            id: SymbolId(index as u32),
            name: name.to_string(),
            strength,
            visibility,
            section,
            value: symbol.address(),
        });
    }
    Ok(symbols)
}

fn parse_relocations<'d>(
    file: &MachOFile64<'d, LittleEndian, &'d [u8]>,
    path: &Path,
    sections: &[InputSection],
    symbol_count: usize,
) -> Result<Vec<InputRelocation>, ParseError> {
    let mut relocations = Vec::new();

    for (section_index, section) in file.sections().enumerate() {
        let section_id = SectionId(section_index as u32);
        let section_name = sections
            .get(section_index)
            .map(InputSection::qualified_name)
            .unwrap_or_else(|| format!("section {section_index}"));

        for (offset, reloc) in section.relocations() {
            // Raw Mach-O fields only — the portable enum reports most ARM64
            // relocations as `Unknown`, which we must never accept.
            let RelocationFlags::MachO {
                r_type,
                r_pcrel,
                r_length,
            } = reloc.flags()
            else {
                return Err(ParseError::UnsupportedRelocation {
                    path: path.to_path_buf(),
                    section: section_name,
                    offset,
                    source: RelocationError::UnsupportedType(0),
                });
            };

            let kind = crate::Arm64RelocationKind::from_r_type(r_type).map_err(|source| {
                ParseError::UnsupportedRelocation {
                    path: path.to_path_buf(),
                    section: section_name.clone(),
                    offset,
                    source,
                }
            })?;
            let length = RelocationLength::from_r_length(r_length).map_err(|source| {
                ParseError::UnsupportedRelocation {
                    path: path.to_path_buf(),
                    section: section_name.clone(),
                    offset,
                    source,
                }
            })?;

            let target = match reloc.target() {
                ObjectRelocTarget::Symbol(SymbolIndex(i)) => {
                    check_index(i, symbol_count, "symbol", path)?;
                    RelocationTarget::Symbol(SymbolId(i as u32))
                }
                ObjectRelocTarget::Section(SectionIndex(i)) => {
                    let zero_based = i.checked_sub(1).ok_or_else(|| {
                        ParseError::UnresolvableRelocationTarget {
                            path: path.to_path_buf(),
                            section: section_name.clone(),
                            offset,
                        }
                    })?;
                    check_index(zero_based, sections.len(), "section", path)?;
                    RelocationTarget::Section(SectionId(zero_based as u32))
                }
                _ => {
                    return Err(ParseError::UnresolvableRelocationTarget {
                        path: path.to_path_buf(),
                        section: section_name.clone(),
                        offset,
                    })
                }
            };

            check_count(relocations.len(), "relocations", path)?;
            relocations.push(InputRelocation {
                id: RelocationId(relocations.len() as u32),
                section: section_id,
                offset,
                target,
                kind,
                length,
                pc_relative: r_pcrel,
                addend: reloc.addend(),
            });
        }
    }
    Ok(relocations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_data_that_is_not_mach_o() {
        let err = parse_object(
            b"not an object file at all",
            Path::new("/x.o"),
            None,
            ObjectId(0),
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn rejects_empty_input() {
        let err = parse_object(&[], Path::new("/x.o"), None, ObjectId(0)).unwrap_err();
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    /// Truncation is the most common corruption, and must be an error rather
    /// than a partial parse.
    #[test]
    fn rejects_a_truncated_header() {
        // 64-bit Mach-O magic, then nothing.
        let err = parse_object(
            &[0xcf, 0xfa, 0xed, 0xfe],
            Path::new("/x.o"),
            None,
            ObjectId(0),
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn error_messages_name_the_file() {
        let err =
            parse_object(b"garbage", Path::new("/some/path.o"), None, ObjectId(0)).unwrap_err();
        assert!(err.to_string().contains("/some/path.o"));
    }

    #[test]
    fn count_guard_accepts_realistic_sizes() {
        assert!(check_count(100_000, "sections", Path::new("/x.o")).is_ok());
    }

    /// Regression: a corrupted one-based section number used to be accepted
    /// and stored as a dangling ID, producing a "successful" parse whose IDs
    /// could not be dereferenced. Found by the mutation-robustness tests.
    #[test]
    fn index_guard_rejects_references_past_the_end_of_a_table() {
        let err = check_index(200, 14, "section", Path::new("/x.o")).unwrap_err();
        match &err {
            ParseError::IndexOutOfRange {
                index, available, ..
            } => {
                assert_eq!(*index, 200);
                assert_eq!(*available, 14);
            }
            other => panic!("expected IndexOutOfRange, got {other:?}"),
        }
        // The message must name both numbers, so the diagnostic is actionable.
        let text = err.to_string();
        assert!(text.contains("200") && text.contains("14"), "{text}");
    }

    #[test]
    fn index_guard_accepts_in_range_references() {
        assert!(check_index(0, 1, "section", Path::new("/x.o")).is_ok());
        assert!(check_index(13, 14, "section", Path::new("/x.o")).is_ok());
    }

    #[test]
    fn index_guard_rejects_any_reference_into_an_empty_table() {
        assert!(check_index(0, 0, "symbol", Path::new("/x.o")).is_err());
    }

    #[test]
    fn count_guard_rejects_counts_beyond_id_range() {
        let err = check_count(u32::MAX as usize + 1, "symbols", Path::new("/x.o")).unwrap_err();
        assert!(matches!(err, ParseError::TooManyElements { .. }));
    }
}
