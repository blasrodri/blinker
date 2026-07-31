//! The differential suite: blinker's output against the system linker's.
//!
//! # Calibration comes first
//!
//! A differential test that reports "no differences" is only meaningful if it
//! *can* report differences. Two of the tests here exist purely to establish
//! that:
//!
//! - linking the same program twice must produce no differences (so nothing
//!   that legitimately varies — UUIDs, timestamps — leaked into the summary);
//! - linking two *different* programs must produce differences (so the
//!   comparison is not vacuously passing).
//!
//! Without both, a summary that recorded nothing at all would pass the suite.

use blinker_differential::{compare, reference, summary, Property};

/// The smallest program that still links against libSystem.
fn trivial_case() -> reference::LinkCase {
    reference::LinkCase::new("trivial").source(
        "main.c",
        r#"
int main(void) { return 0; }
"#,
    )
}

/// A program with globals, a called function and a library call — enough shape
/// to produce several sections and a real symbol table.
fn substantial_case() -> reference::LinkCase {
    reference::LinkCase::new("substantial").source(
        "main.c",
        r#"
#include <stdio.h>
#include <stdlib.h>

static const char greeting[] = "differential";
static int counter = 0;
int shared_total = 0;

int accumulate(int n) {
    counter += n;
    shared_total += n;
    return counter;
}

int main(void) {
    for (int i = 0; i < 4; i++) accumulate(i);
    printf("%s %d\n", greeting, shared_total);
    void *p = malloc(32);
    free(p);
    return 0;
}
"#,
    )
}

/// Every property the suite knows how to compare.
const ALL_PROPERTIES: &[Property] = &[
    Property::Identity,
    Property::HeaderFlags,
    Property::LoadCommandSet,
    Property::LoadCommandOrder,
    Property::SegmentSet,
    Property::SegmentPlacement,
    Property::SectionSet,
    Property::SectionSizes,
    Property::Dependencies,
    Property::ExportedSymbols,
    Property::UndefinedSymbols,
    Property::LocalSymbolCount,
    Property::EntryPoint,
];

/// Calibration: the reference must be built for the target blinker will face.
///
/// The harness's first run reported that blinker emitted the wrong dyld
/// commands — `LC_DYLD_INFO_ONLY` where the reference had
/// `LC_DYLD_CHAINED_FIXUPS`. blinker was right. The reference was wrong,
/// because `cc` defaults to the running OS version (26.0) while `rustc`
/// defaults to 11.0, and the strategy flips at 12.0.
///
/// A differential harness calibrated against the wrong target does not merely
/// fail to catch bugs; it manufactures them, and the manufactured ones look
/// exactly as convincing as real ones. So the deployment target is pinned
/// against `rustc` itself rather than hardcoded and hoped over.
#[test]
fn the_reference_deployment_target_matches_rustc() {
    let output = std::process::Command::new("rustc")
        .arg("--print")
        .arg("deployment-target")
        .output()
        .expect("rustc runs");
    let text = String::from_utf8_lossy(&output.stdout);

    // Printed as `MACOSX_DEPLOYMENT_TARGET=11.0`.
    let rustc_target = text
        .trim()
        .rsplit('=')
        .next()
        .expect("a value after the =")
        .trim();

    assert_eq!(
        reference::DEPLOYMENT_TARGET,
        rustc_target,
        "the harness links its reference for macOS {} but rustc targets {} — \
         every comparison would be against the wrong dyld strategy",
        reference::DEPLOYMENT_TARGET,
        rustc_target
    );
}

/// The consequence of the pin: the reference uses the classic opcode streams.
///
/// This is the property the strategy choice in `blinker-output` depends on
/// (see its crate docs). If a toolchain update moved the cutover, this fails
/// and the choice gets revisited deliberately.
#[test]
fn the_reference_toolchain_emits_classic_dyld_info_at_this_target() {
    let built = reference::build(&trivial_case()).expect("links");
    let image = summary::summarize_file(&built.image).expect("readable");

    assert!(
        image.load_commands.iter().any(|c| c == "LC_DYLD_INFO_ONLY"),
        "expected classic opcode streams at macOS {}, got: {}",
        reference::DEPLOYMENT_TARGET,
        image.load_commands.join(", ")
    );
    assert!(
        !image
            .load_commands
            .iter()
            .any(|c| c == "LC_DYLD_CHAINED_FIXUPS"),
        "chained fixups appeared at macOS {} — the cutover moved",
        reference::DEPLOYMENT_TARGET
    );
}

