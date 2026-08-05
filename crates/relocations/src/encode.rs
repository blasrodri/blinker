//! ARM64 instruction field encoding.
//!
//! Relocations patch immediate fields inside instructions, and the fields are
//! neither contiguous nor uniformly scaled. Every function here does one field,
//! so the bit arithmetic is isolated and individually testable — this is the
//! code where an off-by-one produces a binary that links and then jumps to the
//! wrong address.
//!
//! Encodings are from the ARM Architecture Reference Manual, section C4.1.

/// Errors from encoding a value that does not fit its field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The displacement is outside the field's representable range.
    ///
    /// For `Branch26` this is the case that forces branch islands once a
    /// program grows past ±128 MB.
    OutOfRange { value: i64, bits: u32 },
    /// The value is not a multiple of the field's scale — a misaligned branch
    /// target, or a load offset that does not match the access size.
    Misaligned {
        value: i64,
        alignment: u64,
        /// The instruction whose encoding decided the scale.
        ///
        /// The scale is *decoded* from the instruction, so a misalignment is
        /// as likely to mean the decode was wrong as that the address was.
        /// Without the word there is no way to tell those apart from a log.
        instruction: u32,
    },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::OutOfRange { value, bits } => {
                write!(f, "displacement {value} does not fit in {bits} signed bits")
            }
            EncodeError::Misaligned {
                value,
                alignment,
                instruction,
            } => write!(
                f,
                "value {value} is not {alignment}-byte aligned \
                 (instruction {instruction:#010x})"
            ),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Whether `value` fits in `bits` as a two's-complement signed integer.
pub fn fits_signed(value: i64, bits: u32) -> bool {
    debug_assert!(bits > 0 && bits < 64);
    let limit = 1i64 << (bits - 1);
    value >= -limit && value < limit
}

/// Encode a `B`/`BL` branch displacement into an instruction.
///
/// The `imm26` field holds the displacement in *instructions*, so the byte
/// displacement is shifted right by two. Range is ±128 MB, which is what
/// eventually forces branch islands in large programs.
pub fn encode_branch26(instruction: u32, displacement: i64) -> Result<u32, EncodeError> {
    if displacement % 4 != 0 {
        return Err(EncodeError::Misaligned {
            value: displacement,
            alignment: 4,
            // A branch's scale is fixed by the instruction class, not decoded
            // from the word, so there is nothing informative to report.
            instruction: 0,
        });
    }
    let imm = displacement >> 2;
    if !fits_signed(imm, 26) {
        return Err(EncodeError::OutOfRange {
            value: displacement,
            bits: 26,
        });
    }
    // imm26 occupies bits [25:0]; everything above is opcode.
    Ok((instruction & !0x03FF_FFFF) | (imm as u32 & 0x03FF_FFFF))
}

/// Decode the `imm26` field back to a byte displacement, for tests and
/// diagnostics.
pub fn decode_branch26(instruction: u32) -> i64 {
    let imm = instruction & 0x03FF_FFFF;
    // Sign-extend from 26 bits.
    let signed = ((imm as i32) << 6) >> 6;
    (signed as i64) * 4
}

/// Encode an `ADRP` page displacement.
///
/// `ADRP` splits its 21-bit immediate across two non-adjacent fields: the low
/// two bits sit at [30:29] and the high nineteen at [23:5]. Treating it as one
/// contiguous field is the classic way to get this wrong.
///
/// `page_displacement` is in *pages*, already shifted right by 12.
pub fn encode_adrp(instruction: u32, page_displacement: i64) -> Result<u32, EncodeError> {
    if !fits_signed(page_displacement, 21) {
        return Err(EncodeError::OutOfRange {
            value: page_displacement,
            bits: 21,
        });
    }
    let imm = page_displacement as u32;
    let immlo = imm & 0x3;
    let immhi = (imm >> 2) & 0x7FFFF;

    let cleared = instruction & !((0x3 << 29) | (0x7FFFF << 5));
    Ok(cleared | (immlo << 29) | (immhi << 5))
}

