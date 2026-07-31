//! Emit the smallest program blinker can produce, for inspection by hand.
//!
//! The walking-skeleton test asserts this image runs, but a test that passes
//! is not the same as a file you have looked at. This writes the same image to
//! a path of your choosing so `otool`, `codesign -dv`, `dyld_info` and the
//! kernel can each be pointed at it directly.
//!
//! ```text
//! cargo run -p blinker-output --example skeleton -- /tmp/skeleton
//! codesign -s - -f /tmp/skeleton && /tmp/skeleton; echo "exit: $?"
//! ```

use blinker_layout::InputPlacement;
use blinker_macho::{ObjectId, SectionId, SectionKind};
use blinker_output::image::Dylib;
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Image, ImageBuilder};

/// `mov w0, #42` then `ret`.
fn program_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(0x5280_0000u32 | (42 << 5)).to_le_bytes());
    bytes.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
    bytes
}

fn assemble(entry_offset: u64, code: &[u8]) -> Image {
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
    builder.content(0, code.to_vec());
    builder.symbols().add(OutputSymbol::exported(
        "__mh_execute_header",
        1,
        0x1_0000_0000,
    ));
    builder.build().expect("image assembles")
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: skeleton <output-path>");
        std::process::exit(2);
    });

    let code = program_bytes();
    // Two passes: LC_MAIN's entryoff is not known until __text is placed.
    let probe = assemble(0, &code);
    let text_offset = probe
        .layout
        .sections
        .iter()
        .find(|s| s.name == "__text")
        .and_then(|s| s.file_offset)
        .expect("__text laid out");

    let image = assemble(text_offset, &code);
    std::fs::write(&path, &image.bytes).expect("writable");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    println!("wrote {} ({} bytes)", path, image.bytes.len());
    println!("entry offset: {text_offset:#x}");
    println!("sign and run:  codesign -s - -f {path} && {path}; echo \"exit: $?\"");
}
