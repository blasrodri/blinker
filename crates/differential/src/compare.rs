//! Comparing two linked images, property by property.
//!
//! # Why differences are graded rather than pass/fail
//!
//! blinker will not match `ld64` on everything for a long time, and a
//! comparison that reports "different" is useless when the answer is always
//! "yes, in 40 ways". What is needed instead is: *which* properties match
//! today, so a regression in an already-matching property is a failure while a
//! not-yet-implemented one is merely recorded.
//!
//! So each difference carries a [`Property`], and a test asserts over the
//! properties it claims. Widening the claim is a deliberate edit to a test,
//! which is exactly where that decision belongs.
//!
//! # What counts as agreement
//!
//! Not equality, for several properties:
//!
//! - **Addresses** are compared for *segments*, not sections. Section
//!   placement within a segment depends on ordering heuristics that differ
//!   between linkers without either being wrong.
//! - **Symbols** are compared as sets. Symbol table order is unconstrained.
//! - **Local symbol counts** are compared with a tolerance, because debug and
//!   compiler-local symbols vary with `-g` handling.

use crate::summary::ImageSummary;
use std::collections::BTreeSet;

/// A class of fact two linkers can agree or disagree on.
///
/// Ordered roughly from "must always hold" to "aspirational", which is also
/// the order in which blinker is expected to satisfy them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Property {
    /// Architecture and file type in the header.
    Identity,
    /// Header flag bits.
    HeaderFlags,
    /// Which `LC_*` commands are present.
    LoadCommandSet,
    /// The order they appear in.
    LoadCommandOrder,
    /// Which segments exist.
    SegmentSet,
    /// Their addresses, sizes and protections.
    SegmentPlacement,
    /// Which `segment,section` pairs exist.
    SectionSet,
    /// Their sizes.
    SectionSizes,
    /// Which libraries are loaded, and the dynamic linker path.
    Dependencies,
    /// The set of exported symbols.
    ExportedSymbols,
    /// The set of undefined symbols.
    UndefinedSymbols,
    /// Roughly how many local symbols there are.
    LocalSymbolCount,
    /// `LC_MAIN`'s entry offset.
    EntryPoint,
}

impl std::fmt::Display for Property {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// One disagreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub property: Property,
    /// What differs, in enough detail to act on.
    pub detail: String,
    pub reference: String,
    pub candidate: String,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}\n    reference: {}\n    candidate: {}",
            self.property, self.detail, self.reference, self.candidate
        )
    }
}

/// The result of comparing two images.
#[derive(Debug, Clone)]
pub struct Report {
    pub differences: Vec<Difference>,
}

impl Report {
    /// Differences in the given properties only.
    pub fn in_properties(&self, properties: &[Property]) -> Vec<&Difference> {
        self.differences
            .iter()
            .filter(|d| properties.contains(&d.property))
            .collect()
    }

    /// Which properties the two images agree on.
    pub fn matching(&self, properties: &[Property]) -> Vec<Property> {
        let differing: BTreeSet<_> = self.differences.iter().map(|d| d.property).collect();
        properties
            .iter()
            .copied()
            .filter(|p| !differing.contains(p))
            .collect()
    }

    /// Panic unless the two images agree on every one of `properties`.
    ///
    /// The message lists what differs, because a differential failure is
    /// useless without the specifics.
    pub fn require(&self, properties: &[Property]) {
        let failures = self.in_properties(properties);
        if failures.is_empty() {
            return;
        }
        let mut message = format!("{} difference(s) in required properties:\n", failures.len());
        for difference in failures {
            message.push_str(&format!("\n{difference}\n"));
        }
        panic!("{message}");
    }

    /// A human-readable summary of everything that differs.
    pub fn describe(&self) -> String {
        if self.differences.is_empty() {
            return "images agree on every compared property".to_string();
        }
        let mut out = String::new();
        for difference in &self.differences {
            out.push_str(&format!("{difference}\n\n"));
        }
        out
    }
}

/// How far the local symbol count may differ before it is reported.
///
/// Compiler-local and debug symbols vary with flags and toolchain version;
/// what matters is that one linker has not dropped the entire class.
const LOCAL_SYMBOL_TOLERANCE: f64 = 0.25;

