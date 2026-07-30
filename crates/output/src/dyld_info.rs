//! dyld rebase and bind opcode streams.
//!
//! `LC_DYLD_INFO_ONLY` points at four byte-code programs that dyld interprets
//! at load time. Two matter for a Rust executable:
//!
//! - **rebase** — slide every absolute pointer in the image by the load bias.
//!   A PIE binary is loaded at a random address, so every pointer stored in
//!   `__DATA` needs adjusting.
//! - **bind** — resolve each imported symbol and store its address.
//!
//! Both are stack machines with an immediate packed into the low nibble of the
//! opcode byte. That packing is the thing to get right: the opcode and its
//! immediate share a byte, so emitting them as separate bytes produces a
//! stream that dyld misreads from that point on rather than rejecting.
//!
//! Sizes to aim at, from a real Rust binary: rebase 168 bytes, bind 48, lazy
//! bind 1504, export 22016.

use crate::format::Writer;

/// Rebase opcodes. The low nibble carries an immediate.
pub mod rebase_opcode {
    pub const DONE: u8 = 0x00;
    pub const SET_TYPE_IMM: u8 = 0x10;
    pub const SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x20;
    pub const ADD_ADDR_ULEB: u8 = 0x30;
    pub const ADD_ADDR_IMM_SCALED: u8 = 0x40;
    pub const DO_REBASE_IMM_TIMES: u8 = 0x50;
    pub const DO_REBASE_ULEB_TIMES: u8 = 0x60;
    pub const DO_REBASE_ADD_ADDR_ULEB: u8 = 0x70;
    pub const DO_REBASE_ULEB_TIMES_SKIPPING_ULEB: u8 = 0x80;
}

/// Bind opcodes.
pub mod bind_opcode {
    pub const DONE: u8 = 0x00;
    pub const SET_DYLIB_ORDINAL_IMM: u8 = 0x10;
    pub const SET_DYLIB_ORDINAL_ULEB: u8 = 0x20;
    pub const SET_DYLIB_SPECIAL_IMM: u8 = 0x30;
    pub const SET_SYMBOL_TRAILING_FLAGS_IMM: u8 = 0x40;
    pub const SET_TYPE_IMM: u8 = 0x50;
    pub const SET_ADDEND_SLEB: u8 = 0x60;
    pub const SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x70;
    pub const ADD_ADDR_ULEB: u8 = 0x80;
    pub const DO_BIND: u8 = 0x90;
    pub const DO_BIND_ADD_ADDR_ULEB: u8 = 0xA0;
    pub const DO_BIND_ADD_ADDR_IMM_SCALED: u8 = 0xB0;
    pub const DO_BIND_ULEB_TIMES_SKIPPING_ULEB: u8 = 0xC0;
}

/// What kind of value a rebase or bind writes.
pub const REBASE_TYPE_POINTER: u8 = 1;
pub const BIND_TYPE_POINTER: u8 = 1;

/// Mask selecting the opcode from a byte; the low nibble is the immediate.
pub const OPCODE_MASK: u8 = 0xF0;
pub const IMMEDIATE_MASK: u8 = 0x0F;

/// Append an unsigned LEB128 value.
///
/// The encoding dyld reads for every offset and count in these streams.
pub fn write_uleb(writer: &mut Writer, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        writer.bytes(&[byte]);
        if value == 0 {
            break;
        }
    }
}

/// Append a signed LEB128 value.
pub fn write_sleb(writer: &mut Writer, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        // Arithmetic shift keeps the sign, which is what makes the
        // termination condition below correct for negatives.
        value >>= 7;
        let sign_bit_set = byte & 0x40 != 0;
        let done = (value == 0 && !sign_bit_set) || (value == -1 && sign_bit_set);
        writer.bytes(&[if done { byte } else { byte | 0x80 }]);
        if done {
            break;
        }
    }
}

/// Decode an unsigned LEB128 value, returning it and the bytes consumed.
pub fn read_uleb(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (index, &byte) in bytes.iter().enumerate() {
        // More than ten bytes cannot fit a u64; refuse rather than wrapping.
        if shift >= 64 {
            return None;
        }
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
        shift += 7;
    }
    None
}

