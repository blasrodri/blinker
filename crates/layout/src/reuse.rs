//! Laying out an edit on top of the layout the previous link produced.
//!
//! Why this is not more padding
//! ---------------------------
//!
//! The previous attempt at stable layout reserved slack — a little after each
//! contribution, 4 KiB between sections — and recomputed everything from
//! scratch in the hope that the addresses came out the same. It works when an
//! edit is small enough and fails silently when it is not: one edit to a crate
//! everything depends on changed fourteen rlibs, their combined size delta
//! walked straight through the 4 KiB stride, and **9 of 84 116 relocations**
//! survived (finding 94). Padding buys a probability, not a property.
//!
//! What is here instead is an allocator that *reads* the previous placement:
//! a contribution that has not changed keeps the exact offset it had, because
//! it is told to, and not because the arithmetic happened to land there. Only
//! what changed is placed, and it is placed into the room the previous layout
//! left — its own reservation if it still fits, a hole left by something
//! removed, or the end of the section.
//!
//! Identity, not position
//! ----------------------
//!
//! [`ObjectId`] is assigned by input order and archive extraction round, so it
//! names a different object between two links the moment an archive member
//! stops being pulled in. It cannot be the key. The key is supplied by the
//! caller, which knows the object's path, its archive member name and its
//! section — the things that survive a relink — and hashes them into a
//! [`ContributionKey`].
//!
//! This is deliberately the *only* thing this module knows about identity: it
//! never looks at an `ObjectId`, so it cannot accidentally depend on one.
//!
//! History dependence is the point
//! -------------------------------
//!
//! An image laid out this way is not the image a cold link produces from the
//! same inputs — an edit that shrank a function leaves a hole a cold link
//! would never have. That is D5, adopted rather than tolerated: a cold link
//! stays byte-deterministic and is what a reproducible build asks for, and an
//! incremental link is semantically equivalent and keeps the addresses it can.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::align_up;

/// A contribution's identity across links.
///
/// A hash rather than the parts, because the table is stored in the cache and
/// read on every link: the parts are three strings, and there are thousands of
/// contributions. Collisions place two contributions in one slot, which the
/// caller catches — a slot is claimed once, and a second claim on it allocates
/// fresh (see [`PreviousLayout::take`]).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ContributionKey(pub u64);

/// Where one contribution sat, and how much room it was given.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviousSlot {
    /// Qualified output section name, `__SEGMENT,__section`.
    pub section: String,
    /// Offset within that output section.
    pub offset: u64,
    /// Bytes it may grow into without moving: its own size plus whatever
    /// padding followed it.
    pub capacity: u64,
}

/// Where one output section sat, and how much room it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviousSection {
    pub vm_address: u64,
    pub file_offset: Option<u64>,
    /// Bytes the section may grow to before it must move.
    pub reserved: u64,
}

/// The previous link's layout, in the form the next one consumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviousLayout {
    /// By qualified name, so a section is matched by what it is rather than by
    /// where it happened to be in the section list.
    pub sections: BTreeMap<String, PreviousSection>,
    pub slots: HashMap<ContributionKey, PreviousSlot>,
}

impl PreviousLayout {
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.sections.is_empty()
    }

    /// Read a finished layout back into the table the next link consumes.
    ///
    /// Capacity is *derived* rather than declared: a contribution's room is
    /// the distance to whatever the previous link put after it, and a section's
    /// room is the distance to the next section in its segment. So a layout
    /// laid out with padding records the padding as capacity, one laid out
    /// without records none, and the allocator does not need to be told which
    /// kind it is looking at.
    pub fn record(
        layout: &crate::Layout,
        key_of: impl Fn(blinker_macho::ObjectId, blinker_macho::SectionId) -> ContributionKey,
    ) -> PreviousLayout {
        let mut sections = BTreeMap::new();
        let mut slots = HashMap::new();

        for (index, section) in layout.sections.iter().enumerate() {
            let qualified = section.qualified_name();
            // The next section in the same segment bounds this one's growth.
            // Across a segment boundary there is page alignment and then a new
            // base, so the section's own size is all it may claim.
            let reserved = layout
                .sections
                .get(index + 1)
                .filter(|next| next.segment == section.segment)
                .map_or(section.size, |next| {
                    next.vm_address.saturating_sub(section.vm_address)
                });
            sections.insert(
                qualified.clone(),
                PreviousSection {
                    vm_address: section.vm_address,
                    file_offset: section.file_offset,
                    reserved: reserved.max(section.size),
                },
            );

            let mut ordered: Vec<&crate::Contribution> = section.contributions.iter().collect();
            ordered.sort_unstable_by_key(|c| c.offset);
            for (position, contribution) in ordered.iter().enumerate() {
                let next = ordered
                    .get(position + 1)
                    .map_or(section.size, |next| next.offset);
                slots.insert(
                    key_of(contribution.object, contribution.section),
                    PreviousSlot {
                        section: qualified.clone(),
                        offset: contribution.offset,
                        capacity: next
                            .saturating_sub(contribution.offset)
                            .max(contribution.size),
                    },
                );
            }
        }
        PreviousLayout { sections, slots }
    }

    /// The room a slot was given, for building the occupancy map.
    pub fn capacity(&self, key: ContributionKey) -> Option<u64> {
        self.slots.get(&key).map(|slot| slot.capacity)
    }

    /// Where a section sat, if it is still the same section.
    pub fn section(&self, qualified: &str) -> Option<&PreviousSection> {
        self.sections.get(qualified)
    }

    /// The slot for `key`, if it belongs to `section` and still fits `size`.
    ///
    /// All three conditions matter and each has been a bug in some linker:
    /// a contribution that moved to a different output section (its kind
    /// changed) cannot keep an offset measured in the old one; one that grew
    /// past its reservation cannot stay without overwriting its neighbour.
    pub fn slot(&self, key: ContributionKey, section: &str, size: u64) -> Option<u64> {
        let slot = self.slots.get(&key)?;
        (slot.section == section && size <= slot.capacity).then_some(slot.offset)
    }
}

