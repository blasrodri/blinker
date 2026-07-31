//! The walking skeleton: does an image blinker produced actually *run*?
//!
//! Every other test asks whether the bytes look right — to our own reader, to
//! `otool`, to `nm`, or against what `ld64` emits for the same inputs. All of
//! those can pass for a file the kernel refuses to execute. `otool` is a
//! reader; the kernel is a judge, and it is the only one whose opinion is the
//! product.
//!
//! So this test emits a program whose entire behaviour is "exit 42", runs it,
//! and checks that the exit status is 42. There is no way to pass it by
//! accident: a wrong entry point, a wrong segment protection, a bad
//! `cmdsize`, or a missing signature all produce a process that dies rather
//! than one that returns the wrong number.
//!
//! # Signing is blinker's own
//!
//! The image is signed by `blinker-output` itself, not by `codesign`. That is
//! what makes "it runs" a statement about blinker rather than about the system
//! tool that fixed up its output afterwards.
//!
//! # Why the program calls nothing
//!
//! `main` returning 42 needs no imports of blinker's own. The kernel maps the
//! image, loads `dyld`, `dyld` calls the `LC_MAIN` entry, and when it returns
//! `libdyld` calls `exit` with the value in `w0`. Binding, stubs and the
//! lazy-binding machinery are all still unimplemented — this test deliberately
//! does not need them, so that what it proves is precisely "the image loads
//! and control reaches our code", with nothing else able to fail.

use blinker_layout::InputPlacement;
use blinker_macho::{ObjectId, SectionId, SectionKind};
use blinker_output::image::Dylib;
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Image, ImageBuilder};
use blinker_test_support::Scratch;
use std::process::Command;

/// `mov w0, #42` — MOVZ into the 32-bit return register.
///
/// Encoding: `0101 0010 1 00 imm16 Rd`, so `0x52800000 | (imm16 << 5) | Rd`.
const MOV_W0_42: u32 = 0x5280_0000 | (42 << 5);

/// `ret` — return to the link register.
const RET: u32 = 0xD65F_03C0;

/// The exit status the program is built to produce.
const EXPECTED_STATUS: i32 = 42;

fn program_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&MOV_W0_42.to_le_bytes());
    bytes.extend_from_slice(&RET.to_le_bytes());
    bytes
}

/// Assemble the image.
///
/// Built twice on purpose. `LC_MAIN`'s `entryoff` is a file offset into
/// `__TEXT`, but where `__text` lands depends on how large the load commands
/// are, which is not known until the image has been laid out. The first build
/// discovers the offset and the second uses it — the same circularity
/// `ImageBuilder` resolves internally with two layout passes.
fn build_program() -> Image {
    let code = program_bytes();

    let assemble = |entry_offset: u64| -> Image {
        let mut builder = ImageBuilder::new();
        builder.input(InputPlacement {
            object: ObjectId(0),
            section: SectionId(0),
            segment: "__TEXT".into(),
            name: "__text".into(),
            kind: SectionKind::Code,
            size: code.len() as u64,
            alignment: 4,
        });
        builder.dylib(Dylib::lib_system());
        builder.entry_offset(entry_offset);
        builder.content(0, code.clone());
        // `_main` is not needed to *run* — dyld enters through LC_MAIN, not by
        // name — but an executable with no symbol at its entry point is odd
        // enough that tools complain, and `__mh_execute_header` is what ld64
        // exports for the image itself.
        builder.symbols().add(OutputSymbol::exported(
            "__mh_execute_header",
            1,
            0x1_0000_0000,
        ));
        builder.build().expect("image assembles")
    };

    let probe = assemble(0);
    let text_offset = probe
        .layout
        .sections
        .iter()
        .find(|s| s.name == "__text")
        .and_then(|s| s.file_offset)
        .expect("__text was laid out with file bytes");

    assemble(text_offset)
}

/// Write the image and make it executable.
fn write_executable(tag: &str, image: &Image) -> Scratch {
    let scratch = Scratch::file(tag, &image.bytes).expect("writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    scratch
}

/// Ask the system whether it accepts the signature blinker produced.
fn verify_signature(path: &std::path::Path) -> Result<(), String> {
    let output = Command::new("codesign")
        .arg("-v")
        .arg(path)
        .output()
        .map_err(|e| format!("cannot run codesign: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

/// The whole point: the program runs and returns what it was built to return.
///
/// Nothing external touches the image between assembly and execution — no
/// `codesign`, no `install_name_tool`. What the builder emitted is what the
/// kernel loaded.
#[test]
fn a_blinker_linked_program_runs_and_returns_its_exit_status() {
    let image = build_program();
    let executable = write_executable("skeleton", &image);

    let output = Command::new(executable.path())
        .output()
        .unwrap_or_else(|e| panic!("the linked program could not be executed: {e}"));

    assert_eq!(
        output.status.code(),
        Some(EXPECTED_STATUS),
        "the program ran but returned the wrong status.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The system's own verifier must accept the signature blinker computed.
///
/// Running proves the kernel accepted it; `codesign -v` additionally checks
/// the blob's internal structure, so a signature that happened to satisfy the
/// loader while being malformed is caught here.
#[test]
fn the_system_verifier_accepts_blinkers_signature() {
    let image = build_program();
    let executable = write_executable("verify", &image);
    verify_signature(executable.path()).expect("codesign -v accepts the signature");
}

/// Corrupting one byte of code must make the image unrunnable.
///
/// Without this, every test above could pass with a signature nothing checks —
/// the hashes could be zeros and the program would still run if the kernel
/// were ignoring them. Flipping a byte inside the signed region proves the
/// signature is load-bearing.
#[test]
fn a_single_corrupted_code_byte_invalidates_the_image() {
    let image = build_program();
    let entry = image
        .layout
        .sections
        .iter()
        .find(|s| s.name == "__text")
        .and_then(|s| s.file_offset)
        .expect("__text laid out") as usize;

    let mut corrupted = image.bytes.clone();
    corrupted[entry] ^= 0xff;

    let scratch = Scratch::file("corrupt", &corrupted).expect("writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }

    assert!(
        verify_signature(scratch.path()).is_err(),
        "codesign accepted an image whose code had been modified"
    );

    match Command::new(scratch.path()).output() {
        Err(_) => {} // refused before exec
        Ok(output) => assert_ne!(
            output.status.code(),
            Some(EXPECTED_STATUS),
            "a corrupted image ran successfully — the signature is not being checked"
        ),
    }
}