/// One absolute pointer that needs sliding by the load bias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebase {
    /// Index of the segment holding the pointer.
    pub segment: u8,
    /// Offset of the pointer within that segment.
    pub offset: u64,
}

/// Encode a rebase stream.
///
/// Emits one `SET_SEGMENT_AND_OFFSET` per entry rather than exploiting runs.
/// Correct but not compact — a smaller encoding is a later optimisation, and
/// doing it now would risk correctness for a few hundred bytes.
pub fn encode_rebase(rebases: &[Rebase]) -> Vec<u8> {
    let mut writer = Writer::new();
    if rebases.is_empty() {
        return writer.finish();
    }

    writer.bytes(&[rebase_opcode::SET_TYPE_IMM | REBASE_TYPE_POINTER]);

    let mut sorted = rebases.to_vec();
    // Sorted so the stream is deterministic regardless of discovery order.
    sorted.sort_by_key(|r| (r.segment, r.offset));

    for rebase in sorted {
        writer.bytes(&[rebase_opcode::SET_SEGMENT_AND_OFFSET_ULEB | (rebase.segment & 0x0F)]);
        write_uleb(&mut writer, rebase.offset);
        writer.bytes(&[rebase_opcode::DO_REBASE_IMM_TIMES | 1]);
    }

    writer.bytes(&[rebase_opcode::DONE]);
    writer.finish()
}

/// One imported symbol to resolve at load time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub segment: u8,
    pub offset: u64,
    pub symbol: String,
    /// One-based index of the providing library.
    pub library_ordinal: u8,
    pub addend: i64,
}

/// Encode a bind stream.
pub fn encode_bind(binds: &[Bind]) -> Vec<u8> {
    let mut writer = Writer::new();
    if binds.is_empty() {
        return writer.finish();
    }

    writer.bytes(&[bind_opcode::SET_TYPE_IMM | BIND_TYPE_POINTER]);

    let mut sorted = binds.to_vec();
    sorted.sort_by(|a, b| (a.segment, a.offset, &a.symbol).cmp(&(b.segment, b.offset, &b.symbol)));

    // Track what the machine's registers already hold, so redundant
    // instructions are skipped — the one compression that cannot go wrong.
    let mut current_ordinal = None;
    let mut current_addend = 0i64;

    for bind in sorted {
        if current_ordinal != Some(bind.library_ordinal) {
            // The immediate form covers ordinals 1..=15, which is every
            // realistic case; beyond that the ULEB form is required.
            if bind.library_ordinal <= 0x0F {
                writer.bytes(&[bind_opcode::SET_DYLIB_ORDINAL_IMM | bind.library_ordinal]);
            } else {
                writer.bytes(&[bind_opcode::SET_DYLIB_ORDINAL_ULEB]);
                write_uleb(&mut writer, bind.library_ordinal as u64);
            }
            current_ordinal = Some(bind.library_ordinal);
        }

        if current_addend != bind.addend {
            writer.bytes(&[bind_opcode::SET_ADDEND_SLEB]);
            write_sleb(&mut writer, bind.addend);
            current_addend = bind.addend;
        }

        // Symbol name, NUL-terminated, with flags in the immediate.
        writer.bytes(&[bind_opcode::SET_SYMBOL_TRAILING_FLAGS_IMM]);
        writer.bytes(bind.symbol.as_bytes());
        writer.bytes(&[0]);

        writer.bytes(&[bind_opcode::SET_SEGMENT_AND_OFFSET_ULEB | (bind.segment & 0x0F)]);
        write_uleb(&mut writer, bind.offset);
        writer.bytes(&[bind_opcode::DO_BIND]);
    }

    writer.bytes(&[bind_opcode::DONE]);
    writer.finish()
}