/// Free space inside one output section, as the allocator sees it.
///
/// Built by subtracting the slots that kept their addresses from the section's
/// previous extent. Everything beyond that extent is the tail, which is
/// unbounded — a section may always grow at the end, at the cost of moving the
/// sections after it if it outgrows its own reservation.
#[derive(Debug)]
pub struct FreeSpace {
    /// Disjoint `(start, end)` holes, in address order.
    holes: Vec<(u64, u64)>,
    /// Where the unbounded tail begins.
    tail: u64,
}

impl FreeSpace {
    /// The complement of `occupied` within `extent`, plus a tail.
    ///
    /// `occupied` need not be sorted or disjoint; overlapping claims are
    /// merged, because two contributions keeping overlapping slots is a
    /// corrupt table rather than something to allocate around.
    pub fn new(occupied: &mut [(u64, u64)], extent: u64) -> FreeSpace {
        occupied.sort_unstable();
        let mut holes = Vec::new();
        let mut cursor = 0u64;
        for &(start, end) in occupied.iter() {
            if start > cursor {
                holes.push((cursor, start));
            }
            cursor = cursor.max(end);
        }
        let tail = cursor.max(extent);
        if tail > cursor {
            holes.push((cursor, tail));
        }
        FreeSpace { holes, tail }
    }

    /// Take `size` bytes at `alignment`, from the first hole that fits.
    ///
    /// First fit rather than best fit. Best fit needs a size-ordered index to
    /// be worth its name, and the thing being minimised here is not
    /// fragmentation but *movement*: any hole that fits leaves every other
    /// contribution where it was, and that is the whole return.
    pub fn take(&mut self, size: u64, alignment: u64) -> u64 {
        for index in 0..self.holes.len() {
            let (start, end) = self.holes[index];
            let aligned = align_up(start, alignment);
            if aligned + size <= end {
                if aligned + size == end {
                    self.holes.remove(index);
                } else {
                    self.holes[index] = (aligned + size, end);
                }
                // The alignment gap before it is left behind rather than
                // tracked: it is smaller than any alignment, so nothing can
                // ever be placed in it.
                return aligned;
            }
        }
        let offset = align_up(self.tail, alignment);
        self.tail = offset + size;
        offset
    }

    /// Where the section now ends.
    pub fn extent(&self) -> u64 {
        self.tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free(occupied: &[(u64, u64)], extent: u64) -> FreeSpace {
        FreeSpace::new(&mut occupied.to_vec(), extent)
    }

    /// The hole a removed contribution leaves is where the next new one goes.
    #[test]
    fn a_gap_between_kept_slots_is_allocated_before_the_end() {
        let mut space = free(&[(0, 100), (400, 500)], 500);
        assert_eq!(space.take(200, 16), 112);
        assert_eq!(space.extent(), 500, "the section did not need to grow");
    }

    /// And when nothing fits, the section grows rather than overlapping.
    #[test]
    fn something_too_large_for_every_hole_goes_at_the_end() {
        let mut space = free(&[(0, 100), (400, 500)], 500);
        // 512, not 500: the tail is aligned before it is used.
        assert_eq!(space.take(1000, 16), 512);
        assert_eq!(space.extent(), 1512);
    }

    /// Alignment is honoured inside a hole, not only at the end — a hole that
    /// fits the size but not the aligned size is not a fit.
    #[test]
    fn a_hole_that_only_fits_unaligned_is_not_used() {
        // 8 bytes free at offset 100, but 16-byte alignment needs 112..120.
        let mut space = free(&[(0, 100), (108, 500)], 500);
        assert_eq!(space.take(8, 16), 512, "it should have gone to the end");
    }

    /// Two allocations must not be handed the same bytes.
    #[test]
    fn a_hole_is_not_handed_out_twice() {
        let mut space = free(&[(0, 100), (400, 500)], 500);
        let first = space.take(100, 16);
        let second = space.take(100, 16);
        assert_eq!(first, 112);
        assert_eq!(second, 224, "the second allocation aligned past the first");
        assert_ne!(first, second);
    }

    /// A corrupt table claiming overlapping slots must not produce a hole
    /// inside a live contribution.
    #[test]
    fn overlapping_claims_are_merged_rather_than_trusted() {
        let mut space = free(&[(0, 200), (100, 300)], 300);
        assert_eq!(space.take(16, 16), 304.min(align_up(300, 16)));
    }

    /// The identity check, not the arithmetic: a slot is only reusable for the
    /// section it was recorded in.
    #[test]
    fn a_slot_from_another_section_is_not_reused() {
        let mut slots = HashMap::new();
        slots.insert(
            ContributionKey(7),
            PreviousSlot {
                section: "__TEXT,__text".into(),
                offset: 4096,
                capacity: 256,
            },
        );
        let previous = PreviousLayout {
            sections: BTreeMap::new(),
            slots,
        };
        assert_eq!(
            previous.slot(ContributionKey(7), "__TEXT,__text", 100),
            Some(4096)
        );
        assert_eq!(
            previous.slot(ContributionKey(7), "__DATA,__data", 100),
            None
        );
        assert_eq!(
            previous.slot(ContributionKey(7), "__TEXT,__text", 257),
            None
        );
        assert_eq!(
            previous.slot(ContributionKey(8), "__TEXT,__text", 100),
            None
        );
    }
}