/// Decode an `ADRP` immediate back to a page displacement.
pub fn decode_adrp(instruction: u32) -> i64 {
    let immlo = (instruction >> 29) & 0x3;
    let immhi = (instruction >> 5) & 0x7FFFF;
    let imm = (immhi << 2) | immlo;
    // Sign-extend from 21 bits.
    (((imm as i32) << 11) >> 11) as i64
}

/// The scale an instruction's `imm12` field is measured in.
///
/// This is the subtle part of `PAGEOFF12`. For `ADD` the field is a plain byte
/// offset, but for load/store it is scaled by the access size, so the same
/// byte offset encodes differently depending on whether the instruction moves
/// 1, 2, 4, 8, or 16 bytes. Encoding an `LDR` offset as if it were an `ADD`
/// offset produces an address wrong by a factor of the access width.
pub fn imm12_scale(instruction: u32) -> u64 {
    // Load/store unsigned-immediate: bits [29:27] == 0b111.
    let is_load_store = (instruction >> 27) & 0x7 == 0x7;
    if !is_load_store {
        // ADD/SUB immediate, and anything else we patch: unscaled.
        return 1;
    }

    // `size` at [31:30] gives the access width, except for the 128-bit
    // SIMD forms, which are `size == 0` with the opc bit at [23] set.
    //
    // The `V` bit at [26] is what says the operand is a vector register at
    // all, and leaving it out of the test was a real bug: `LDRSB Wt` — a
    // *signed byte* load — is also `size == 0` with bit [23] set, so it was
    // read as a 128-bit access and its scale came out 16 instead of 1. Every
    // reference through one was then rejected as misaligned unless the target
    // happened to sit on a 16-byte boundary, which is how a working C++
    // program failed to link with `value 871 is not 16-byte aligned`.
    let vector = (instruction >> 26) & 0x1 == 1;
    let size = (instruction >> 30) & 0x3;
    let opc1 = (instruction >> 23) & 0x1;
    if vector && size == 0 && opc1 == 1 {
        16 // 128-bit SIMD load/store
    } else {
        1u64 << size
    }
}

/// Encode a 12-bit immediate at bits [21:10], scaled per the instruction.
pub fn encode_imm12(instruction: u32, byte_offset: u64) -> Result<u32, EncodeError> {
    let scale = imm12_scale(instruction);
    if scale > 1 && !byte_offset.is_multiple_of(scale) {
        return Err(EncodeError::Misaligned {
            value: byte_offset as i64,
            alignment: scale,
            instruction,
        });
    }
    let imm = byte_offset / scale;
    if imm > 0xFFF {
        return Err(EncodeError::OutOfRange {
            value: byte_offset as i64,
            bits: 12,
        });
    }
    Ok((instruction & !(0xFFF << 10)) | ((imm as u32 & 0xFFF) << 10))
}

/// Decode an `imm12` field back to a byte offset.
pub fn decode_imm12(instruction: u32) -> u64 {
    let imm = ((instruction >> 10) & 0xFFF) as u64;
    imm * imm12_scale(instruction)
}