/// And the other side of the cutover still behaves as documented.
///
/// Pins the *reason* the target is pinned. Without this, the table in
/// [`reference::DEPLOYMENT_TARGET`] is an unverified claim.
#[test]
fn raising_the_deployment_target_switches_to_chained_fixups() {
    let case = trivial_case().deployment_target("12.0");
    let built = reference::build(&case).expect("links");
    let image = summary::summarize_file(&built.image).expect("readable");

    assert!(
        image
            .load_commands
            .iter()
            .any(|c| c == "LC_DYLD_CHAINED_FIXUPS"),
        "expected chained fixups at macOS 12.0, got: {}",
        image.load_commands.join(", ")
    );
}

/// Calibration: the same program linked twice must compare equal.
///
/// This is what proves the summary excludes everything that legitimately
/// varies between two runs of one linker. If `LC_UUID`'s bytes or a build
/// timestamp were being compared, this test would fail — and it would fail
/// intermittently, which is why it is pinned explicitly rather than assumed.
#[test]
fn linking_the_same_program_twice_produces_no_differences() {
    let case = substantial_case();
    let first = reference::build(&case).expect("system toolchain links the case");
    let second = reference::build(&case).expect("system toolchain links the case again");

    let a = summary::summarize_file(&first.image).expect("first image is readable");
    let b = summary::summarize_file(&second.image).expect("second image is readable");

    let report = compare::compare(&a, &b);
    assert!(
        report.differences.is_empty(),
        "two links of the same program disagreed — the summary is comparing \
         something that legitimately varies:\n{}",
        report.describe()
    );
}

/// Calibration: two different programs must compare *unequal*.
///
/// Without this, a summary that recorded nothing would pass the test above.
#[test]
fn linking_two_different_programs_produces_differences() {
    let trivial = reference::build(&trivial_case()).expect("links");
    let substantial = reference::build(&substantial_case()).expect("links");

    let a = summary::summarize_file(&trivial.image).expect("readable");
    let b = summary::summarize_file(&substantial.image).expect("readable");

    let report = compare::compare(&a, &b);
    assert!(
        !report.differences.is_empty(),
        "two different programs compared equal — the comparison is vacuous"
    );

    // Specifically, the substantial program calls into libSystem for printf
    // and malloc, so its undefined symbols must differ.
    assert!(
        !report
            .in_properties(&[Property::UndefinedSymbols])
            .is_empty(),
        "the undefined symbol sets should differ:\n{}",
        report.describe()
    );
}

/// The `-###` capture must yield a usable link line.
///
/// This is the mechanism by which blinker gets handed the *same* request ld64
/// received. If it silently returned the compile line instead, every later
/// comparison would be against the wrong thing.
#[test]
fn the_captured_link_argv_is_a_real_link_command() {
    let built = reference::build(&substantial_case()).expect("links");

    assert!(
        !built.link_argv.is_empty(),
        "no link command was captured from `cc -###`"
    );

    let program = &built.link_argv[0];
    assert!(
        program.contains("ld"),
        "captured command does not look like a linker: {program}"
    );

    // Our object files must appear in it, or we captured someone else's link.
    for object in &built.objects {
        let name = object.to_string_lossy().into_owned();
        assert!(
            built.link_argv.contains(&name),
            "captured link line does not mention {name}:\n{:?}",
            built.link_argv
        );
    }

    // And the driver-supplied arguments blinker must handle should be present.
    for expected in ["-o", "-arch", "-platform_version"] {
        assert!(
            built.link_argv.iter().any(|a| a == expected),
            "captured link line is missing {expected}:\n{:?}",
            built.link_argv
        );
    }
}

