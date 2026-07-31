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
    /// An archive could not be read or indexed.
    Archive(Box<blinker_archive::ArchiveError>),
    /// Symbols referenced but never defined.
    UndefinedSymbols {
        names: Vec<String>,
    },
    /// No object contributed anything placeable.
    NothingToLink,
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
    Relocation {
        object: ObjectId,
        kind: Arm64RelocationKind,
        source: Box<blinker_relocations::RelocationError>,
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
            LinkError::Archive(source) => write!(f, "{source}"),
            LinkError::UndefinedSymbols { names } => {
                write!(f, "undefined symbols:")?;
                for name in names {
                    write!(f, "\n  {name}")?;
                }
                Ok(())
            }
            LinkError::NothingToLink => write!(f, "no input sections to link"),
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
            LinkError::Relocation {
                object,
                kind,
                source,
            } => write!(f, "object {}: cannot apply {kind:?}: {source}", object.0),
            LinkError::Emit(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for LinkError {}