/// Compare a candidate image against a reference one.
pub fn compare(reference: &ImageSummary, candidate: &ImageSummary) -> Report {
    let mut differences = Vec::new();
    let mut add = |property: Property, detail: String, r: String, c: String| {
        differences.push(Difference {
            property,
            detail,
            reference: r,
            candidate: c,
        });
    };

    if reference.cpu_type != candidate.cpu_type {
        add(
            Property::Identity,
            "cpu type".into(),
            reference.cpu_type.to_string(),
            candidate.cpu_type.to_string(),
        );
    }
    if reference.file_type != candidate.file_type {
        add(
            Property::Identity,
            "file type".into(),
            reference.file_type.to_string(),
            candidate.file_type.to_string(),
        );
    }
    if reference.flags != candidate.flags {
        add(
            Property::HeaderFlags,
            format!(
                "flags differ in bits {:#x}",
                reference.flags ^ candidate.flags
            ),
            format!("{:#010x}", reference.flags),
            format!("{:#010x}", candidate.flags),
        );
    }

    // Load commands: set first, then order. Reporting both separately means a
    // reordering does not masquerade as a missing command.
    let reference_commands: BTreeSet<_> = reference.load_commands.iter().cloned().collect();
    let candidate_commands: BTreeSet<_> = candidate.load_commands.iter().cloned().collect();
    for missing in reference_commands.difference(&candidate_commands) {
        add(
            Property::LoadCommandSet,
            format!("{missing} is missing"),
            "present".into(),
            "absent".into(),
        );
    }
    for extra in candidate_commands.difference(&reference_commands) {
        add(
            Property::LoadCommandSet,
            format!("{extra} is unexpected"),
            "absent".into(),
            "present".into(),
        );
    }
    if reference_commands == candidate_commands
        && reference.load_commands != candidate.load_commands
    {
        add(
            Property::LoadCommandOrder,
            "same commands in a different order".into(),
            reference.load_commands.join(", "),
            candidate.load_commands.join(", "),
        );
    }

    compare_segments(reference, candidate, &mut add);
    compare_sections(reference, candidate, &mut add);
    compare_dependencies(reference, candidate, &mut add);
    compare_symbols(reference, candidate, &mut add);

    if reference.entry_offset != candidate.entry_offset {
        add(
            Property::EntryPoint,
            "LC_MAIN entry offset".into(),
            describe_option(reference.entry_offset),
            describe_option(candidate.entry_offset),
        );
    }

    Report { differences }
}

fn describe_option(value: Option<u64>) -> String {
    match value {
        Some(v) => format!("{v:#x}"),
        None => "none".to_string(),
    }
}

fn compare_segments(
    reference: &ImageSummary,
    candidate: &ImageSummary,
    add: &mut impl FnMut(Property, String, String, String),
) {
    let reference_names: BTreeSet<_> = reference.segments.iter().map(|s| &s.name).collect();
    let candidate_names: BTreeSet<_> = candidate.segments.iter().map(|s| &s.name).collect();

    for missing in reference_names.difference(&candidate_names) {
        add(
            Property::SegmentSet,
            format!("segment {missing} is missing"),
            "present".into(),
            "absent".into(),
        );
    }
    for extra in candidate_names.difference(&reference_names) {
        add(
            Property::SegmentSet,
            format!("segment {extra} is unexpected"),
            "absent".into(),
            "present".into(),
        );
    }

    for reference_segment in &reference.segments {
        let Some(candidate_segment) = candidate
            .segments
            .iter()
            .find(|s| s.name == reference_segment.name)
        else {
            continue; // already reported as a set difference
        };

        if reference_segment.vm_address != candidate_segment.vm_address {
            add(
                Property::SegmentPlacement,
                format!("{} vm address", reference_segment.name),
                format!("{:#x}", reference_segment.vm_address),
                format!("{:#x}", candidate_segment.vm_address),
            );
        }
        if reference_segment.init_protection != candidate_segment.init_protection {
            add(
                Property::SegmentPlacement,
                format!("{} initial protection", reference_segment.name),
                format!("{:#x}", reference_segment.init_protection),
                format!("{:#x}", candidate_segment.init_protection),
            );
        }
        if reference_segment.max_protection != candidate_segment.max_protection {
            add(
                Property::SegmentPlacement,
                format!("{} maximum protection", reference_segment.name),
                format!("{:#x}", reference_segment.max_protection),
                format!("{:#x}", candidate_segment.max_protection),
            );
        }
    }
}

fn compare_sections(
    reference: &ImageSummary,
    candidate: &ImageSummary,
    add: &mut impl FnMut(Property, String, String, String),
) {
    let reference_names: BTreeSet<_> = reference
        .sections
        .iter()
        .map(|s| s.qualified_name())
        .collect();
    let candidate_names: BTreeSet<_> = candidate
        .sections
        .iter()
        .map(|s| s.qualified_name())
        .collect();

    for missing in reference_names.difference(&candidate_names) {
        add(
            Property::SectionSet,
            format!("section {missing} is missing"),
            "present".into(),
            "absent".into(),
        );
    }
    for extra in candidate_names.difference(&reference_names) {
        add(
            Property::SectionSet,
            format!("section {extra} is unexpected"),
            "absent".into(),
            "present".into(),
        );
    }

    for reference_section in &reference.sections {
        let Some(candidate_section) = candidate
            .sections
            .iter()
            .find(|s| s.qualified_name() == reference_section.qualified_name())
        else {
            continue;
        };
        if reference_section.size != candidate_section.size {
            add(
                Property::SectionSizes,
                format!("{} size", reference_section.qualified_name()),
                reference_section.size.to_string(),
                candidate_section.size.to_string(),
            );
        }
    }
}

