//! ARM64 Mach-O relocations.
//!
//! Every relocation kind blinker supports is enumerated here explicitly. An
//! `r_type` outside the set becomes [`RelocationError::UnsupportedType`] rather
//! than a default or a guess — spec §20 is explicit that unknown relocations
//! must not be guessed, because a wrong relocation produces a binary that links
//! cleanly and then misbehaves at runtime.
//!
//! The set was chosen by census rather than by reading the ARM64 ABI: 2,276
//! objects from four real Rust projects contain exactly these ten kinds. See
//! `DECISIONS.md` D2.

use std::fmt;

/// An ARM64 Mach-O relocation kind.
///
/// Discriminants are the on-disk `r_type` values from `<mach-o/arm64/reloc.h>`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(u8)]
pub enum Arm64RelocationKind {
    /// `ARM64_RELOC_UNSIGNED` — a plain absolute pointer. The most common kind
    /// by a wide margin (~46% of all relocations observed).
    Unsigned = 0,
    /// `ARM64_RELOC_SUBTRACTOR` — the first half of a pair encoding a
    /// difference between two addresses. Always immediately followed by an
    /// `Unsigned` relocation naming the other operand.
    Subtractor = 1,
    /// `ARM64_RELOC_BRANCH26` — a `B`/`BL` branch displacement, 26 bits of
    /// instruction-relative offset. ~27% of relocations, and the kind whose
    /// range limit forces branch islands once code grows.
    Branch26 = 2,
    /// `ARM64_RELOC_PAGE21` — the page portion of an `ADRP` address.
    Page21 = 3,
    /// `ARM64_RELOC_PAGEOFF12` — the offset-within-page portion, pairing with
    /// a preceding `Page21`.
    PageOff12 = 4,
    /// `ARM64_RELOC_GOT_LOAD_PAGE21` — `ADRP` page of a GOT entry.
    GotLoadPage21 = 5,
    /// `ARM64_RELOC_GOT_LOAD_PAGEOFF12` — offset within the GOT page.
    GotLoadPageOff12 = 6,
    /// `ARM64_RELOC_POINTER_TO_GOT` — a pointer to a GOT entry.
    PointerToGot = 7,
    /// `ARM64_RELOC_TLVP_LOAD_PAGE21` — `ADRP` page of a thread-local variable
    /// descriptor. Rare (a handful per project) but load-bearing: this is how
    /// TLS is addressed, and getting it wrong breaks thread locals in a way no
    /// aggregate count would reveal.
    TlvpLoadPage21 = 8,
    /// `ARM64_RELOC_TLVP_LOAD_PAGEOFF12` — offset within the TLV page.
    TlvpLoadPageOff12 = 9,
}

impl Arm64RelocationKind {
    /// Map an on-disk `r_type`, refusing anything outside the supported set.
    ///
    /// `ARM64_RELOC_ADDEND` (10) and `ARM64_RELOC_AUTHENTICATED_POINTER` (11)
    /// are deliberately absent: neither appears in any observed Rust build, and
    /// accepting them without an implementation would be worse than refusing.
    pub fn from_r_type(r_type: u8) -> Result<Self, RelocationError> {
        Ok(match r_type {
            0 => Arm64RelocationKind::Unsigned,
            1 => Arm64RelocationKind::Subtractor,
            2 => Arm64RelocationKind::Branch26,
            3 => Arm64RelocationKind::Page21,
            4 => Arm64RelocationKind::PageOff12,
            5 => Arm64RelocationKind::GotLoadPage21,
            6 => Arm64RelocationKind::GotLoadPageOff12,
            7 => Arm64RelocationKind::PointerToGot,
            8 => Arm64RelocationKind::TlvpLoadPage21,
            9 => Arm64RelocationKind::TlvpLoadPageOff12,
            other => return Err(RelocationError::UnsupportedType(other)),
        })
    }

    /// The `ARM64_RELOC_*` spelling, for diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Arm64RelocationKind::Unsigned => "ARM64_RELOC_UNSIGNED",
            Arm64RelocationKind::Subtractor => "ARM64_RELOC_SUBTRACTOR",
            Arm64RelocationKind::Branch26 => "ARM64_RELOC_BRANCH26",
            Arm64RelocationKind::Page21 => "ARM64_RELOC_PAGE21",
            Arm64RelocationKind::PageOff12 => "ARM64_RELOC_PAGEOFF12",
            Arm64RelocationKind::GotLoadPage21 => "ARM64_RELOC_GOT_LOAD_PAGE21",
            Arm64RelocationKind::GotLoadPageOff12 => "ARM64_RELOC_GOT_LOAD_PAGEOFF12",
            Arm64RelocationKind::PointerToGot => "ARM64_RELOC_POINTER_TO_GOT",
            Arm64RelocationKind::TlvpLoadPage21 => "ARM64_RELOC_TLVP_LOAD_PAGE21",
            Arm64RelocationKind::TlvpLoadPageOff12 => "ARM64_RELOC_TLVP_LOAD_PAGEOFF12",
        }
    }

    /// Whether this kind reaches its target through the global offset table.
    pub fn is_got_based(self) -> bool {
        matches!(
            self,
            Arm64RelocationKind::GotLoadPage21
                | Arm64RelocationKind::GotLoadPageOff12
                | Arm64RelocationKind::PointerToGot
        )
    }

    /// Whether this kind addresses a thread-local variable descriptor.
    pub fn is_thread_local(self) -> bool {
        matches!(
            self,
            Arm64RelocationKind::TlvpLoadPage21 | Arm64RelocationKind::TlvpLoadPageOff12
        )
    }

    /// Whether this kind begins a relocation *pair*.
    ///
    /// `Subtractor` is always followed by an `Unsigned` naming the other
    /// operand of the difference; the two must be processed together.
    pub fn starts_pair(self) -> bool {
        self == Arm64RelocationKind::Subtractor
    }
}

