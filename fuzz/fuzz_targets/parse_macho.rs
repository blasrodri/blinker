//! Fuzz the Mach-O object parser.
//!
//! The same entry point the mutation-robustness tests drive on stable, run
//! here under libFuzzer for coverage-guided depth. Seed the corpus with real
//! objects — random bytes are rejected at the magic number and exercise almost
//! nothing:
//!
//!     cargo +nightly fuzz run parse_macho
//!
//! The contract under test is not that parsing succeeds, but that failure is
//! always a structured `Err` and never a panic or a memory-safety fault.

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::Path;

fuzz_target!(|data: &[u8]| {
    let result = blinker_macho::parse_object(data, Path::new("/fuzz/input.o"), None, blinker_macho::ObjectId(0));

    // A parse that succeeds must be internally consistent: downstream code
    // treats every ID as an index, so a dangling one is a latent panic.
    if let Ok(object) = result {
        for symbol in &object.symbols {
            if let Some(section) = symbol.section {
                assert!(object.section(section).is_some());
            }
        }
        for relocation in &object.relocations {
            assert!(object.section(relocation.section).is_some());
        }
    }
});
