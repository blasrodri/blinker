//! Every image must carry its own `LC_UUID`.
//!
//! blinker emitted sixteen zero bytes, with a comment saying a real UUID was
//! "a later step". It was not a cosmetic gap, because the UUID is not
//! decoration: macOS resolves debug information *by* it. Spotlight indexes
//! `.dSYM` bundles under the UUID of the binary they describe, and a
//! symbolicator asked about an address in program A can be handed the debug
//! information for program B if the two claim the same identity — which every
//! blinker output did.
//!
//! How it presented, which is why it took a while to find: the same
//! executable, byte for byte, printed a correct panic backtrace when it was
//! alone in a directory and a wrong one when a sibling blinker binary sat next
//! to it. Nothing about the file changed. The two tests below are the two
//! halves of that.
//!
//! ```text
//!   hello-u1  (content-derived UUID)   1: hello::deep     <- correct
//!   hello-bl3 (zero UUID)              1: std::rt::lang_start::<()>
//! ```
//!
//! Both binaries came from the same linker with the same symbol table. Only
//! the UUID differed.

use blinker_link::{link_to_file, LinkRequest};
use blinker_test_support::Scratch;
use std::path::PathBuf;
use std::process::Command;

const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

fn compile(scratch: &Scratch, name: &str, code: &str) -> Vec<PathBuf> {
    let source = scratch.write(format!("{name}.c"), code).expect("writable");
    let object = scratch.join(format!("{name}.o"));
    let status = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    assert!(status.success(), "cc failed to compile {name}");
    vec![object]
}

/// The sixteen `LC_UUID` payload bytes of a linked image.
///
/// Walks the load commands rather than shelling out to `dwarfdump`, so the
/// test states what it is reading and fails on a malformed file instead of on
/// a parse of someone else's output.
fn uuid_of(path: &PathBuf) -> [u8; 16] {
    const LC_UUID: u32 = 0x1b;
    let bytes = std::fs::read(path).expect("the image is readable");
    let word = |at: usize| {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("inside the header")) as usize
    };
    let count = word(16);
    let mut at = 32; // past the 64-bit Mach-O header
    for _ in 0..count {
        let kind = word(at) as u32;
        let size = word(at + 4);
        assert!(size >= 8, "a load command of {size} bytes cannot exist");
        if kind == LC_UUID {
            return bytes[at + 8..at + 24].try_into().expect("16 payload bytes");
        }
        at += size;
    }
    panic!("the image has no LC_UUID at all");
}

/// Two different programs must not claim the same identity.
///
/// This is the assertion that fails against the zero UUID, and it fails on
/// both counts at once: the values are equal *and* they are zero.
#[test]
fn two_different_programs_get_two_different_uuids() {
    let scratch = Scratch::dir("uuid-distinct").expect("scratch");
    let first = scratch.join("first");
    let second = scratch.join("second");
    link_to_file(
        &LinkRequest::new(compile(&scratch, "a", "int main(void) { return 1; }\n")),
        &first,
    )
    .expect("the first link succeeds");
    link_to_file(
        &LinkRequest::new(compile(&scratch, "b", "int main(void) { return 2; }\n")),
        &second,
    )
    .expect("the second link succeeds");

    let (a, b) = (uuid_of(&first), uuid_of(&second));
    assert_ne!(
        a, [0u8; 16],
        "the image carries no identity at all: {a:02x?}"
    );
    assert_ne!(
        a, b,
        "two different programs claim the same identity: {a:02x?}"
    );
}

/// And the identity must be a *hash*, not a fresh value per link: the same
/// inputs have to keep producing the same bytes.
///
/// This is the half that a random or clock-derived UUID would break, and
/// breaking it would break far more than this test — the cache's entire
/// premise is that an unchanged link produces an unchanged image.
#[test]
fn the_same_inputs_still_produce_a_byte_identical_image() {
    let scratch = Scratch::dir("uuid-stable").expect("scratch");
    let objects = compile(&scratch, "s", "int main(void) { return 0; }\n");
    // Linked to the same path twice, because the output's base name is the
    // identifier signed into the image: two different names differ by design,
    // and comparing them would report a determinism failure that is not one.
    let out = scratch.join("program");

    link_to_file(&LinkRequest::new(objects.clone()), &out).expect("the first link succeeds");
    let first = std::fs::read(&out).expect("readable");
    link_to_file(&LinkRequest::new(objects), &out).expect("the second link succeeds");
    let second = std::fs::read(&out).expect("readable");

    assert_eq!(
        uuid_of(&out),
        {
            std::fs::write(&out, &first).expect("writable");
            uuid_of(&out)
        },
        "the same inputs produced two different identities"
    );
    std::fs::write(&out, &second).expect("writable");
    assert!(
        first == second,
        "relinking the same inputs produced a different image ({} vs {} bytes)",
        first.len(),
        second.len()
    );
}

/// The UUID sits inside the region the ad-hoc signature covers, so stamping it
/// at the wrong moment produces an image whose signature does not match its
/// own bytes. macOS refuses to run that, which makes this the test that
/// catches an ordering mistake rather than a value mistake.
#[test]
fn the_image_is_still_correctly_signed_and_runs() {
    let scratch = Scratch::dir("uuid-signed").expect("scratch");
    let objects = compile(&scratch, "r", "int main(void) { return 9; }\n");
    let out = scratch.join("program");
    link_to_file(&LinkRequest::new(objects), &out).expect("the link succeeds");

    let verified = Command::new("codesign")
        .args(["--verify", "--verbose=2"])
        .arg(&out)
        .output()
        .expect("codesign runs");
    assert!(
        verified.status.success(),
        "the signature does not match the image:\n{}",
        String::from_utf8_lossy(&verified.stderr)
    );

    let status = Command::new(&out).status().expect("the program runs");
    assert_eq!(status.code(), Some(9));
}
