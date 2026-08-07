//! What can go wrong in a link, and how it is reported.
//!
//! Every variant names the object it came from. A linker error that says only
//! "undefined symbol" makes the user grep; one that says which object
//! referenced it does not.

use blinker_macho::{Arm64RelocationKind, ObjectId, SectionId, SymbolId};
use std::path::PathBuf;

#[derive(Debug)]
pub enum LinkError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Boxed because `ParseError` carries a path and detail string, and an
    /// unboxed variant would make every `LinkError` that large.
    Parse(Box<blinker_macho::ParseError>),
    /// A well-formed input that requires a linker feature blinker does not
    /// implement. The driver may delegate this exact outcome; malformed input
    /// and every other link error remain failures.
    UnsupportedInputFormat {
        path: PathBuf,
        member: Option<String>,
        format: &'static str,
        remedy: &'static str,
    },
    /// An archive could not be read or indexed.
    Archive(Box<blinker_archive::ArchiveError>),
    /// Symbols referenced but never defined.
    UndefinedSymbols {
        names: Vec<String>,
    },
    /// Names defined strongly by more than one input.
    ///
    /// Never a silent pick: the two definitions are different code, and
    /// choosing one produces a program that runs the wrong one with nothing in
    /// the build log to say so.
    DuplicateSymbols {
        names: Vec<String>,
    },
    /// No object contributed anything placeable.
    NothingToLink,
    /// Two contributions to one output section claim the same bytes, or one
    /// runs past the section's end.
    ///
    /// Its own variant because it used to be reported as `NothingToLink`, and
    /// a layout that overlaps is the opposite of one that placed nothing: the
    /// message named the one condition that was not true, on a link with 552
    /// inputs and 47 MB of `__text` (finding 241).
    OverlappingContributions {
        section: usize,
    },
    /// A buffer was carved for an output section the layout does not have.
    ///
    /// The buffers are built from that same layout, so this is the two
    /// disagreeing with each other rather than anything an input can cause.
    MissingOutputSection {
        section: usize,
    },
    /// The entry symbol was not found in any object.
    NoEntryPoint {
        symbol: String,
    },
    /// A contribution named an object that is not in the input set.
    ///
    /// Unreachable unless layout and the object list disagree, which would
    /// mean bytes were about to be copied from the wrong file.
    MissingObject {
        object: ObjectId,
    },
    MissingSection {
        object: ObjectId,
        section: SectionId,
    },
    MissingSymbol {
        symbol: SymbolId,
    },
    /// A section's declared range lies outside the file it came from.
    SectionOutOfBounds {
        object: ObjectId,
        section: SectionId,
    },
    /// A `SUBTRACTOR` with no relocation after it to pair with.
    UnpairedSubtractor {
        object: ObjectId,
        offset: u64,
    },
    /// The unwind table did not fit the space reserved for it.
    UnwindTableTooLarge {
        reserved: u64,
        needed: usize,
    },
    Relocation {
        object: ObjectId,
        kind: Arm64RelocationKind,
        source: Box<blinker_relocations::RelocationError>,
        /// What the relocation pointed at, and where that landed.
        ///
        /// Without these the message named only an object number and an
        /// encoding rule — "value 107 is not 8-byte aligned" — which says a
        /// field could not be written but not what was being referred to, so
        /// there is nothing to look up and nothing to act on. The symbol name
        /// is what turns it into a report about a program.
        symbol: Option<String>,
        target: u64,
    },
    Emit(blinker_output::ImageError),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Read { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            LinkError::Write { path, source } => {
                write!(f, "cannot write {}: {source}", path.display())
            }
            LinkError::Parse(source) => write!(f, "{source}"),
            LinkError::UnsupportedInputFormat {
                path,
                member,
                format,
                remedy,
            } => {
                write!(f, "{}", path.display())?;
                if let Some(member) = member {
                    write!(f, "({member})")?;
                }
                write!(f, " is {format}; {remedy}")
            }
            LinkError::Archive(source) => write!(f, "{source}"),
            LinkError::UndefinedSymbols { names } => {
                write!(f, "undefined symbols:")?;
                for name in names {
                    write!(f, "\n  {name}")?;
                }
                Ok(())
            }
            LinkError::DuplicateSymbols { names } => {
                write!(f, "duplicate symbol definitions:")?;
                for name in names {
                    write!(f, "\n  {name}")?;
                }
                Ok(())
            }
            LinkError::NothingToLink => write!(f, "no input sections to link"),
            LinkError::OverlappingContributions { section } => write!(
                f,
                "internal: contributions to output section {section} overlap or run past its end"
            ),
            LinkError::MissingOutputSection { section } => {
                write!(f, "internal: the layout has no output section {section}")
            }
            LinkError::NoEntryPoint { symbol } => {
                write!(f, "entry symbol {symbol} is not defined in any input")
            }
            LinkError::MissingObject { object } => {
                write!(f, "internal: object {} is not in the input set", object.0)
            }
            LinkError::MissingSection { object, section } => write!(
                f,
                "internal: object {} has no section {}",
                object.0, section.0
            ),
            LinkError::MissingSymbol { symbol } => {
                write!(f, "internal: no symbol with id {}", symbol.0)
            }
            LinkError::SectionOutOfBounds { object, section } => write!(
                f,
                "object {} declares section {} outside the file",
                object.0, section.0
            ),
            LinkError::UnpairedSubtractor { object, offset } => write!(
                f,
                "object {}: ARM64_RELOC_SUBTRACTOR at {offset:#x} has no paired relocation",
                object.0
            ),
            LinkError::UnwindTableTooLarge { reserved, needed } => write!(
                f,
                "the unwind table needs {needed} bytes but {reserved} were reserved"
            ),
            LinkError::Relocation {
                object,
                kind,
                source,
                symbol,
                target,
            } => match symbol {
                Some(symbol) => write!(
                    f,
                    "object {}: cannot apply {kind:?} against {symbol} \
                     (resolved to {target:#x}): {source}",
                    object.0
                ),
                None => write!(f, "object {}: cannot apply {kind:?}: {source}", object.0),
            },
            LinkError::Emit(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for LinkError {}
