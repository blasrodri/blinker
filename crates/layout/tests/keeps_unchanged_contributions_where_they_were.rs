//! The invariant an incremental layout exists to hold: **an unchanged
//! contribution does not move because a different one changed.**
//!
//! Not "most of them usually don't". The previous mechanism reserved padding
//! and recomputed the layout from scratch, which holds this property when an
//! edit is small and quietly stops holding it when it is not — a fourteen-rlib
//! edit left 9 of 84 116 relocations valid (finding 94). The difference is that
//! an address is now *read back* from the previous layout rather than arrived
//! at again by the same arithmetic.
//!
//! These tests state the property directly, so a regression is a failing test
//! rather than a benchmark that got slower.

use blinker_layout::{
    compute_layout_reusing, compute_layout_with_slop, ContributionKey, ImageKind, InputPlacement,
    Layout, PreviousLayout, Slop,
};
use blinker_macho::{ObjectId, SectionId, SectionKind};

/// Identity is the object's ordinal here, standing in for the path and archive
/// member name a real link hashes. The layout crate never sees an `ObjectId`
/// through this — that is the point of the key.
fn key_of(input: &InputPlacement) -> ContributionKey {
    ContributionKey(u64::from(input.object.0) << 32 | u64::from(input.section.0))
}

/// These tests are about placement, not about dead-stripping: a contribution
/// occupies what it declares, so it needs no room reserved beyond the slop.
fn no_extra_room(_: &InputPlacement) -> u64 {
    0
}

fn contribution(object: u32, name: &str, kind: SectionKind, size: u64) -> InputPlacement {
    InputPlacement {
        object: ObjectId(object),
        section: SectionId(0),
        segment: "__TEXT".into(),
        name: name.into(),
        kind,
        size,
        alignment: 16,
    }
}

/// Four objects contributing code, as a Rust link's `__text` is built.
fn code(sizes: [u64; 4]) -> Vec<InputPlacement> {
    sizes
        .iter()
        .enumerate()
        .map(|(index, &size)| contribution(index as u32, "__text", SectionKind::Code, size))
        .collect()
}

fn address_of(layout: &Layout, object: u32) -> u64 {
    let section = layout
        .sections
        .iter()
        .find(|s| s.name == "__text")
        .expect("__text exists");
    section
        .contributions
        .iter()
        .find(|c| c.object == ObjectId(object))
        .map(|c| section.vm_address + c.offset)
        .unwrap_or_else(|| panic!("object {object} contributed nothing"))
}

fn record(layout: &Layout) -> PreviousLayout {
    PreviousLayout::record(layout, |object, section| {
        ContributionKey(u64::from(object.0) << 32 | u64::from(section.0))
    })
}

/// The property. One object's code grows; the three after it do not move.
#[test]
fn a_neighbour_growing_does_not_move_anything_else() {
    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );
    let previous = record(&first);

    // Object 1 gains 900 bytes — more than its own size class, and enough that
    // sequential packing would push everything after it along.
    let second = compute_layout_reusing(
        ImageKind::Executable,
        &code([1000, 2900, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
        &previous,
        &key_of,
        &no_extra_room,
    );

    for object in [0, 2, 3] {
        assert_eq!(
            address_of(&first, object),
            address_of(&second, object),
            "object {object} moved when object 1 grew"
        );
    }
}

/// The negative control. Without the previous layout, the same edit moves them
/// — so the test above is measuring reuse and not an accident of alignment.
#[test]
fn without_the_previous_layout_the_same_edit_moves_them() {
    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );
    let second = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2900, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );

    let moved = [2, 3]
        .iter()
        .filter(|&&object| address_of(&first, object) != address_of(&second, object))
        .count();
    assert_eq!(
        moved, 2,
        "a from-scratch layout absorbed the edit, so the reuse test proves nothing"
    );
}