/// The page an address belongs to, for `ADRP`.
pub fn page_of(address: u64) -> u64 {
    address & !0xFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_range_check_covers_both_ends() {
        assert!(fits_signed(0, 8));
        assert!(fits_signed(127, 8));
        assert!(fits_signed(-128, 8));
        assert!(!fits_signed(128, 8));
        assert!(!fits_signed(-129, 8));
    }

    /// `B` with displacement 0 is `0x14000000`; the field is in instructions,
    /// so +4 bytes encodes as 1.
    #[test]
    fn branch26_encodes_forward_displacements() {
        let b = 0x1400_0000u32;
        assert_eq!(encode_branch26(b, 0).expect("fits"), 0x1400_0000);
        assert_eq!(encode_branch26(b, 4).expect("fits"), 0x1400_0001);
        assert_eq!(encode_branch26(b, 16).expect("fits"), 0x1400_0004);
    }

    #[test]
    fn branch26_round_trips_through_decode() {
        let b = 0x1400_0000u32;
        for displacement in [0, 4, -4, 1024, -1024, 0x7FF_FFFC, -0x800_0000] {
            let encoded = encode_branch26(b, displacement).expect("fits");
            assert_eq!(
                decode_branch26(encoded),
                displacement,
                "round trip failed for {displacement}"
            );
        }
    }

    #[test]
    fn branch26_preserves_the_opcode() {
        // BL is 0x94000000; patching the immediate must not turn it into B.
        let bl = 0x9400_0000u32;
        let encoded = encode_branch26(bl, 64).expect("fits");
        assert_eq!(encoded & 0xFC00_0000, 0x9400_0000, "opcode was clobbered");
    }

    /// ±128 MB. This limit is what eventually forces branch islands.
    #[test]
    fn branch26_rejects_displacements_beyond_128_megabytes() {
        let b = 0x1400_0000u32;
        assert!(encode_branch26(b, 0x7FF_FFFC).is_ok());
        assert!(encode_branch26(b, 0x800_0000).is_err());
        assert!(encode_branch26(b, -0x800_0000).is_ok());
        assert!(encode_branch26(b, -0x800_0004).is_err());
    }

    #[test]
    fn branch26_rejects_a_misaligned_target() {
        // Instructions are 4-byte aligned; anything else is a bug upstream.
        let b = 0x1400_0000u32;
        assert!(matches!(
            encode_branch26(b, 2),
            Err(EncodeError::Misaligned { .. })
        ));
    }

    /// ADRP's immediate is split across two non-adjacent fields. Treating it
    /// as contiguous is the classic error.
    #[test]
    fn adrp_encodes_into_two_separate_fields() {
        let adrp = 0x9000_0000u32;

        // 3 = 0b11: entirely within immlo at [30:29], immhi stays zero.
        let encoded = encode_adrp(adrp, 3).expect("fits");
        assert_eq!((encoded >> 29) & 0x3, 3, "immlo");
        assert_eq!((encoded >> 5) & 0x7FFFF, 0, "immhi should be empty");

        // 4 = 0b100: spills into immhi with immlo zero.
        let encoded = encode_adrp(adrp, 4).expect("fits");
        assert_eq!((encoded >> 29) & 0x3, 0, "immlo");
        assert_eq!((encoded >> 5) & 0x7FFFF, 1, "immhi");
    }

    #[test]
    fn adrp_round_trips_through_decode() {
        let adrp = 0x9000_0000u32;
        for pages in [0, 1, 3, 4, 100, -1, -4, -100, 0xF_FFFF, -0x10_0000] {
            let encoded = encode_adrp(adrp, pages).expect("fits");
            assert_eq!(decode_adrp(encoded), pages, "round trip failed for {pages}");
        }
    }

    #[test]
    fn adrp_preserves_the_register_field() {
        // Rd is at [4:0] and must survive patching.
        let adrp_x8 = 0x9000_0008u32;
        let encoded = encode_adrp(adrp_x8, 42).expect("fits");
        assert_eq!(encoded & 0x1F, 8, "destination register was clobbered");
    }

    #[test]
    fn adrp_rejects_displacements_beyond_21_signed_bits() {
        let adrp = 0x9000_0000u32;
        assert!(encode_adrp(adrp, 0xF_FFFF).is_ok());
        assert!(encode_adrp(adrp, 0x10_0000).is_err());
        assert!(encode_adrp(adrp, -0x10_0000).is_ok());
        assert!(encode_adrp(adrp, -0x10_0001).is_err());
    }

    #[test]
    fn page_of_masks_off_the_low_twelve_bits() {
        assert_eq!(page_of(0x1000), 0x1000);
        assert_eq!(page_of(0x1FFF), 0x1000);
        assert_eq!(page_of(0x2000), 0x2000);
        assert_eq!(page_of(0x1_0000_0FFF), 0x1_0000_0000);
    }

    /// The scaling rule that makes `PAGEOFF12` subtle: the same byte offset
    /// encodes differently depending on the instruction's access width.
    #[test]
    fn imm12_scale_follows_the_access_width() {
        // ADD immediate — unscaled.
        assert_eq!(imm12_scale(0x9100_0000), 1);
        // LDRB (size=00): 1 byte.
        assert_eq!(imm12_scale(0x3940_0000), 1);
        // LDRH (size=01): 2 bytes.
        assert_eq!(imm12_scale(0x7940_0000), 2);
        // LDR 32-bit (size=10): 4 bytes.
        assert_eq!(imm12_scale(0xB940_0000), 4);
        // LDR 64-bit (size=11): 8 bytes.
        assert_eq!(imm12_scale(0xF940_0000), 8);
    }

    #[test]
    fn imm12_encodes_an_add_offset_unscaled() {
        let add = 0x9100_0000u32;
        let encoded = encode_imm12(add, 8).expect("fits");
        assert_eq!((encoded >> 10) & 0xFFF, 8);
        assert_eq!(decode_imm12(encoded), 8);
    }

    /// Encoding an `LDR` offset as if it were unscaled yields an address wrong
    /// by a factor of the access width — a bug invisible until runtime.
    #[test]
    fn imm12_scales_a_64_bit_load_offset() {
        let ldr = 0xF940_0000u32;
        let encoded = encode_imm12(ldr, 8).expect("fits");
        assert_eq!((encoded >> 10) & 0xFFF, 1, "8 bytes is 1 unit of 8");
        assert_eq!(decode_imm12(encoded), 8, "decodes back to the byte offset");
    }

    #[test]
    fn imm12_rejects_an_offset_that_does_not_match_the_access_width() {
        // A 64-bit load cannot address an offset of 4.
        let ldr = 0xF940_0000u32;
        assert!(matches!(
            encode_imm12(ldr, 4),
            Err(EncodeError::Misaligned { .. })
        ));
    }

    #[test]
    fn imm12_rejects_offsets_beyond_the_field() {
        let add = 0x9100_0000u32;
        assert!(encode_imm12(add, 0xFFF).is_ok());
        assert!(encode_imm12(add, 0x1000).is_err());

        // Scaling extends the reach: a 64-bit load covers 8 × 0xFFF bytes.
        let ldr = 0xF940_0000u32;
        assert!(encode_imm12(ldr, 0xFFF * 8).is_ok());
        assert!(encode_imm12(ldr, 0x1000 * 8).is_err());
    }

    #[test]
    fn imm12_preserves_the_register_fields() {
        let ldr = 0xF940_0123u32;
        let encoded = encode_imm12(ldr, 8).expect("fits");
        assert_eq!(encoded & 0x3FF, 0x123, "Rn/Rt were clobbered");
    }
}

