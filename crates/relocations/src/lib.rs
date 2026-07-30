//! Applying ARM64 Mach-O relocations.
//!
//! # Why this is the most dangerous code in the linker
//!
//! Every other failure mode is loud. A missing symbol fails the link; a
//! malformed object fails to parse. A wrong relocation does neither — it
//! produces a binary that links cleanly, loads cleanly, and then branches to
//! the wrong address or reads the wrong global. The build log says nothing.
//!
//! Three properties follow from that:
//!
//! - **Every range is checked.** A displacement that does not fit its field is
//!   an error, not a truncation. Truncating a `BRANCH26` silently redirects a
//!   call.
//! - **Nothing is guessed.** The ten kinds implemented here are the ten the
//!   census found (`DECISIONS.md` D2); anything else was refused at parse time.
//! - **Pairs are handled as pairs.** `SUBTRACTOR` is meaningless alone — it
//!   supplies one operand of a difference whose other half is the next
//!   relocation. Applying either in isolation writes a wrong value.

use blinker_macho::{Arm64RelocationKind, RelocationLength};

pub mod encode;
pub use encode::EncodeError;

/// Everything needed to compute one relocation's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    /// Virtual address of the field being patched. `ADRP` and branches are
    /// relative to this.
    pub place: u64,
    /// Virtual address the relocation refers to, after symbol resolution.
    pub target: u64,
    /// Addend from the instruction stream or the relocation entry.
    pub addend: i64,
    /// Address of the target's GOT entry, for the GOT-based kinds. `None` when
    /// the kind does not need one.
    pub got: Option<u64>,
    /// Address of the target's thread-local descriptor, for the TLV kinds.
    pub tlv: Option<u64>,
}

impl Context {
    /// A context for a simple, non-indirect relocation.
    pub fn direct(place: u64, target: u64, addend: i64) -> Self {
        Context {
            place,
            target,
            addend,
            got: None,
            tlv: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelocationError {
    /// A field could not represent the computed value.
    Encoding {
        kind: Arm64RelocationKind,
        place: u64,
        source: EncodeError,
    },
    /// A GOT- or TLV-based relocation with no indirect address supplied.
    MissingIndirection {
        kind: Arm64RelocationKind,
        place: u64,
    },
    /// The patch site lies outside the section's bytes.
    OutOfBounds {
        place: u64,
        offset: u64,
        available: usize,
    },
    /// A `SUBTRACTOR` not followed by the `UNSIGNED` that completes it.
    UnpairedSubtractor { place: u64 },
    /// A relocation width the kind does not support.
    UnsupportedWidth {
        kind: Arm64RelocationKind,
        length: RelocationLength,
    },
}

impl std::fmt::Display for RelocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelocationError::Encoding {
                kind,
                place,
                source,
            } => write!(f, "{kind} at {place:#x}: {source}"),
            RelocationError::MissingIndirection { kind, place } => write!(
                f,
                "{kind} at {place:#x} needs an indirect address that was not supplied"
            ),
            RelocationError::OutOfBounds {
                place,
                offset,
                available,
            } => write!(
                f,
                "relocation at {place:#x} patches offset {offset} of a {available}-byte section"
            ),
            RelocationError::UnpairedSubtractor { place } => write!(
                f,
                "ARM64_RELOC_SUBTRACTOR at {place:#x} is not followed by a paired relocation"
            ),
            RelocationError::UnsupportedWidth { kind, length } => {
                write!(f, "{kind} cannot be applied at width {length:?}")
            }
        }
    }
}

impl std::error::Error for RelocationError {}