/// A contribution that outgrows its reservation has to move. Everything else
/// still must not — this is the case padding could not handle, because there
/// the mover pushed its neighbours along.
#[test]
fn one_that_outgrows_its_slot_moves_alone() {
    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );
    let previous = record(&first);

    // Ten times its size: far past any reservation.
    let second = compute_layout_reusing(
        ImageKind::Executable,
        &code([1000, 20_000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
        &previous,
        &key_of,
        &no_extra_room,
    );

    assert_ne!(
        address_of(&first, 1),
        address_of(&second, 1),
        "object 1 could not have stayed — it no longer fits"
    );
    for object in [0, 2, 3] {
        assert_eq!(
            address_of(&first, object),
            address_of(&second, object),
            "object {object} moved out of the way of object 1, which it need not have"
        );
    }
}

/// Growth within the reservation keeps the address, which is the common case:
/// most edits change a function body without changing much of its size.
#[test]
fn growth_inside_the_reservation_keeps_the_address() {
    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );
    let previous = record(&first);
    let second = compute_layout_reusing(
        ImageKind::Executable,
        &code([1000, 2010, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
        &previous,
        &key_of,
        &no_extra_room,
    );

    for object in 0..4 {
        assert_eq!(
            address_of(&first, object),
            address_of(&second, object),
            "object {object} moved for a ten-byte edit"
        );
    }
}

/// A removed contribution leaves a hole, and a new one is allowed to use it —
/// otherwise every edit that replaces an object grows the image forever.
#[test]
fn a_removed_contribution_leaves_room_for_a_new_one() {
    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );
    let previous = record(&first);
    let end = |layout: &Layout| {
        layout
            .sections
            .iter()
            .find(|s| s.name == "__text")
            .map(|s| s.size)
            .expect("__text exists")
    };

    // Object 1 is gone; object 4 arrives at the same size.
    let mut inputs = code([1000, 2000, 3000, 4000]);
    inputs.remove(1);
    inputs.push(contribution(4, "__text", SectionKind::Code, 2000));

    let second = compute_layout_reusing(
        ImageKind::Executable,
        &inputs,
        0x1000,
        Slop::DEFAULT,
        &previous,
        &key_of,
        &no_extra_room,
    );

    assert_eq!(
        address_of(&first, 1),
        address_of(&second, 4),
        "the new contribution did not take the hole the removed one left"
    );
    assert_eq!(
        end(&first),
        end(&second),
        "the section grew even though nothing net was added"
    );
}

/// Sections whose shape carries meaning are repacked rather than retained.
///
/// `__eh_frame` is a chain walked by each record's length field: a hole in it
/// is not padding but a record header made of zeroes, and the program links,
/// runs, and dies the moment it unwinds. Keeping addresses is worth less than
/// this, so the allocator does not touch such sections at all.
#[test]
fn a_section_that_cannot_hold_holes_is_packed_from_the_start() {
    let unwind = |sizes: [u64; 3]| -> Vec<InputPlacement> {
        sizes
            .iter()
            .enumerate()
            .map(|(index, &size)| {
                contribution(index as u32, "__eh_frame", SectionKind::Unwind, size)
            })
            .collect()
    };

    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &unwind([400, 800, 1200]),
        0x1000,
        Slop::DEFAULT,
    );
    let previous = record(&first);
    let mut inputs = unwind([400, 800, 1200]);
    inputs.remove(1);
    let second = compute_layout_reusing(
        ImageKind::Executable,
        &inputs,
        0x1000,
        Slop::DEFAULT,
        &previous,
        &key_of,
        &no_extra_room,
    );

    let section = second
        .sections
        .iter()
        .find(|s| s.name == "__eh_frame")
        .expect("__eh_frame exists");
    let mut expected = 0u64;
    for contribution in &section.contributions {
        assert_eq!(
            contribution.offset, expected,
            "__eh_frame has a gap at offset {expected}, which is a zero-length record"
        );
        expected += contribution.size;
    }
    assert_eq!(section.size, expected, "__eh_frame has trailing padding");
}

/// Recording a layout and laying it out again with nothing changed must
/// reproduce it exactly. Without this, every relink drifts.
#[test]
fn relaying_out_unchanged_inputs_reproduces_the_layout() {
    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );
    let previous = record(&first);
    let second = compute_layout_reusing(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
        &previous,
        &key_of,
        &no_extra_room,
    );
    assert_eq!(
        first, second,
        "an unchanged relink produced a different layout"
    );
}

/// **The case that breaks the invariant, pinned as it currently behaves.**
///
/// An unchanged input can still change size. Dead-stripping decides how much
/// of an object is live, and liveness is not a property of that object — it is
/// a property of what reaches it. Edit one crate, and an object nobody touched
/// can have more of itself kept, grow past a capacity sized from last link's
/// smaller live size, and move.
///
/// On a real one-crate edit that is 48 contributions of byte-for-byte
/// unchanged files (finding 97), and every one invalidates every relocation
/// pointing at it.
///
/// This test asserts what happens *today*, not what should. Two things make
/// that worth committing rather than leaving as prose: it fails the moment
/// somebody fixes it, which is when the fix wants to be noticed; and it states
/// which of the two fixes is acceptable. Capacity reserved against the
/// object's full unstripped size would make it pass at a cost in image size
/// proportional to what stripping removes. Persisting liveness per
/// contribution would make it pass and remove the 4.6 ms the stage costs.
/// Widening the padding until this particular number goes green is finding 94
/// a second time, and would leave every other size delta failing silently.
#[test]
fn growth_from_liveness_moves_an_input_that_did_not_change() {
    let first = compute_layout_with_slop(
        ImageKind::Executable,
        &code([1000, 2000, 3000, 4000]),
        0x1000,
        Slop::DEFAULT,
    );
    let previous = record(&first);

    // Object 2's file is identical. Dead-stripping simply kept more of it,
    // because something that changed elsewhere now reaches further in.
    let second = compute_layout_reusing(
        ImageKind::Executable,
        &code([1000, 2000, 6000, 4000]),
        0x1000,
        Slop::DEFAULT,
        &previous,
        &key_of,
        &no_extra_room,
    );

    assert_ne!(
        address_of(&first, 2),
        address_of(&second, 2),
        "this now holds the invariant — delete the assertion and keep the one below"
    );
    // What does hold, and must keep holding through any fix: the mover moves
    // alone. A grown contribution taking a hole or the end is a bounded cost;
    // pushing its neighbours along is the unbounded one.
    for object in [0, 1, 3] {
        assert_eq!(
            address_of(&first, object),
            address_of(&second, object),
            "object {object} moved out of the way, which it need not have"
        );
    }
}