#[cfg(test)]
mod scale_tests {
    use super::imm12_scale;

    /// Real instruction words, decoded by hand against the ARM ARM's
    /// "Load/store register (unsigned immediate)" encoding. Each of these was
    /// taken from an object file a real program failed to link.
    #[test]
    fn the_scale_matches_the_access_width() {
        // LDR Xt, [Xn, #imm]  — size=11, V=0, opc=01
        assert_eq!(imm12_scale(0xf940_0108), 8);
        // LDR Qt, [Xn, #imm]  — size=00, V=1, opc=11: the 128-bit SIMD form
        assert_eq!(imm12_scale(0x3dc0_0141), 16);
        // LDRSB Wt, [Xn, #imm] — size=00, V=0, opc=11. Byte-sized, and the
        // reason the `V` bit has to be in the test: without it this reads as
        // the 128-bit form and every reference through it is rejected.
        assert_eq!(imm12_scale(0x39c0_0108), 1);
        // ADD Xd, Xn, #imm — not a load/store at all, so unscaled.
        assert_eq!(imm12_scale(0x9100_0000), 1);
    }

    /// A byte-sized access can address any offset; a 16-byte one cannot.
    #[test]
    fn a_byte_load_encodes_an_odd_offset() {
        assert!(super::encode_imm12(0x39c0_0108, 871).is_ok());
        assert!(super::encode_imm12(0x3dc0_0141, 871).is_err());
    }
}