fn compare_dependencies(
    reference: &ImageSummary,
    candidate: &ImageSummary,
    add: &mut impl FnMut(Property, String, String, String),
) {
    let reference_dylibs: BTreeSet<_> = reference.dylibs.iter().cloned().collect();
    let candidate_dylibs: BTreeSet<_> = candidate.dylibs.iter().cloned().collect();

    for missing in reference_dylibs.difference(&candidate_dylibs) {
        add(
            Property::Dependencies,
            format!("dylib {missing} is missing"),
            "present".into(),
            "absent".into(),
        );
    }
    for extra in candidate_dylibs.difference(&reference_dylibs) {
        add(
            Property::Dependencies,
            format!("dylib {extra} is unexpected"),
            "absent".into(),
            "present".into(),
        );
    }
    if reference.dynamic_linker != candidate.dynamic_linker {
        add(
            Property::Dependencies,
            "dynamic linker".into(),
            reference.dynamic_linker.clone().unwrap_or_default(),
            candidate.dynamic_linker.clone().unwrap_or_default(),
        );
    }
}

fn compare_symbols(
    reference: &ImageSummary,
    candidate: &ImageSummary,
    add: &mut impl FnMut(Property, String, String, String),
) {
    report_set_difference(
        Property::ExportedSymbols,
        "exported symbol",
        &reference.exported_symbols,
        &candidate.exported_symbols,
        add,
    );
    report_set_difference(
        Property::UndefinedSymbols,
        "undefined symbol",
        &reference.undefined_symbols,
        &candidate.undefined_symbols,
        add,
    );

    let reference_locals = reference.local_symbol_count as f64;
    let candidate_locals = candidate.local_symbol_count as f64;
    let allowed = (reference_locals * LOCAL_SYMBOL_TOLERANCE).max(4.0);
    if (reference_locals - candidate_locals).abs() > allowed {
        add(
            Property::LocalSymbolCount,
            format!("local symbol count differs by more than {allowed:.0}"),
            reference.local_symbol_count.to_string(),
            candidate.local_symbol_count.to_string(),
        );
    }
}