/// One decoded instruction from a bind stream.
///
/// Decoding exists because these streams **cannot be inspected by scanning**.
/// Data is embedded inline: `uleb(16)` is the byte `0x10`, which is also
/// `SET_DYLIB_ORDINAL_IMM`, and the `a` in a symbol name is `0x61`, which is
/// also `SET_ADDEND_SLEB | 1`. Only a walk that consumes operands as it goes
/// can tell an opcode from a byte that merely looks like one — which is
/// exactly what dyld does, and therefore what validation must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindOp {
    SetType(u8),
    SetDylibOrdinal(u64),
    SetSymbol(String),
    SetAddend(i64),
    SetSegmentAndOffset { segment: u8, offset: u64 },
    AddAddress(u64),
    DoBind,
    Done,
}

/// Walk a bind stream, returning its instructions.
///
/// Returns `None` on a malformed stream rather than guessing, so a validation
/// pass can distinguish "well-formed" from "we gave up".
pub fn decode_bind(stream: &[u8]) -> Option<Vec<BindOp>> {
    let mut ops = Vec::new();
    let mut cursor = 0usize;

    while cursor < stream.len() {
        let byte = stream[cursor];
        cursor += 1;
        let opcode = byte & OPCODE_MASK;
        let immediate = byte & IMMEDIATE_MASK;

        match opcode {
            bind_opcode::DONE => ops.push(BindOp::Done),
            bind_opcode::SET_TYPE_IMM => ops.push(BindOp::SetType(immediate)),
            bind_opcode::SET_DYLIB_ORDINAL_IMM => {
                ops.push(BindOp::SetDylibOrdinal(immediate as u64))
            }
            bind_opcode::SET_DYLIB_ORDINAL_ULEB => {
                let (value, used) = read_uleb(stream.get(cursor..)?)?;
                cursor += used;
                ops.push(BindOp::SetDylibOrdinal(value));
            }
            bind_opcode::SET_SYMBOL_TRAILING_FLAGS_IMM => {
                let rest = stream.get(cursor..)?;
                let end = rest.iter().position(|&b| b == 0)?;
                let name = std::str::from_utf8(&rest[..end]).ok()?.to_string();
                cursor += end + 1;
                ops.push(BindOp::SetSymbol(name));
            }
            bind_opcode::SET_ADDEND_SLEB => {
                // An SLEB is self-terminating: the last byte has its
                // continuation bit clear.
                let rest = stream.get(cursor..)?;
                let end = rest.iter().position(|&b| b & 0x80 == 0)?;
                cursor += end + 1;
                ops.push(BindOp::SetAddend(0));
            }
            bind_opcode::SET_SEGMENT_AND_OFFSET_ULEB => {
                let (offset, used) = read_uleb(stream.get(cursor..)?)?;
                cursor += used;
                ops.push(BindOp::SetSegmentAndOffset {
                    segment: immediate,
                    offset,
                });
            }
            bind_opcode::ADD_ADDR_ULEB => {
                let (value, used) = read_uleb(stream.get(cursor..)?)?;
                cursor += used;
                ops.push(BindOp::AddAddress(value));
            }
            bind_opcode::DO_BIND => ops.push(BindOp::DoBind),
            _ => return None,
        }
    }
    Some(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uleb(value: u64) -> Vec<u8> {
        let mut w = Writer::new();
        write_uleb(&mut w, value);
        w.finish()
    }

    fn sleb(value: i64) -> Vec<u8> {
        let mut w = Writer::new();
        write_sleb(&mut w, value);
        w.finish()
    }

    #[test]
    fn uleb_encodes_small_values_in_one_byte() {
        assert_eq!(uleb(0), vec![0x00]);
        assert_eq!(uleb(1), vec![0x01]);
        assert_eq!(uleb(127), vec![0x7f]);
    }

    #[test]
    fn uleb_sets_the_continuation_bit_on_multibyte_values() {
        assert_eq!(uleb(128), vec![0x80, 0x01]);
        assert_eq!(uleb(300), vec![0xac, 0x02]);
        assert_eq!(uleb(16384), vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn uleb_round_trips_across_the_whole_range() {
        for value in [
            0,
            1,
            127,
            128,
            255,
            256,
            0x3fff,
            0x4000,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let encoded = uleb(value);
            let (decoded, consumed) = read_uleb(&encoded).expect("decodes");
            assert_eq!(decoded, value, "round trip failed for {value}");
            assert_eq!(consumed, encoded.len(), "consumed the wrong length");
        }
    }

    #[test]
    fn reading_an_unterminated_uleb_fails_rather_than_running_off() {
        // Every byte has the continuation bit set and the input ends.
        assert_eq!(read_uleb(&[0x80, 0x80, 0x80]), None);
        assert_eq!(read_uleb(&[]), None);
    }

    #[test]
    fn reading_an_overlong_uleb_fails_rather_than_wrapping() {
        // Eleven continuation bytes cannot fit a u64.
        let overlong = vec![0x80; 11];
        assert_eq!(read_uleb(&overlong), None);
    }

    #[test]
    fn sleb_encodes_small_positive_values_like_uleb() {
        assert_eq!(sleb(0), vec![0x00]);
        assert_eq!(sleb(1), vec![0x01]);
        assert_eq!(sleb(63), vec![0x3f]);
    }

    /// The sign bit is bit 6, not bit 7 — a value of 64 needs a second byte
    /// even though it fits in seven bits.
    #[test]
    fn sleb_uses_bit_six_as_the_sign() {
        assert_eq!(sleb(64), vec![0xc0, 0x00]);
        assert_eq!(sleb(-1), vec![0x7f]);
        assert_eq!(sleb(-64), vec![0x40]);
        assert_eq!(sleb(-65), vec![0xbf, 0x7f]);
    }

    #[test]
    fn sleb_terminates_on_negative_values() {
        // A naive loop that only stops at zero never terminates for negatives.
        for value in [-1, -100, -1000, i32::MIN as i64] {
            let encoded = sleb(value);
            assert!(
                encoded.len() < 12,
                "sleb({value}) produced {} bytes",
                encoded.len()
            );
            assert_eq!(
                encoded.last().unwrap() & 0x80,
                0,
                "last byte must terminate"
            );
        }
    }

    #[test]
    fn an_empty_rebase_stream_is_empty() {
        // No pointers to slide means no program at all, not a program that
        // does nothing.
        assert!(encode_rebase(&[]).is_empty());
        assert!(encode_bind(&[]).is_empty());
    }

    #[test]
    fn a_rebase_stream_sets_the_type_and_terminates() {
        let stream = encode_rebase(&[Rebase {
            segment: 2,
            offset: 0x10,
        }]);
        assert_eq!(
            stream[0],
            rebase_opcode::SET_TYPE_IMM | REBASE_TYPE_POINTER,
            "stream must open by setting the type"
        );
        assert_eq!(
            *stream.last().expect("non-empty"),
            rebase_opcode::DONE,
            "stream must terminate"
        );
    }

    /// The packing that matters: opcode in the high nibble, immediate in the
    /// low. Emitting them as separate bytes desynchronises dyld's read.
    #[test]
    fn opcodes_pack_their_immediate_into_the_low_nibble() {
        let stream = encode_rebase(&[Rebase {
            segment: 3,
            offset: 0x20,
        }]);

        let set = stream[1];
        assert_eq!(
            set & OPCODE_MASK,
            rebase_opcode::SET_SEGMENT_AND_OFFSET_ULEB
        );
        assert_eq!(set & IMMEDIATE_MASK, 3, "segment index is the immediate");
    }

    #[test]
    fn a_rebase_entrys_offset_follows_as_uleb() {
        let stream = encode_rebase(&[Rebase {
            segment: 1,
            offset: 300,
        }]);
        let (offset, _) = read_uleb(&stream[2..]).expect("decodes");
        assert_eq!(offset, 300);
    }

    #[test]
    fn rebase_entries_are_emitted_in_a_deterministic_order() {
        // Discovery order must not change the bytes.
        let a = encode_rebase(&[
            Rebase {
                segment: 2,
                offset: 0x20,
            },
            Rebase {
                segment: 1,
                offset: 0x10,
            },
        ]);
        let b = encode_rebase(&[
            Rebase {
                segment: 1,
                offset: 0x10,
            },
            Rebase {
                segment: 2,
                offset: 0x20,
            },
        ]);
        assert_eq!(a, b);
    }

    #[test]
    fn a_bind_stream_carries_the_symbol_name_nul_terminated() {
        let stream = encode_bind(&[Bind {
            segment: 2,
            offset: 0x10,
            symbol: "_malloc".to_string(),
            library_ordinal: 1,
            addend: 0,
        }]);

        let ops = decode_bind(&stream).expect("stream decodes");
        assert!(ops
            .iter()
            .any(|op| matches!(op, BindOp::SetSymbol(name) if name == "_malloc")));
    }

    #[test]
    fn a_bind_stream_sets_the_library_ordinal() {
        let stream = encode_bind(&[Bind {
            segment: 2,
            offset: 0x10,
            symbol: "_free".to_string(),
            library_ordinal: 1,
            addend: 0,
        }]);
        let ops = decode_bind(&stream).expect("stream decodes");
        assert!(ops.contains(&BindOp::SetDylibOrdinal(1)));
    }

    /// Re-emitting a register the machine already holds is wasted bytes; not
    /// emitting a register it does *not* hold is a wrong bind.
    #[test]
    fn a_repeated_library_ordinal_is_set_only_once() {
        let binds: Vec<Bind> = (0..3)
            .map(|i| Bind {
                segment: 2,
                offset: i * 8,
                symbol: format!("_sym{i}"),
                library_ordinal: 1,
                addend: 0,
            })
            .collect();
        let ops = decode_bind(&encode_bind(&binds)).expect("stream decodes");

        let ordinal_sets = ops
            .iter()
            .filter(|op| matches!(op, BindOp::SetDylibOrdinal(_)))
            .count();
        assert_eq!(
            ordinal_sets, 1,
            "ordinal should be set once for one library"
        );
    }

    #[test]
    fn a_changed_library_ordinal_is_re_emitted() {
        let stream = encode_bind(&[
            Bind {
                segment: 2,
                offset: 0,
                symbol: "_a".into(),
                library_ordinal: 1,
                addend: 0,
            },
            Bind {
                segment: 2,
                offset: 8,
                symbol: "_b".into(),
                library_ordinal: 2,
                addend: 0,
            },
        ]);
        let ops = decode_bind(&stream).expect("stream decodes");
        let ordinal_sets = ops
            .iter()
            .filter(|op| matches!(op, BindOp::SetDylibOrdinal(_)))
            .count();
        assert_eq!(ordinal_sets, 2);
    }

    #[test]
    fn an_addend_is_emitted_only_when_non_zero() {
        let without = encode_bind(&[Bind {
            segment: 2,
            offset: 0,
            symbol: "_a".into(),
            library_ordinal: 1,
            addend: 0,
        }]);
        let ops = decode_bind(&without).expect("stream decodes");
        assert!(!ops.iter().any(|op| matches!(op, BindOp::SetAddend(_))));

        let with = encode_bind(&[Bind {
            segment: 2,
            offset: 0,
            symbol: "_a".into(),
            library_ordinal: 1,
            addend: 16,
        }]);
        let ops = decode_bind(&with).expect("stream decodes");
        assert!(ops.iter().any(|op| matches!(op, BindOp::SetAddend(_))));
    }

    #[test]
    fn every_bind_ends_with_a_do_bind() {
        let stream = encode_bind(&[Bind {
            segment: 2,
            offset: 0x10,
            symbol: "_malloc".into(),
            library_ordinal: 1,
            addend: 0,
        }]);
        let ops = decode_bind(&stream).expect("stream decodes");
        let do_binds = ops.iter().filter(|op| **op == BindOp::DoBind).count();
        assert_eq!(do_binds, 1);
        assert_eq!(ops.last(), Some(&BindOp::Done));
    }

    #[test]
    fn a_high_library_ordinal_falls_back_to_the_uleb_form() {
        // The immediate is a nibble; ordinals above 15 need the wider form.
        let stream = encode_bind(&[Bind {
            segment: 2,
            offset: 0,
            symbol: "_a".into(),
            library_ordinal: 20,
            addend: 0,
        }]);
        let ops = decode_bind(&stream).expect("stream decodes");
        assert!(ops.contains(&BindOp::SetDylibOrdinal(20)));
    }

    /// The property that makes scanning wrong, pinned explicitly. Both of
    /// these encode data whose bytes are identical to opcodes, and an earlier
    /// version of these tests was fooled by exactly this.
    #[test]
    fn embedded_data_can_look_exactly_like_an_opcode() {
        // uleb(16) is 0x10, which is also SET_DYLIB_ORDINAL_IMM.
        assert_eq!(uleb(16), vec![bind_opcode::SET_DYLIB_ORDINAL_IMM]);
        // The `a` in a symbol name is 0x61, which is also SET_ADDEND_SLEB | 1.
        assert_eq!(b'a' & OPCODE_MASK, bind_opcode::SET_ADDEND_SLEB);

        // A walk is not fooled by either.
        let stream = encode_bind(&[Bind {
            segment: 1,
            offset: 16,
            symbol: "_a".into(),
            library_ordinal: 1,
            addend: 0,
        }]);
        let ops = decode_bind(&stream).expect("stream decodes");
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, BindOp::SetDylibOrdinal(_)))
                .count(),
            1,
            "the uleb 0x10 was miscounted as an opcode"
        );
        assert!(!ops.iter().any(|op| matches!(op, BindOp::SetAddend(_))));
    }

    #[test]
    fn a_multi_symbol_stream_round_trips_through_the_decoder() {
        // Every stream we emit must be walkable, because dyld walks it.
        let binds: Vec<Bind> = ["_malloc", "_free", "_memcpy", "_abort"]
            .iter()
            .enumerate()
            .map(|(i, name)| Bind {
                segment: 2,
                offset: (i as u64) * 8,
                symbol: (*name).to_string(),
                library_ordinal: 1,
                addend: 0,
            })
            .collect();

        let ops = decode_bind(&encode_bind(&binds)).expect("stream decodes");
        let names: Vec<&str> = ops
            .iter()
            .filter_map(|op| match op {
                BindOp::SetSymbol(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        // Ordered by (segment, offset) — deterministic, and the order the
        // addresses appear in, not alphabetical.
        assert_eq!(names, vec!["_malloc", "_free", "_memcpy", "_abort"]);
        assert_eq!(
            ops.iter().filter(|op| **op == BindOp::DoBind).count(),
            4,
            "one DO_BIND per symbol"
        );
    }

    #[test]
    fn a_truncated_stream_fails_to_decode_rather_than_guessing() {
        let stream = encode_bind(&[Bind {
            segment: 2,
            offset: 0x10,
            symbol: "_malloc".into(),
            library_ordinal: 1,
            addend: 0,
        }]);
        // A prefix cut at an instruction boundary is still a valid program,
        // so truncate *inside* the symbol name, where the NUL terminator is
        // lost. That is the case a walk cannot recover from.
        // Byte 0 is SET_TYPE, byte 1 SET_DYLIB_ORDINAL, byte 2 SET_SYMBOL,
        // then the name. Cutting at 5 lands three characters into "_malloc",
        // past the point of no return: the NUL is gone.
        assert_eq!(decode_bind(&stream[..5]), None);
        // Cutting at 2, a genuine instruction boundary, leaves a shorter but
        // well-formed program — the case that must *not* be reported as
        // malformed.
        assert!(decode_bind(&stream[..2]).is_some());
    }

    #[test]
    fn segment_indices_beyond_a_nibble_are_masked_not_overflowed() {
        // A segment index above 15 cannot be represented; masking keeps the
        // opcode byte intact rather than corrupting the adjacent opcode bits.
        let stream = encode_rebase(&[Rebase {
            segment: 0x1F,
            offset: 0,
        }]);
        assert_eq!(
            stream[1] & OPCODE_MASK,
            rebase_opcode::SET_SEGMENT_AND_OFFSET_ULEB
        );
    }
}
