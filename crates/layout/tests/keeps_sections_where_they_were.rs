//! One section changing size must not move the sections after it.
//!
//! This is the half of "stable layout" that was missing. Contribution slop
//! keeps an object's bytes at the same offset *within* an output section when
//! its neighbours change. But sections are laid end to end, so a section that
//! changes size at all slides every section after it — and with them every
//! symbol those sections contain.
//!
//! Measured on a real link before the fix: a one-line edit to one crate of
//! sixty changed `__text` by 768 bytes, and the nine sections following it
//! moved by exactly 768 bytes each while their own sizes did not change by a
//! byte. 57% of the symbols in the image moved. The cache matches an object's
//! cached bytes by where they landed, so almost nothing matched: **9 of 83 687
//! relocations were reused.**
//!
//! The fix pads the gap *between* sections rather than the sections
//! themselves, which is what makes it safe — nothing reads the gap, so no
//! section's size or internal structure changes. That matters: a padded
//! `__eh_frame` dies in the unwinder, and dyld rejects a `__thread_vars` whose
//! size is not a multiple of its record size.

use blinker_layout::{compute_layout_with_slop, InputPlacement, Layout, Slop};
use blinker_macho::{ObjectId, SectionId, SectionKind};

fn section(object: u32, name: &str, kind: SectionKind, size: u64) -> InputPlacement {
    InputPlacement {
        object: ObjectId(object),
        section: SectionId(0),
        segment: "__TEXT".into(),
        name: name.into(),
        kind,
        size,
        alignment: 4,
    }
}

/// A link with code, then three sections after it that an edit must not move.
fn inputs(text_size: u64) -> Vec<InputPlacement> {
    vec![
        section(0, "__text", SectionKind::Code, text_size),
        section(1, "__const", SectionKind::Data, 4096),
        section(2, "__cstring", SectionKind::ReadOnlyData, 2048),
        section(3, "__eh_frame", SectionKind::Unwind, 8192),
    ]
}

fn address_of(layout: &Layout, name: &str) -> u64 {
    layout
        .sections
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name} in the layout"))
        .vm_address
}

/// The property: growing `__text` inside its stride leaves everything after it
/// exactly where it was.
#[test]
fn an_edit_within_the_stride_moves_nothing_after_it() {
    let before = compute_layout_with_slop(&inputs(100_000), 0x1000, Slop::DEFAULT);
    // 768 bytes: the size change a real one-crate edit produced.
    let after = compute_layout_with_slop(&inputs(100_768), 0x1000, Slop::DEFAULT);

    for name in ["__const", "__cstring", "__eh_frame"] {
        assert_eq!(
            address_of(&before, name),
            address_of(&after, name),
            "{name} moved when __text grew by 768 bytes"
        );
    }
}

/// And the negative control: without a stable layout there is no padding, so
/// the same edit *does* move them.
///
/// Without this the test above would pass on a layout that happened to place
/// these sections at aligned addresses for unrelated reasons.
#[test]
fn without_a_stable_layout_the_same_edit_moves_them_all() {
    let before = compute_layout_with_slop(&inputs(100_000), 0x1000, Slop::NONE);
    let after = compute_layout_with_slop(&inputs(100_768), 0x1000, Slop::NONE);

    let moved = ["__const", "__cstring", "__eh_frame"]
        .iter()
        .filter(|name| address_of(&before, name) != address_of(&after, name))
        .count();
    assert_eq!(
        moved, 3,
        "an unpadded layout absorbed an edit, so the test above proves nothing"
    );
}

/// A change larger than the stride does move things — stated so the limit is
/// recorded rather than discovered later.
///
/// This is not a defect being papered over: reserving enough slack to absorb
/// an arbitrary edit means reserving it everywhere, and there are thousands of
/// contributions in a real link. The stride buys the common case; the general
/// case needs the previous layout to be *reused* rather than recomputed, which
/// is a different mechanism.
#[test]
fn a_change_larger_than_the_stride_still_moves_what_follows() {
    let before = compute_layout_with_slop(&inputs(100_000), 0x1000, Slop::DEFAULT);
    let after = compute_layout_with_slop(&inputs(140_000), 0x1000, Slop::DEFAULT);
    assert_ne!(
        address_of(&before, "__const"),
        address_of(&after, "__const"),
        "a 40 KB change was absorbed, which would mean far more padding than \
         intended is being reserved"
    );
}

/// Padding costs space, and the cost must stay bounded by the stride per
/// section rather than growing with the content.
#[test]
fn the_padding_costs_at_most_one_stride_per_section() {
    let unpadded = compute_layout_with_slop(&inputs(100_000), 0x1000, Slop::NONE);
    let padded = compute_layout_with_slop(&inputs(100_000), 0x1000, Slop::DEFAULT);

    let end = |layout: &Layout| {
        layout
            .sections
            .iter()
            .map(|s| s.vm_address + s.size)
            .max()
            .expect("sections exist")
    };
    let overhead = end(&padded) - end(&unpadded);
    // Four sections, and contribution slop adds its own on top, so the bound
    // is stated generously; what it rules out is padding that scales with the
    // section's size.
    assert!(
        overhead < 4 * 4096 + 4096,
        "padding cost {overhead} bytes for four sections, which is more than \
         a stride each"
    );
}