/// Report set differences, capped so one systematically-wrong image cannot
/// produce thousands of lines.
fn report_set_difference(
    property: Property,
    noun: &str,
    reference: &BTreeSet<String>,
    candidate: &BTreeSet<String>,
    add: &mut impl FnMut(Property, String, String, String),
) {
    const MAX_LISTED: usize = 20;

    let missing: Vec<_> = reference.difference(candidate).collect();
    if !missing.is_empty() {
        let listed: Vec<_> = missing
            .iter()
            .take(MAX_LISTED)
            .map(|s| s.as_str())
            .collect();
        let suffix = if missing.len() > MAX_LISTED {
            format!(" (and {} more)", missing.len() - MAX_LISTED)
        } else {
            String::new()
        };
        add(
            property,
            format!("{} {noun}(s) missing", missing.len()),
            format!("{}{suffix}", listed.join(", ")),
            "absent".into(),
        );
    }

    let extra: Vec<_> = candidate.difference(reference).collect();
    if !extra.is_empty() {
        let listed: Vec<_> = extra.iter().take(MAX_LISTED).map(|s| s.as_str()).collect();
        let suffix = if extra.len() > MAX_LISTED {
            format!(" (and {} more)", extra.len() - MAX_LISTED)
        } else {
            String::new()
        };
        add(
            property,
            format!("{} unexpected {noun}(s)", extra.len()),
            "absent".into(),
            format!("{}{suffix}", listed.join(", ")),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::{SectionSummary, SegmentSummary};

    fn empty() -> ImageSummary {
        ImageSummary {
            cpu_type: 0x0100_000c,
            cpu_subtype: 0,
            file_type: 2,
            flags: 0,
            load_commands: Vec::new(),
            segments: Vec::new(),
            sections: Vec::new(),
            dylibs: Vec::new(),
            dynamic_linker: None,
            entry_offset: None,
            exported_symbols: BTreeSet::new(),
            undefined_symbols: BTreeSet::new(),
            local_symbol_count: 0,
            file_size: 0,
        }
    }

    #[test]
    fn identical_images_produce_no_differences() {
        let image = empty();
        assert!(compare(&image, &image).differences.is_empty());
    }

    #[test]
    fn a_missing_segment_is_reported_once_not_per_field() {
        // Reporting the absent segment's address, size and protections as
        // three more differences would bury the actual problem.
        let mut reference = empty();
        reference.segments.push(SegmentSummary {
            name: "__TEXT".into(),
            vm_address: 0x1_0000_0000,
            vm_size: 0x4000,
            file_offset: 0,
            file_size: 0x4000,
            max_protection: 5,
            init_protection: 5,
            section_count: 1,
        });

        let report = compare(&reference, &empty());
        let segment_differences = report.in_properties(&[Property::SegmentSet]);
        assert_eq!(segment_differences.len(), 1);
        assert!(report
            .in_properties(&[Property::SegmentPlacement])
            .is_empty());
    }

    #[test]
    fn reordering_load_commands_is_not_reported_as_a_missing_command() {
        let mut reference = empty();
        reference.load_commands = vec!["LC_SEGMENT_64".into(), "LC_SYMTAB".into()];
        let mut candidate = empty();
        candidate.load_commands = vec!["LC_SYMTAB".into(), "LC_SEGMENT_64".into()];

        let report = compare(&reference, &candidate);
        assert!(report.in_properties(&[Property::LoadCommandSet]).is_empty());
        assert_eq!(report.in_properties(&[Property::LoadCommandOrder]).len(), 1);
    }

    #[test]
    fn symbol_sets_are_compared_regardless_of_order() {
        let mut reference = empty();
        reference.exported_symbols = ["_a", "_b"].iter().map(|s| s.to_string()).collect();
        let mut candidate = empty();
        candidate.exported_symbols = ["_b", "_a"].iter().map(|s| s.to_string()).collect();

        assert!(compare(&reference, &candidate).differences.is_empty());
    }

    #[test]
    fn a_missing_export_is_reported_with_its_name() {
        let mut reference = empty();
        reference.exported_symbols = ["_main"].iter().map(|s| s.to_string()).collect();

        let report = compare(&reference, &empty());
        let differences = report.in_properties(&[Property::ExportedSymbols]);
        assert_eq!(differences.len(), 1);
        assert!(
            differences[0].reference.contains("_main"),
            "the report must name the missing symbol: {}",
            differences[0].reference
        );
    }

    #[test]
    fn a_flood_of_symbol_differences_is_capped() {
        // A systematically wrong image must not produce an unreadable report.
        let mut reference = empty();
        reference.exported_symbols = (0..500).map(|i| format!("_sym{i}")).collect();

        let report = compare(&reference, &empty());
        let differences = report.in_properties(&[Property::ExportedSymbols]);
        assert_eq!(differences.len(), 1, "one summary line, not one per symbol");
        assert!(differences[0].detail.contains("500"));
        assert!(differences[0].reference.contains("and 480 more"));
    }

    #[test]
    fn small_local_symbol_differences_are_tolerated_but_large_ones_are_not() {
        let mut reference = empty();
        reference.local_symbol_count = 100;

        let mut close = empty();
        close.local_symbol_count = 110;
        assert!(compare(&reference, &close)
            .in_properties(&[Property::LocalSymbolCount])
            .is_empty());

        let mut far = empty();
        far.local_symbol_count = 0;
        assert_eq!(
            compare(&reference, &far)
                .in_properties(&[Property::LocalSymbolCount])
                .len(),
            1
        );
    }

    #[test]
    fn require_names_the_failing_property() {
        let mut reference = empty();
        reference.file_type = 2;
        let mut candidate = empty();
        candidate.file_type = 1;

        let report = compare(&reference, &candidate);
        let panic = std::panic::catch_unwind(|| report.require(&[Property::Identity]));
        let payload = panic.expect_err("require should panic");
        let message = payload
            .downcast_ref::<String>()
            .expect("panic message is a String");
        assert!(message.contains("Identity"), "{message}");
        assert!(message.contains("file type"), "{message}");
    }

    #[test]
    fn require_ignores_properties_it_was_not_asked_about() {
        // The point of grading: a not-yet-implemented property must not fail
        // a test that does not claim it.
        let mut reference = empty();
        reference.exported_symbols = ["_main"].iter().map(|s| s.to_string()).collect();

        let report = compare(&reference, &empty());
        report.require(&[Property::Identity]); // must not panic
        assert_eq!(
            report.matching(&[Property::Identity]),
            vec![Property::Identity]
        );
    }

    #[test]
    fn section_sizes_are_only_compared_for_sections_that_exist_in_both() {
        let mut reference = empty();
        reference.sections.push(SectionSummary {
            segment: "__TEXT".into(),
            name: "__text".into(),
            address: 0,
            size: 64,
            file_offset: 0,
            alignment: 2,
            flags: 0,
        });

        let report = compare(&reference, &empty());
        assert!(report.in_properties(&[Property::SectionSizes]).is_empty());
        assert_eq!(report.in_properties(&[Property::SectionSet]).len(), 1);
    }
}
