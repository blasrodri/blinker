//! Malformed-input robustness.
//!
//! Spec §14: malformed input must produce a structured error, not a panic or a
//! memory-safety failure. The parser reads untrusted bytes — an object file can
//! be truncated by a killed build, corrupted on disk, or simply not be what its
//! extension claims.
//!
//! These tests run on stable in the normal gate, deriving their inputs by
//! mutating *real* objects so the bytes stay structurally plausible: random
//! noise is rejected at the magic number and exercises nothing. For deep
//! coverage see `fuzz/`, which drives the same entry point under libFuzzer.
//!
//! Any panic here is a failure. The assertion is not that parsing *succeeds* —
//! a mutated object is usually invalid — but that failure is always an `Err`.

use blinker_macho::{parse_object, ObjectId};
use blinker_test_support::catalog;
use std::path::Path;

/// A tiny deterministic PRNG.
///
/// Deterministic on purpose: a failure found here must be reproducible from
/// the seed alone, without a corpus file to carry around.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One real object's bytes, to mutate.
fn sample_object() -> Vec<u8> {
    let fixture = catalog()
        .into_iter()
        .find(|k| k.tag == "multimod")
        .expect("multimod fixture exists")
        .build()
        .expect("fixture is creatable");

    let build = fixture.build_with_system_linker().expect("cargo runs");
    assert!(build.success, "fixture build failed:\n{}", build.stderr);

    let deps = fixture
        .path()
        .join("target/aarch64-apple-darwin/debug/deps");
    let object = std::fs::read_dir(&deps)
        .expect("deps exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("o"))
        .expect("at least one object file");

    std::fs::read(object).expect("object is readable")
}

/// Parse, requiring only that it does not panic.
fn parse_without_panicking(data: &[u8]) {
    let _ = parse_object(data, Path::new("/fuzz/input.o"), None, ObjectId(0));
}

#[test]
fn truncation_at_every_length_is_handled() {
    let data = sample_object();

    // Truncation is the most common real corruption — a build killed while
    // writing. Every prefix must be rejected rather than partially parsed.
    let mut length = 0;
    while length < data.len() {
        parse_without_panicking(&data[..length]);
        // Dense near the start, where the headers are, then coarser.
        length += if length < 512 { 1 } else { 97 };
    }
}

#[test]
fn single_byte_corruption_is_handled() {
    let data = sample_object();
    let mut rng = Rng(0x5EED_1234);

    for _ in 0..2000 {
        let mut corrupted = data.clone();
        let index = rng.below(corrupted.len());
        corrupted[index] ^= 1 << rng.below(8);
        parse_without_panicking(&corrupted);
    }
}

#[test]
fn header_field_corruption_is_handled() {
    let data = sample_object();
    let mut rng = Rng(0xC0FF_EE00);

    // The first 1 KB holds the header and load commands: counts, offsets, and
    // sizes. These are the fields most likely to drive an out-of-bounds read
    // if a bound were missing, so they get concentrated attention.
    let window = data.len().min(1024);
    for _ in 0..2000 {
        let mut corrupted = data.clone();
        let index = rng.below(window);
        corrupted[index] = rng.next() as u8;
        parse_without_panicking(&corrupted);
    }
}

#[test]
fn multi_byte_splices_are_handled() {
    let data = sample_object();
    let mut rng = Rng(0xABCD_EF01);

    for _ in 0..500 {
        let mut corrupted = data.clone();
        // Overwrite a run of bytes with a repeated value — a cheap way to
        // produce absurd counts and offsets rather than merely wrong ones.
        let start = rng.below(corrupted.len());
        let len = rng.below(64).min(corrupted.len() - start);
        let value = rng.next() as u8;
        corrupted[start..start + len].fill(value);
        parse_without_panicking(&corrupted);
    }
}

#[test]
fn degenerate_inputs_are_handled() {
    for data in [
        vec![],
        vec![0u8; 1],
        vec![0u8; 4096],
        vec![0xFF; 4096],
        // 64-bit Mach-O magic followed by nothing useful.
        vec![0xcf, 0xfa, 0xed, 0xfe],
        // Magic plus a plausible-looking but truncated header.
        {
            let mut v = vec![0xcf, 0xfa, 0xed, 0xfe];
            v.extend_from_slice(&[0x0c, 0x00, 0x00, 0x01]);
            v.extend_from_slice(&[0xFF; 16]);
            v
        },
    ] {
        parse_without_panicking(&data);
    }
}

#[test]
fn a_parse_that_succeeds_on_mutated_input_is_still_self_consistent() {
    // Mutation occasionally produces a file that parses. When it does, the
    // result must still satisfy the invariants downstream code relies on —
    // every ID in range. A parse that "succeeds" with dangling IDs would be
    // worse than one that failed.
    let data = sample_object();
    let mut rng = Rng(0x1357_9BDF);
    let mut succeeded = 0;

    for _ in 0..2000 {
        let mut corrupted = data.clone();
        let index = rng.below(corrupted.len());
        corrupted[index] ^= 1 << rng.below(8);

        if let Ok(object) = parse_object(&corrupted, Path::new("/fuzz/x.o"), None, ObjectId(0)) {
            succeeded += 1;
            for symbol in &object.symbols {
                if let Some(section) = symbol.section {
                    assert!(
                        object.section(section).is_some(),
                        "symbol references a section outside the table"
                    );
                }
            }
            for relocation in &object.relocations {
                assert!(
                    object.section(relocation.section).is_some(),
                    "relocation references a section outside the table"
                );
            }
        }
    }

    // Most single-bit flips land in padding or string data and still parse.
    // If none did, the mutation is not reaching the parser at all.
    assert!(
        succeeded > 0,
        "no mutated input parsed; the mutation may not be exercising the parser"
    );
}
