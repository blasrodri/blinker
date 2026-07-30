//! Validating the encoders against instructions the real toolchain produced.
//!
//! The unit tests check the bit arithmetic against the architecture manual.
//! That confirms the code matches my *reading* of the manual — which is exactly
//! the thing that could be wrong. These tests decode instructions `ld64`
//! actually emitted into a linked binary and check that our understanding
//! reproduces them.
//!
//! The strongest property available without executing code: for every `ADRP`
//! in a real binary, decoding it must yield a page that lies inside the image.
//! A misread field would put the target outside the mapped range almost every
//! time.

use blinker_relocations::encode;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// The fixture binary's `__TEXT,__text`, built once and shared.
///
/// Every test here needs the same linked binary. Building per test raced on a
/// shared scratch directory — each test deleting it while the others were
/// still reading — so it is built exactly once instead.
fn linked_text_section() -> Option<&'static (Vec<u8>, u64, u64)> {
    static FIXTURE: OnceLock<Option<(Vec<u8>, u64, u64)>> = OnceLock::new();
    FIXTURE.get_or_init(build_fixture).as_ref()
}

fn build_fixture() -> Option<(Vec<u8>, u64, u64)> {
    let dir = std::env::temp_dir().join(format!("blinker-reloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;

    let source = dir.join("t.c");
    // Enough globals and calls to force ADRP/ADD pairs and BL branches.
    std::fs::write(
        &source,
        r#"
#include <stdio.h>
static const char message[] = "relocation fixture";
static int counter = 0;
int helper(int n) { counter += n; return counter; }
int main(void) {
    for (int i = 0; i < 3; i++) helper(i);
    printf("%s %d\n", message, counter);
    return 0;
}
"#,
    )
    .ok()?;

    let binary = dir.join("t");
    let status = Command::new("cc")
        .args(["-arch", "arm64", "-O1", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    let data = std::fs::read(&binary).ok()?;
    let (vm_address, file_offset, size) = text_section_bounds(&binary)?;
    let start = file_offset as usize;
    let bytes = data.get(start..start + size as usize)?.to_vec();

    let _ = std::fs::remove_dir_all(&dir);
    Some((bytes, vm_address, size))
}

/// Read `__TEXT,__text`'s address, file offset, and size from `otool -l`.
fn text_section_bounds(binary: &PathBuf) -> Option<(u64, u64, u64)> {
    let output = Command::new("otool").arg("-l").arg(binary).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);

    let mut in_text = false;
    let (mut address, mut offset, mut size) = (None, None, None);
    for line in text.lines() {
        let line = line.trim();
        if line == "sectname __text" {
            in_text = true;
            continue;
        }
        if !in_text {
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("addr"), Some(v)) => {
                address = u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()
            }
            (Some("size"), Some(v)) => {
                size = u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()
            }
            (Some("offset"), Some(v)) => offset = v.parse().ok(),
            _ => {}
        }
        if address.is_some() && offset.is_some() && size.is_some() {
            return Some((address?, offset?, size?));
        }
    }
    None
}

/// Whether an instruction word is an `ADRP`.
///
/// Encoding: bit 31 set (op=1), bits [28:24] = 0b10000.
fn is_adrp(instruction: u32) -> bool {
    (instruction & 0x9F00_0000) == 0x9000_0000
}

/// Whether an instruction word is a `B` or `BL`.
fn is_branch(instruction: u32) -> bool {
    let masked = instruction & 0xFC00_0000;
    masked == 0x1400_0000 || masked == 0x9400_0000
}

/// Every `ADRP` in a real binary must resolve to a page inside the image.
///
/// A misread immediate would land outside the mapped range almost every time,
/// so this catches a field-offset or sign-extension error immediately.
#[test]
fn adrp_targets_in_a_real_binary_land_inside_the_image() {
    let Some((bytes, text_address, _)) = linked_text_section() else {
        panic!("could not build and read a linked binary");
    };

    let mut checked = 0;
    for (index, word) in bytes.chunks_exact(4).enumerate() {
        let instruction = u32::from_le_bytes(word.try_into().expect("4 bytes"));
        if !is_adrp(instruction) {
            continue;
        }

        let place = *text_address + (index as u64 * 4);
        let pages = encode::decode_adrp(instruction);
        let target_page = (encode::page_of(place) as i64 + (pages << 12)) as u64;

        // The image is mapped from 0x100000000 upward. A decode error puts the
        // target far outside that window.
        assert!(
            (0x1_0000_0000..0x2_0000_0000).contains(&target_page),
            "ADRP at {place:#x} decoded to page {target_page:#x}, outside the image"
        );
        checked += 1;
    }

    assert!(checked > 0, "no ADRP instructions found to check");
}

/// Re-encoding an instruction with its own decoded value must reproduce it
/// exactly. Any asymmetry between the encoder and decoder shows up here.
#[test]
fn encoding_a_real_adrps_own_value_reproduces_it_bit_for_bit() {
    let Some((bytes, _, _)) = linked_text_section() else {
        panic!("could not build and read a linked binary");
    };

    let mut checked = 0;
    for word in bytes.chunks_exact(4) {
        let instruction = u32::from_le_bytes(word.try_into().expect("4 bytes"));
        if !is_adrp(instruction) {
            continue;
        }

        let pages = encode::decode_adrp(instruction);
        let reencoded = encode::encode_adrp(instruction, pages).expect("its own value fits");
        assert_eq!(
            reencoded, instruction,
            "re-encoding {instruction:#010x} produced {reencoded:#010x}"
        );
        checked += 1;
    }

    assert!(checked > 0, "no ADRP instructions found to check");
}

/// The same round-trip property for branches, plus the range invariant: every
/// branch in a real binary lands within `__TEXT`.
#[test]
fn branches_in_a_real_binary_round_trip_and_stay_in_range() {
    let Some((bytes, text_address, size)) = linked_text_section() else {
        panic!("could not build and read a linked binary");
    };

    let mut checked = 0;
    for (index, word) in bytes.chunks_exact(4).enumerate() {
        let instruction = u32::from_le_bytes(word.try_into().expect("4 bytes"));
        if !is_branch(instruction) {
            continue;
        }

        let displacement = encode::decode_branch26(instruction);
        let reencoded =
            encode::encode_branch26(instruction, displacement).expect("its own value fits");
        assert_eq!(reencoded, instruction, "branch round trip failed");

        // A local branch should land inside __TEXT. Calls into stubs land just
        // past the section, so allow a generous margin rather than asserting
        // an exact bound.
        let place = *text_address + (index as u64 * 4);
        let target = (place as i64 + displacement) as u64;
        let window = text_address.saturating_sub(0x10_0000)..(text_address + size + 0x10_0000);
        assert!(
            window.contains(&target),
            "branch at {place:#x} targets {target:#x}, far outside __TEXT"
        );
        checked += 1;
    }

    assert!(checked > 0, "no branch instructions found to check");
}

/// The `imm12` scale must match what the instruction encoding implies for
/// every load/store the compiler actually emitted.
#[test]
fn imm12_scales_agree_with_real_load_store_instructions() {
    let Some((bytes, _, _)) = linked_text_section() else {
        panic!("could not build and read a linked binary");
    };

    let mut checked = 0;
    for word in bytes.chunks_exact(4) {
        let instruction = u32::from_le_bytes(word.try_into().expect("4 bytes"));

        // Load/store unsigned immediate: bits [29:27] == 0b111.
        if (instruction >> 27) & 0x7 != 0x7 {
            continue;
        }

        let scale = encode::imm12_scale(instruction);
        assert!(
            matches!(scale, 1 | 2 | 4 | 8 | 16),
            "instruction {instruction:#010x} produced an impossible scale {scale}"
        );

        // Decoding then re-encoding the instruction's own offset must be exact.
        let byte_offset = encode::decode_imm12(instruction);
        let reencoded = encode::encode_imm12(instruction, byte_offset).expect("its own value fits");
        assert_eq!(reencoded, instruction, "imm12 round trip failed");
        checked += 1;
    }

    assert!(checked > 0, "no load/store instructions found to check");
}