/// A summary of a real linked image must be non-trivial.
///
/// Guards against the summary reader silently producing an empty record —
/// which would make every comparison pass.
#[test]
fn a_real_image_summarizes_to_something_substantial() {
    let built = reference::build(&substantial_case()).expect("links");
    let image = summary::summarize_file(&built.image).expect("readable");

    assert_eq!(image.file_type, 2, "MH_EXECUTE");
    assert_eq!(image.cpu_type, 0x0100_000c, "arm64");
    assert!(
        image.segments.len() >= 3,
        "expected at least __PAGEZERO, __TEXT, __LINKEDIT: {:?}",
        image.segments.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        image.sections.iter().any(|s| s.name == "__text"),
        "no __text section"
    );
    assert!(
        image.dylibs.iter().any(|d| d.contains("libSystem")),
        "libSystem not linked: {:?}",
        image.dylibs
    );
    assert!(image.entry_offset.is_some(), "no LC_MAIN");
    assert!(
        image.undefined_symbols.iter().any(|s| s.contains("printf")),
        "printf not undefined: {:?}",
        image.undefined_symbols
    );
}

/// What ld64 actually produces, recorded so blinker can be built toward it.
///
/// Not an assertion about blinker — a *measurement*, printed with
/// `--nocapture`. Several of blinker's earlier corrections came from reading
/// exactly this kind of output rather than reasoning about the format.
#[test]
fn record_what_ld64_produces_for_a_trivial_program() {
    let built = reference::build(&trivial_case()).expect("links");
    let image = summary::summarize_file(&built.image).expect("readable");

    println!("\n=== ld64 output for a trivial program ===");
    println!("flags: {:#010x}", image.flags);
    println!("load commands: {}", image.load_commands.join(", "));
    println!("\nsegments:");
    for segment in &image.segments {
        println!(
            "  {:<12} vm {:#012x}+{:<#8x} file {:#8x}+{:<#8x} prot {}/{}",
            segment.name,
            segment.vm_address,
            segment.vm_size,
            segment.file_offset,
            segment.file_size,
            segment.init_protection,
            segment.max_protection
        );
    }
    println!("\nsections:");
    for section in &image.sections {
        println!(
            "  {:<24} addr {:#012x} size {:<#8x} align 2^{} flags {:#010x}",
            section.qualified_name(),
            section.address,
            section.size,
            section.alignment,
            section.flags
        );
    }
    println!("\ndylibs: {:?}", image.dylibs);
    println!("dynamic linker: {:?}", image.dynamic_linker);
    println!("entry offset: {:?}", image.entry_offset);
    println!("exported: {:?}", image.exported_symbols);
    println!("undefined: {:?}", image.undefined_symbols);
    println!("locals: {}", image.local_symbol_count);
    println!("file size: {}", image.file_size);
    println!("\nlink argv:");
    for arg in &built.link_argv {
        println!("  {arg}");
    }
}

/// The gap between blinker's current output and ld64's, as a measurement.
///
/// blinker cannot yet link real object files, so this compares ld64's image
/// against one assembled by hand through `ImageBuilder` — the closest thing
/// blinker can currently produce. What it reports is the M2 to-do list,
/// derived rather than guessed.
#[test]
fn measure_the_gap_between_blinker_and_ld64() {
    use blinker_layout::InputPlacement;
    use blinker_macho::{ObjectId, SectionId, SectionKind};
    use blinker_output::image::Dylib;
    use blinker_output::ImageBuilder;

    let built = reference::build(&trivial_case()).expect("links");
    let ld64 = summary::summarize_file(&built.image).expect("readable");

    // Mirror the reference's shape as closely as the builder allows.
    let mut builder = ImageBuilder::new();
    let text_size = ld64
        .sections
        .iter()
        .find(|s| s.name == "__text")
        .map(|s| s.size)
        .unwrap_or(64);
    builder.input(InputPlacement {
        object: ObjectId(0),
        section: SectionId(0),
        segment: "__TEXT".into(),
        name: "__text".into(),
        kind: SectionKind::Code,
        size: text_size,
        alignment: 4,
    });
    builder.dylib(Dylib::lib_system());
    builder.entry_offset(ld64.entry_offset.unwrap_or(0x4000));
    builder.content(0, vec![0u8; text_size as usize]);

    let image = builder.build().expect("blinker assembles an image");
    let blinker = summary::summarize(&image.bytes).expect("blinker's image is readable");

    let report = compare::compare(&ld64, &blinker);

    println!("\n=== blinker vs ld64, trivial program ===");
    println!("matching properties: {:?}", report.matching(ALL_PROPERTIES));
    println!("\n{}", report.describe());

    // The claim blinker makes today. Widening this list is the point of the
    // remaining M2 work, and each addition is a deliberate edit here.
    report.require(&[Property::Identity]);
}
