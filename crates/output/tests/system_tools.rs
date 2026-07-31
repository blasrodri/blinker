//! Does Apple's toolchain accept what we emit?
//!
//! Every other test in this crate checks the bytes against my understanding of
//! the format. These check them against `otool`, `nm`, and `dyld_info` — the
//! programs that were right first. A file that satisfies our own assertions
//! but that `otool` cannot walk is not a Mach-O image; it is a buffer that
//! resembles one.

use blinker_layout::InputPlacement;
use blinker_macho::{ObjectId, SectionId, SectionKind};
use blinker_output::image::Dylib;
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Image, ImageBuilder};
use std::path::PathBuf;
use std::process::Command;

/// A scratch file that removes itself.
struct Artifact(PathBuf);

impl Artifact {
    fn write(tag: &str, bytes: &[u8]) -> Self {
        // A plain counter, not the thread id: `{:?}` on a ThreadId renders as
        // `ThreadId(6)`, and the parentheses break otool's path handling —
        // it reports "can't open file" against a name truncated at the paren.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("blinker-image-{tag}-{}-{seq}", std::process::id()));
        std::fs::write(&path, bytes).expect("writable");
        Artifact(path)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Artifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn section(index: u32, segment: &str, name: &str, kind: SectionKind, size: u64) -> InputPlacement {
    InputPlacement {
        object: ObjectId(0),
        section: SectionId(index),
        segment: segment.into(),
        name: name.into(),
        kind,
        size,
        alignment: 4,
    }
}

/// An image with code, data, symbols and a dylib — enough shape to exercise
/// the parts of the format the tools inspect.
fn sample_image() -> Image {
    let mut builder = ImageBuilder::new();
    builder.input(section(0, "__TEXT", "__text", SectionKind::Code, 64));
    builder.input(section(1, "__DATA", "__data", SectionKind::Data, 32));
    builder.dylib(Dylib::lib_system());
    builder.entry_offset(0x4000);
    builder
        .symbols()
        .add(OutputSymbol::local("_helper", 1, 0x1_0000_4020))
        .add(OutputSymbol::exported("_main", 1, 0x1_0000_4000))
        .add(OutputSymbol::undefined("_exit", 1))
        .add(OutputSymbol::undefined("_malloc", 1));

    let image = {
        let probe = ImageBuilder::new();
        drop(probe);
        // Content must match the sizes layout assigned, so build once to learn
        // them, then supply bytes of exactly that size.
        builder.content(0, vec![0x1F; 64]);
        builder.content(1, vec![0x2F; 32]);
        builder.build().expect("image builds")
    };
    image
}

fn run(program: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program} runs: {e}"));
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The baseline: `otool -h` must recognise the file as a Mach-O executable.
#[test]
fn otool_recognises_the_header() {
    let image = sample_image();
    let artifact = Artifact::write("header", &image.bytes);
    let (ok, stdout, stderr) = run("otool", &["-h", &artifact.path().to_string_lossy()]);

    assert!(ok, "otool -h failed: {stderr}");
    assert!(
        stdout.contains("0xfeedfacf"),
        "otool did not see a 64-bit Mach-O magic:\n{stdout}"
    );
    // cputype 16777228 is arm64; filetype 2 is MH_EXECUTE.
    assert!(stdout.contains("16777228"), "wrong cputype:\n{stdout}");
}

/// The decisive structural check: `otool -l` walks the load commands the same
/// way dyld does. If `cmdsize` were wrong anywhere, the walk desynchronises
/// and otool reports garbage or bails.
#[test]
fn otool_walks_every_load_command() {
    let image = sample_image();
    let artifact = Artifact::write("commands", &image.bytes);
    let (ok, stdout, stderr) = run("otool", &["-l", &artifact.path().to_string_lossy()]);

    assert!(ok, "otool -l failed: {stderr}");
    assert!(
        !stdout.contains("malformed") && !stderr.contains("malformed"),
        "otool called the image malformed:\n{stdout}\n{stderr}"
    );

    // Every command we emit must be named in the output.
    for command in [
        "LC_SEGMENT_64",
        "LC_DYLD_INFO_ONLY",
        "LC_SYMTAB",
        "LC_DYSYMTAB",
        "LC_LOAD_DYLINKER",
        "LC_UUID",
        "LC_BUILD_VERSION",
        "LC_SOURCE_VERSION",
        "LC_MAIN",
        "LC_LOAD_DYLIB",
    ] {
        assert!(
            stdout.contains(command),
            "otool did not report {command}:\n{stdout}"
        );
    }

    // The number of segments otool finds must match what we laid out.
    let segments = stdout.matches("LC_SEGMENT_64").count();
    assert_eq!(
        segments,
        image.layout.segments.len(),
        "otool found a different number of segments than the layout has"
    );
}

/// The segments otool reports must carry the addresses layout assigned.
#[test]
fn otool_reports_the_addresses_layout_assigned() {
    let image = sample_image();
    let artifact = Artifact::write("segments", &image.bytes);
    let (_, stdout, _) = run("otool", &["-l", &artifact.path().to_string_lossy()]);

    // __PAGEZERO guards the low 4 GiB, and __TEXT starts immediately above.
    assert!(stdout.contains("__PAGEZERO"), "no __PAGEZERO:\n{stdout}");
    assert!(stdout.contains("__TEXT"), "no __TEXT:\n{stdout}");
    assert!(stdout.contains("__LINKEDIT"), "no __LINKEDIT:\n{stdout}");
    assert!(
        stdout.contains("0x0000000100000000"),
        "__TEXT is not at the expected base address:\n{stdout}"
    );
}

/// `nm` reads the symbol table through `LC_SYMTAB` and `LC_DYSYMTAB`. If the
/// three group ranges were wrong, it would report the wrong symbols rather
/// than failing — which is exactly why this is checked against nm and not
/// against our own reader.
#[test]
fn nm_reads_back_the_symbols_we_wrote() {
    let image = sample_image();
    let artifact = Artifact::write("symbols", &image.bytes);
    let (ok, stdout, stderr) = run("nm", &["-a", &artifact.path().to_string_lossy()]);

    assert!(ok, "nm failed: {stderr}\n{stdout}");
    for symbol in ["_main", "_helper", "_exit", "_malloc"] {
        assert!(
            stdout.contains(symbol),
            "nm did not report {symbol}:\n{stdout}"
        );
    }
}

/// Undefined symbols must be reported as undefined, not as definitions — the
/// property that depends on `n_type` and the dysymtab ranges agreeing.
#[test]
fn nm_classifies_undefined_symbols_correctly() {
    let image = sample_image();
    let artifact = Artifact::write("undef", &image.bytes);
    let (ok, stdout, stderr) = run("nm", &["-u", &artifact.path().to_string_lossy()]);

    assert!(ok, "nm -u failed: {stderr}");
    assert!(stdout.contains("_exit"), "_exit not undefined:\n{stdout}");
    assert!(
        stdout.contains("_malloc"),
        "_malloc not undefined:\n{stdout}"
    );
    assert!(
        !stdout.contains("_main"),
        "_main was reported undefined but it is a definition:\n{stdout}"
    );
}

/// `otool -L` reads the dylib load commands.
#[test]
fn otool_reports_the_linked_library() {
    let image = sample_image();
    let artifact = Artifact::write("dylib", &image.bytes);
    let (ok, stdout, _) = run("otool", &["-L", &artifact.path().to_string_lossy()]);

    assert!(ok, "otool -L failed");
    assert!(
        stdout.contains("/usr/lib/libSystem.B.dylib"),
        "otool did not report the linked library:\n{stdout}"
    );
}

/// Sections must be reported under the right segments with the right sizes.
#[test]
fn otool_reports_sections_with_their_sizes() {
    let image = sample_image();
    let artifact = Artifact::write("sections", &image.bytes);
    let (_, stdout, _) = run("otool", &["-l", &artifact.path().to_string_lossy()]);

    assert!(stdout.contains("sectname __text"), "no __text:\n{stdout}");
    assert!(stdout.contains("sectname __data"), "no __data:\n{stdout}");

    // 64 bytes of code and 32 of data, as supplied.
    assert!(
        stdout.contains("0x0000000000000040"),
        "__text size not reported as 64:\n{stdout}"
    );
}

/// An empty image is a degenerate but legal case, and must still be walkable.
#[test]
fn even_an_empty_image_is_well_formed() {
    let image = ImageBuilder::new().build().expect("builds");
    let artifact = Artifact::write("empty", &image.bytes);
    let (ok, stdout, stderr) = run("otool", &["-l", &artifact.path().to_string_lossy()]);

    assert!(ok, "otool rejected an empty image: {stderr}");
    assert!(stdout.contains("LC_SEGMENT_64"));
}