impl fmt::Display for Arm64RelocationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Encoded width of the field a relocation patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RelocationLength {
    Byte = 0,
    Half = 1,
    Word = 2,
    Long = 3,
}

impl RelocationLength {
    /// Map the two-bit on-disk `r_length` field.
    pub fn from_r_length(r_length: u8) -> Result<Self, RelocationError> {
        Ok(match r_length {
            0 => RelocationLength::Byte,
            1 => RelocationLength::Half,
            2 => RelocationLength::Word,
            3 => RelocationLength::Long,
            other => return Err(RelocationError::InvalidLength(other)),
        })
    }

    /// Width in bytes of the patched field.
    pub fn byte_width(self) -> u64 {
        1 << (self as u8)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelocationError {
    /// An `r_type` outside the supported set. Carries the raw value so the
    /// diagnostic can name exactly what was found.
    UnsupportedType(u8),
    /// `r_length` outside the two bits it is defined over — only reachable
    /// from a malformed or corrupt file.
    InvalidLength(u8),
}

impl fmt::Display for RelocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelocationError::UnsupportedType(t) => {
                write!(f, "unsupported ARM64 relocation type {t}")
            }
            RelocationError::InvalidLength(l) => write!(f, "invalid relocation length {l}"),
        }
    }
}

impl std::error::Error for RelocationError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind the census found in real Rust objects must map.
    #[test]
    fn maps_every_observed_relocation_type() {
        let expected = [
            (0, "ARM64_RELOC_UNSIGNED"),
            (1, "ARM64_RELOC_SUBTRACTOR"),
            (2, "ARM64_RELOC_BRANCH26"),
            (3, "ARM64_RELOC_PAGE21"),
            (4, "ARM64_RELOC_PAGEOFF12"),
            (5, "ARM64_RELOC_GOT_LOAD_PAGE21"),
            (6, "ARM64_RELOC_GOT_LOAD_PAGEOFF12"),
            (7, "ARM64_RELOC_POINTER_TO_GOT"),
            (8, "ARM64_RELOC_TLVP_LOAD_PAGE21"),
            (9, "ARM64_RELOC_TLVP_LOAD_PAGEOFF12"),
        ];
        for (r_type, name) in expected {
            let kind = Arm64RelocationKind::from_r_type(r_type).expect("observed kind maps");
            assert_eq!(kind.name(), name);
            assert_eq!(kind as u8, r_type, "discriminant must match on-disk value");
        }
    }

    /// The two defined-but-unobserved kinds must be refused, not silently
    /// accepted. Accepting one without an implementation would produce a
    /// binary that links and then misbehaves.
    #[test]
    fn refuses_defined_but_unimplemented_types() {
        for (r_type, what) in [(10, "ADDEND"), (11, "AUTHENTICATED_POINTER")] {
            assert_eq!(
                Arm64RelocationKind::from_r_type(r_type),
                Err(RelocationError::UnsupportedType(r_type)),
                "{what} should be refused until implemented"
            );
        }
    }

    #[test]
    fn refuses_types_outside_the_defined_range() {
        for r_type in [12, 15, 200, 255] {
            assert!(Arm64RelocationKind::from_r_type(r_type).is_err());
        }
    }

    #[test]
    fn error_names_the_offending_type() {
        let err = Arm64RelocationKind::from_r_type(42).unwrap_err();
        assert!(err.to_string().contains("42"));
    }

    #[test]
    fn identifies_got_based_kinds() {
        use Arm64RelocationKind::*;
        for kind in [GotLoadPage21, GotLoadPageOff12, PointerToGot] {
            assert!(kind.is_got_based(), "{kind} is GOT-based");
        }
        for kind in [Unsigned, Branch26, Page21, PageOff12, Subtractor] {
            assert!(!kind.is_got_based(), "{kind} is not GOT-based");
        }
    }

    #[test]
    fn identifies_thread_local_kinds() {
        use Arm64RelocationKind::*;
        assert!(TlvpLoadPage21.is_thread_local());
        assert!(TlvpLoadPageOff12.is_thread_local());
        assert!(!Page21.is_thread_local());
        // The TLS pair must not be confused with the ordinary ADRP pair —
        // they differ only by kind and target a different addressing scheme.
        assert!(!GotLoadPage21.is_thread_local());
    }

    #[test]
    fn only_subtractor_starts_a_pair() {
        use Arm64RelocationKind::*;
        assert!(Subtractor.starts_pair());
        for kind in [Unsigned, Branch26, Page21, PageOff12, PointerToGot] {
            assert!(!kind.starts_pair());
        }
    }

    #[test]
    fn maps_relocation_lengths_to_byte_widths() {
        for (r_length, width) in [(0, 1), (1, 2), (2, 4), (3, 8)] {
            let len = RelocationLength::from_r_length(r_length).expect("valid length");
            assert_eq!(len.byte_width(), width);
        }
    }

    #[test]
    fn refuses_out_of_range_relocation_length() {
        assert_eq!(
            RelocationLength::from_r_length(4),
            Err(RelocationError::InvalidLength(4))
        );
    }

    #[test]
    fn kinds_round_trip_through_serde() {
        // The parsed form is cached in M4, so every type in it must survive a
        // serialization round trip unchanged.
        for r_type in 0..=9 {
            let kind = Arm64RelocationKind::from_r_type(r_type).expect("valid");
            let json = serde_json::to_string(&kind).expect("serializes");
            let back: Arm64RelocationKind = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(kind, back);
        }
    }
}