/// Read a little-endian instruction word from `bytes` at `offset`.
fn read_u32(bytes: &[u8], offset: u64) -> Option<u32> {
    let start = usize::try_from(offset).ok()?;
    let slice = bytes.get(start..start.checked_add(4)?)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn write_u32(bytes: &mut [u8], offset: u64, value: u32) -> Option<()> {
    let start = usize::try_from(offset).ok()?;
    let slice = bytes.get_mut(start..start.checked_add(4)?)?;
    slice.copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// Write `value` at `offset`, truncated to `length` bytes.
fn write_scalar(bytes: &mut [u8], offset: u64, value: u64, length: RelocationLength) -> Option<()> {
    let start = usize::try_from(offset).ok()?;
    let width = length.byte_width() as usize;
    let slice = bytes.get_mut(start..start.checked_add(width)?)?;
    let encoded = value.to_le_bytes();
    slice.copy_from_slice(&encoded[..width]);
    Some(())
}

/// Apply one relocation to a section's bytes.
///
/// `offset` is the position within `bytes`; `context.place` is the virtual
/// address that same position will have in the output, which is what the
/// PC-relative kinds are computed against.
pub fn apply(
    kind: Arm64RelocationKind,
    length: RelocationLength,
    offset: u64,
    context: Context,
    bytes: &mut [u8],
) -> Result<(), RelocationError> {
    use Arm64RelocationKind::*;

    // Captured up front: the error closure must not hold a borrow of `bytes`
    // across the mutable writes below.
    let available = bytes.len();
    let out_of_bounds = move || RelocationError::OutOfBounds {
        place: context.place,
        offset,
        available,
    };

    match kind {
        // A plain pointer. The only kind whose width varies.
        Unsigned => {
            let value = context.target.wrapping_add(context.addend as u64);
            match length {
                RelocationLength::Word | RelocationLength::Long => {
                    write_scalar(bytes, offset, value, length).ok_or_else(out_of_bounds)
                }
                other => Err(RelocationError::UnsupportedWidth {
                    kind,
                    length: other,
                }),
            }
        }

        // Meaningless alone: it supplies one operand of a difference that the
        // following relocation completes. `apply_pair` handles the real case.
        Subtractor => Err(RelocationError::UnpairedSubtractor {
            place: context.place,
        }),

        // A branch displacement, relative to the instruction itself.
        Branch26 => {
            let instruction = read_u32(bytes, offset).ok_or_else(out_of_bounds)?;
            let displacement = context
                .target
                .wrapping_add(context.addend as u64)
                .wrapping_sub(context.place) as i64;
            let patched = encode::encode_branch26(instruction, displacement).map_err(|source| {
                RelocationError::Encoding {
                    kind,
                    place: context.place,
                    source,
                }
            })?;
            write_u32(bytes, offset, patched).ok_or_else(out_of_bounds)
        }

        // The page half of an address, relative to the patch site's page.
        Page21 | GotLoadPage21 | TlvpLoadPage21 => {
            let target = indirect_target(kind, &context)?;
            let instruction = read_u32(bytes, offset).ok_or_else(out_of_bounds)?;
            let target = target.wrapping_add(context.addend as u64);

            // Both sides are masked to their page *before* subtracting: the
            // displacement is between pages, not between addresses, and
            // subtracting first would be wrong whenever the two sit at
            // different offsets within their pages.
            let pages =
                (encode::page_of(target) as i64 - encode::page_of(context.place) as i64) >> 12;
            let patched = encode::encode_adrp(instruction, pages).map_err(|source| {
                RelocationError::Encoding {
                    kind,
                    place: context.place,
                    source,
                }
            })?;
            write_u32(bytes, offset, patched).ok_or_else(out_of_bounds)
        }

        // The within-page half, scaled per the instruction's access width.
        PageOff12 | GotLoadPageOff12 | TlvpLoadPageOff12 => {
            let target = indirect_target(kind, &context)?;
            let instruction = read_u32(bytes, offset).ok_or_else(out_of_bounds)?;
            let target = target.wrapping_add(context.addend as u64);
            let patched = encode::encode_imm12(instruction, target & 0xFFF).map_err(|source| {
                RelocationError::Encoding {
                    kind,
                    place: context.place,
                    source,
                }
            })?;
            write_u32(bytes, offset, patched).ok_or_else(out_of_bounds)
        }

        // The address of the GOT entry itself, written as a pointer.
        PointerToGot => {
            let got = context.got.ok_or(RelocationError::MissingIndirection {
                kind,
                place: context.place,
            })?;
            write_scalar(bytes, offset, got, length).ok_or_else(out_of_bounds)
        }
    }
}

/// The address a kind actually refers to: the symbol, its GOT entry, or its
/// thread-local descriptor.
fn indirect_target(kind: Arm64RelocationKind, context: &Context) -> Result<u64, RelocationError> {
    let missing = || RelocationError::MissingIndirection {
        kind,
        place: context.place,
    };
    if kind.is_got_based() {
        context.got.ok_or_else(missing)
    } else if kind.is_thread_local() {
        context.tlv.ok_or_else(missing)
    } else {
        Ok(context.target)
    }
}

/// Apply a `SUBTRACTOR`/`UNSIGNED` pair, which encodes a difference.
///
/// The value written is `minuend - subtrahend + addend`. Both relocations
/// share a patch site, and applying either alone would write a wrong value —
/// hence a separate entry point rather than two calls to [`apply`].
pub fn apply_pair(
    length: RelocationLength,
    offset: u64,
    subtrahend: u64,
    minuend: u64,
    addend: i64,
    place: u64,
    bytes: &mut [u8],
) -> Result<(), RelocationError> {
    let value = minuend.wrapping_sub(subtrahend).wrapping_add(addend as u64);
    write_scalar(bytes, offset, value, length).ok_or(RelocationError::OutOfBounds {
        place,
        offset,
        available: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Arm64RelocationKind::*;

    /// A section whose bytes are all zero except a supplied instruction.
    fn section_with(instruction: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 32];
        bytes[..4].copy_from_slice(&instruction.to_le_bytes());
        bytes
    }

    fn read(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
    }

    #[test]
    fn unsigned_writes_a_64_bit_pointer() {
        let mut bytes = vec![0u8; 16];
        apply(
            Unsigned,
            RelocationLength::Long,
            0,
            Context::direct(0x1000, 0x1_0000_4000, 0),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            0x1_0000_4000
        );
    }

    #[test]
    fn unsigned_writes_a_32_bit_pointer_without_touching_neighbours() {
        let mut bytes = vec![0xFFu8; 16];
        apply(
            Unsigned,
            RelocationLength::Word,
            0,
            Context::direct(0x1000, 0xDEAD_BEEF, 0),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(read(&bytes, 0), 0xDEAD_BEEF);
        assert_eq!(bytes[4], 0xFF, "wrote past the field");
    }

    #[test]
    fn unsigned_applies_its_addend() {
        let mut bytes = vec![0u8; 16];
        apply(
            Unsigned,
            RelocationLength::Long,
            0,
            Context::direct(0x1000, 0x2000, 0x10),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(u64::from_le_bytes(bytes[..8].try_into().unwrap()), 0x2010);
    }

    #[test]
    fn unsigned_rejects_widths_it_cannot_represent() {
        let mut bytes = vec![0u8; 16];
        for length in [RelocationLength::Byte, RelocationLength::Half] {
            assert!(matches!(
                apply(
                    Unsigned,
                    length,
                    0,
                    Context::direct(0x1000, 0x2000, 0),
                    &mut bytes
                ),
                Err(RelocationError::UnsupportedWidth { .. })
            ));
        }
    }

    #[test]
    fn branch26_computes_a_displacement_relative_to_the_instruction() {
        // BL at 0x1000 targeting 0x1100 → +0x100 bytes → 64 instructions.
        let mut bytes = section_with(0x9400_0000);
        apply(
            Branch26,
            RelocationLength::Word,
            0,
            Context::direct(0x1000, 0x1100, 0),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(encode::decode_branch26(read(&bytes, 0)), 0x100);
    }

    #[test]
    fn branch26_handles_a_backward_branch() {
        let mut bytes = section_with(0x1400_0000);
        apply(
            Branch26,
            RelocationLength::Word,
            0,
            Context::direct(0x2000, 0x1000, 0),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(encode::decode_branch26(read(&bytes, 0)), -0x1000);
    }

    /// Truncating instead of failing would silently redirect a call — the
    /// exact failure this whole module is shaped to avoid.
    #[test]
    fn branch26_out_of_range_is_an_error_not_a_truncation() {
        let mut bytes = section_with(0x9400_0000);
        let err = apply(
            Branch26,
            RelocationLength::Word,
            0,
            // Well beyond ±128 MB.
            Context::direct(0x1000, 0x1_0000_0000, 0),
            &mut bytes,
        )
        .unwrap_err();
        assert!(matches!(err, RelocationError::Encoding { .. }));
        // The bytes must be left untouched when the relocation fails.
        assert_eq!(read(&bytes, 0), 0x9400_0000);
    }

    /// The page displacement is between *pages*, so both operands are masked
    /// before subtracting. Subtracting first then masking gives a different —
    /// wrong — answer whenever the two sit at different page offsets.
    #[test]
    fn page21_masks_both_operands_before_subtracting() {
        let mut bytes = section_with(0x9000_0000);
        // Place at 0x1FFF (page 0x1000), target 0x2001 (page 0x2000):
        // one page apart, though the raw difference is only 2 bytes.
        apply(
            Page21,
            RelocationLength::Word,
            0,
            Context::direct(0x1FFF, 0x2001, 0),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(encode::decode_adrp(read(&bytes, 0)), 1);
    }

    #[test]
    fn page21_handles_same_page_and_backward_cases() {
        for (place, target, expected) in [
            (0x1000u64, 0x1FFF_u64, 0i64),
            (0x2000, 0x1000, -1),
            (0x1_0000_0000, 0x1_0001_0000, 16),
        ] {
            let mut bytes = section_with(0x9000_0000);
            apply(
                Page21,
                RelocationLength::Word,
                0,
                Context::direct(place, target, 0),
                &mut bytes,
            )
            .expect("applies");
            assert_eq!(
                encode::decode_adrp(read(&bytes, 0)),
                expected,
                "place={place:#x} target={target:#x}"
            );
        }
    }

    #[test]
    fn pageoff12_writes_the_low_twelve_bits() {
        let mut bytes = section_with(0x9100_0000); // ADD immediate
        apply(
            PageOff12,
            RelocationLength::Word,
            0,
            Context::direct(0x1000, 0x2ABC, 0),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(encode::decode_imm12(read(&bytes, 0)), 0xABC);
    }

    /// The scaling rule in practice: a 64-bit load encodes 0xAB8 as 0x157.
    #[test]
    fn pageoff12_scales_for_a_load_instruction() {
        let mut bytes = section_with(0xF940_0000); // LDR 64-bit
        apply(
            PageOff12,
            RelocationLength::Word,
            0,
            Context::direct(0x1000, 0x2AB8, 0),
            &mut bytes,
        )
        .expect("applies");
        assert_eq!((read(&bytes, 0) >> 10) & 0xFFF, 0xAB8 / 8);
        assert_eq!(encode::decode_imm12(read(&bytes, 0)), 0xAB8);
    }

    #[test]
    fn got_relocations_use_the_got_address_not_the_symbol() {
        let mut bytes = section_with(0x9000_0000);
        let context = Context {
            place: 0x1000,
            // If the symbol address were used the answer would be 15 pages.
            target: 0x1_0000,
            addend: 0,
            got: Some(0x5000),
            tlv: None,
        };
        apply(
            GotLoadPage21,
            RelocationLength::Word,
            0,
            context,
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(
            encode::decode_adrp(read(&bytes, 0)),
            4,
            "used the GOT entry"
        );
    }

    #[test]
    fn thread_local_relocations_use_the_descriptor_address() {
        let mut bytes = section_with(0x9000_0000);
        let context = Context {
            place: 0x1000,
            target: 0x1_0000,
            addend: 0,
            got: Some(0x9_0000),
            tlv: Some(0x3000),
        };
        apply(
            TlvpLoadPage21,
            RelocationLength::Word,
            0,
            context,
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(
            encode::decode_adrp(read(&bytes, 0)),
            2,
            "used the TLV descriptor, not the GOT"
        );
    }

    #[test]
    fn indirect_relocations_without_their_address_are_an_error() {
        // Defaulting to the symbol address here would produce a binary that
        // reads the variable's contents as if they were its descriptor.
        for kind in [
            GotLoadPage21,
            GotLoadPageOff12,
            TlvpLoadPage21,
            TlvpLoadPageOff12,
            PointerToGot,
        ] {
            let mut bytes = section_with(0x9000_0000);
            let err = apply(
                kind,
                RelocationLength::Long,
                0,
                Context::direct(0x1000, 0x2000, 0),
                &mut bytes,
            )
            .unwrap_err();
            assert!(
                matches!(err, RelocationError::MissingIndirection { .. }),
                "{kind} should require an indirect address"
            );
        }
    }

    #[test]
    fn pointer_to_got_writes_the_got_entrys_address() {
        let mut bytes = vec![0u8; 16];
        let context = Context {
            place: 0x1000,
            target: 0x9999,
            addend: 0,
            got: Some(0x1_0000_8000),
            tlv: None,
        };
        apply(PointerToGot, RelocationLength::Long, 0, context, &mut bytes).expect("applies");
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            0x1_0000_8000
        );
    }

    /// A `SUBTRACTOR` alone is meaningless; applying it as if it were complete
    /// would write a wrong value with no error.
    #[test]
    fn a_lone_subtractor_is_refused() {
        let mut bytes = vec![0u8; 16];
        let err = apply(
            Subtractor,
            RelocationLength::Long,
            0,
            Context::direct(0x1000, 0x2000, 0),
            &mut bytes,
        )
        .unwrap_err();
        assert!(matches!(err, RelocationError::UnpairedSubtractor { .. }));
    }

    #[test]
    fn a_subtractor_pair_writes_the_difference() {
        let mut bytes = vec![0u8; 16];
        apply_pair(
            RelocationLength::Long,
            0,
            0x1000, // subtrahend
            0x3000, // minuend
            0,
            0x500,
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(u64::from_le_bytes(bytes[..8].try_into().unwrap()), 0x2000);
    }

    #[test]
    fn a_subtractor_pair_can_produce_a_negative_difference() {
        // Differences run both ways; two's complement must be preserved.
        let mut bytes = vec![0u8; 16];
        apply_pair(
            RelocationLength::Long,
            0,
            0x3000,
            0x1000,
            0,
            0x500,
            &mut bytes,
        )
        .expect("applies");
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()) as i64,
            -0x2000
        );
    }

    #[test]
    fn a_patch_site_outside_the_section_is_an_error() {
        let mut bytes = vec![0u8; 8];
        for kind in [Unsigned, Branch26, Page21] {
            let err = apply(
                kind,
                RelocationLength::Long,
                100,
                Context::direct(0x1000, 0x2000, 0),
                &mut bytes,
            )
            .unwrap_err();
            assert!(
                matches!(err, RelocationError::OutOfBounds { .. }),
                "{kind} should refuse an out-of-bounds site"
            );
        }
    }

    #[test]
    fn a_patch_site_that_straddles_the_end_is_an_error() {
        // Six bytes left, an eight-byte write: must not write four and stop.
        let mut bytes = vec![0u8; 6];
        assert!(matches!(
            apply(
                Unsigned,
                RelocationLength::Long,
                0,
                Context::direct(0x1000, 0x2000, 0),
                &mut bytes
            ),
            Err(RelocationError::OutOfBounds { .. })
        ));
    }

    /// The realistic sequence: `ADRP` then `ADD` reconstructing one address.
    #[test]
    fn an_adrp_add_pair_reconstructs_the_target_address() {
        let target = 0x1_0000_5ABCu64;
        let place = 0x1_0000_1000u64;

        let mut bytes = vec![0u8; 8];
        bytes[..4].copy_from_slice(&0x9000_0000u32.to_le_bytes()); // ADRP x0
        bytes[4..].copy_from_slice(&0x9100_0000u32.to_le_bytes()); // ADD x0, x0

        apply(
            Page21,
            RelocationLength::Word,
            0,
            Context::direct(place, target, 0),
            &mut bytes,
        )
        .expect("adrp applies");
        apply(
            PageOff12,
            RelocationLength::Word,
            4,
            Context::direct(place + 4, target, 0),
            &mut bytes,
        )
        .expect("add applies");

        // Recompute what the CPU would: ADRP gives the page, ADD the offset.
        let pages = encode::decode_adrp(read(&bytes, 0));
        let reconstructed = (encode::page_of(place) as i64 + (pages << 12)) as u64
            + encode::decode_imm12(read(&bytes, 4));
        assert_eq!(
            reconstructed, target,
            "ADRP/ADD did not reconstruct the target"
        );
    }
}
