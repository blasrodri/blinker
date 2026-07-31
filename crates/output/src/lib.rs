//! Writing Mach-O executables.
//!
//! Emitting is a different problem from parsing. The `macho` crate wraps a
//! fuzzed parser because its input is untrusted; here there is no untrusted
//! input, and the requirement is instead that the bytes match an exact shape
//! that dyld will accept. Every constant is cross-checked against a real Rust
//! executable rather than taken from the header files alone.
//!
//! # The strategy choice
//!
//! Spec §22 asks for one dyld metadata strategy, correct before any second one
//! is attempted. Measurement settled it: at rustc's macOS 11 deployment target
//! the toolchain emits classic `LC_DYLD_INFO_ONLY` opcode streams, not chained
//! fixups — those appear only at macOS 12 and above. So classic it is.

pub mod commands;
pub mod dyld_info;
pub mod format;
pub mod image;
pub mod signature;
pub mod symtab;
pub mod unwind;

pub use commands::{LinkEditLayout, SymbolGroups};
pub use dyld_info::{Bind, Rebase};
pub use format::{MachHeader, Writer};
pub use image::{Dylib, Image, ImageBuilder, ImageError};
pub use signature::{sign, signature_size, SignatureRequest};
pub use symtab::{OutputSymbol, SymbolGroup, SymbolTable, SymbolTableBuilder};
pub use unwind::UnwindEntry;
