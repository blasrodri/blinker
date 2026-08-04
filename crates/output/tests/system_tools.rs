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
use blinker_test_support::Scratch;
use std::process::Command;

/// Write an image to a scratch file for a tool to inspect.
fn artifact(tag: &str, bytes: &[u8]) -> Scratch {
    Scratch::file(tag, bytes).expect("writable")
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
    let artifact = artifact("header", &image.bytes);
    let (ok, stdout, stderr) = run("otool", &["-h", &artifact.as_str()]);

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
    let artifact = artifact("commands", &image.bytes);
    let (ok, stdout, stderr) = run("otool", &["-l", &artifact.as_str()]);

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
    let artifact = artifact("segments", &image.bytes);
    let (_, stdout, _) = run("otool", &["-l", &artifact.as_str()]);

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
    let artifact = artifact("symbols", &image.bytes);
    let (ok, stdout, stderr) = run("nm", &["-a", &artifact.as_str()]);

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
    let artifact = artifact("undef", &image.bytes);
    let (ok, stdout, stderr) = run("nm", &["-u", &artifact.as_str()]);

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
    let artifact = artifact("dylib", &image.bytes);
    let (ok, stdout, _) = run("otool", &["-L", &artifact.as_str()]);

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
    let artifact = artifact("sections", &image.bytes);
    let (_, stdout, _) = run("otool", &["-l", &artifact.as_str()]);

    assert!(stdout.contains("sectname __text"), "no __text:\n{stdout}");
    assert!(stdout.contains("sectname __data"), "no __data:\n{stdout}");

    // 64 bytes of code and 32 of data, as supplied.
    assert!(
        stdout.contains("0x0000000000000040"),
        "__text size not reported as 64:\n{stdout}"
    );
}

/// The same shape, built as a dynamic library.
fn sample_dylib(install_name: &str) -> Image {
    let mut builder = ImageBuilder::new();
    builder.dylib_output(install_name);
    builder.input(section(0, "__TEXT", "__text", SectionKind::Code, 64));
    builder.input(section(1, "__DATA", "__data", SectionKind::Data, 32));
    builder.dylib(Dylib::lib_system());
    builder
        .symbols()
        // Addresses start at zero here, not above __PAGEZERO: that is the
        // difference this whole image kind is about.
        .add(OutputSymbol::exported("_answer", 1, 0x4000))
        .add(OutputSymbol::undefined("_malloc", 1));
    builder.content(0, vec![0x1F; 64]);
    builder.content(1, vec![0x2F; 32]);
    builder.build().expect("dylib builds")
}

/// A dylib is a different *file type*, and `otool` is the one that says so.
///
/// Four things distinguish it, and each is a way to produce a file that walks
/// perfectly and does not load: the filetype itself, the absent `__PAGEZERO`,
/// the base address that follows from it, and `LC_ID_DYLIB` in place of the
/// `LC_MAIN` that would claim this library is a program.
#[test]
fn otool_sees_a_dylib_as_a_dylib() {
    let image = sample_dylib("/usr/local/lib/libsample.dylib");
    let artifact = artifact("libsample", &image.bytes);

    let (ok, header, stderr) = run("otool", &["-hv", &artifact.as_str()]);
    assert!(ok, "otool -h failed: {stderr}");
    assert!(
        header.contains("DYLIB"),
        "not reported as a dylib:\n{header}"
    );
    // The flags a real `cc -dynamiclib` carries, and not MH_PIE.
    assert!(
        header.contains("NOUNDEFS DYLDLINK TWOLEVEL NO_REEXPORTED_DYLIBS"),
        "the flags are not the ones ld64 writes:\n{header}"
    );
    assert!(
        !header.contains("PIE"),
        "a dylib must not claim PIE:\n{header}"
    );

    let (ok, commands, stderr) = run("otool", &["-l", &artifact.as_str()]);
    assert!(ok, "otool -l failed: {stderr}");
    assert!(
        !commands.contains("malformed") && !stderr.contains("malformed"),
        "otool called the dylib malformed:\n{commands}\n{stderr}"
    );
    assert!(
        commands.contains("LC_ID_DYLIB"),
        "no LC_ID_DYLIB, so nothing could record what to load:\n{commands}"
    );
    assert!(
        commands.contains("/usr/local/lib/libsample.dylib"),
        "LC_ID_DYLIB does not carry the install name:\n{commands}"
    );
    assert!(
        !commands.contains("LC_MAIN"),
        "a dylib with an entry point is a program dyld will refuse:\n{commands}"
    );
    assert!(
        !commands.contains("LC_LOAD_DYLINKER"),
        "a dylib does not name the dynamic linker:\n{commands}"
    );
    assert!(
        !commands.contains("__PAGEZERO"),
        "the low 4 GiB is not this library's to claim:\n{commands}"
    );
    // __TEXT therefore starts at zero: every address in the image is an offset
    // from wherever dyld maps it.
    assert!(
        commands.contains("segname __TEXT") && commands.contains("vmaddr 0x0000000000000000"),
        "__TEXT is not based at zero:\n{commands}"
    );

    assert!(
        image.layout.segment("__PAGEZERO").is_none(),
        "the layout still holds a __PAGEZERO"
    );
    let segments = commands.matches("LC_SEGMENT_64").count();
    assert_eq!(
        segments,
        image.layout.segments.len(),
        "otool found a different number of segments than the layout has"
    );
}

/// `dyld_info` reads `__LINKEDIT` the way dyld does, and complains about
/// content it cannot use — "mis-aligned LINKEDIT content" is how it reports a
/// stream placed where dyld will silently ignore it.
///
/// It is *not* the oracle for what the trie contains: with no trie at all it
/// falls back to the symbol table and prints the same names, so a passing
/// `-exports` here would say nothing about whether a trie was written. That
/// question belongs to `a_dylib_exports_what_it_defines.rs`, which decodes the
/// bytes.
#[test]
fn dyld_info_reads_the_dylibs_linkedit() {
    let image = sample_dylib("/usr/local/lib/libsample.dylib");
    let artifact = artifact("libexports", &image.bytes);
    let (ok, stdout, stderr) = run("dyld_info", &["-exports", &artifact.as_str()]);

    assert!(ok, "dyld_info failed: {stderr}\n{stdout}");
    assert!(
        !stdout.contains("mis-aligned") && !stderr.contains("mis-aligned"),
        "dyld_info cannot use the __LINKEDIT content:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("_answer"),
        "the exported symbol is not there at all:\n{stdout}"
    );
}

/// An empty image is a degenerate but legal case, and must still be walkable.
#[test]
fn even_an_empty_image_is_well_formed() {
    let image = ImageBuilder::new().build().expect("builds");
    let artifact = artifact("empty", &image.bytes);
    let (ok, stdout, stderr) = run("otool", &["-l", &artifact.as_str()]);

    assert!(ok, "otool rejected an empty image: {stderr}");
    assert!(stdout.contains("LC_SEGMENT_64"));
}
