//! Recorded placements, the input to M6's stable layout.
//!
//! A cold link assigns addresses from scratch. M6 reuses the previous link's
//! placements so unchanged code keeps its address across an edit, which is what
//! makes dependent relocations reusable rather than needing recomputation.
//! Recording placements now — even though nothing consumes them yet — keeps the
//! shape stable when that arrives.

/// Identifier for a placed region of the output.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct OutputRegionId(pub u32);

/// Where a region was placed, and how much room it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegionPlacement {
    pub region: OutputRegionId,
    pub file_offset: u64,
    pub virtual_address: u64,
    /// Bytes actually occupied.
    pub used_size: u64,
    /// Bytes reserved. Equal to `used_size` in a cold layout; M6 makes this
    /// larger so a region can grow without moving its neighbours.
    pub reserved_size: u64,
    pub alignment: u64,
}

impl RegionPlacement {
    /// Whether `size` still fits the reservation.
    ///
    /// The question M6 asks on every edit: if the answer is yes the region
    /// keeps its address, and every relocation pointing at it stays valid.
    pub fn accommodates(&self, size: u64) -> bool {
        size <= self.reserved_size
    }

    /// Unused bytes within the reservation.
    pub fn slack(&self) -> u64 {
        self.reserved_size.saturating_sub(self.used_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(used: u64, reserved: u64) -> RegionPlacement {
        RegionPlacement {
            region: OutputRegionId(0),
            file_offset: 0,
            virtual_address: 0x1000,
            used_size: used,
            reserved_size: reserved,
            alignment: 16,
        }
    }

    #[test]
    fn a_region_accommodates_anything_up_to_its_reservation() {
        let p = placement(100, 200);
        assert!(p.accommodates(100));
        assert!(p.accommodates(200));
        assert!(!p.accommodates(201));
    }

    #[test]
    fn a_cold_placement_has_no_slack() {
        assert_eq!(placement(100, 100).slack(), 0);
        assert_eq!(placement(100, 250).slack(), 150);
    }

    #[test]
    fn slack_does_not_underflow_when_used_exceeds_reserved() {
        // Only reachable from a corrupt cached placement, but it must not wrap
        // into a huge number and be read as abundant room.
        assert_eq!(placement(300, 100).slack(), 0);
    }
}
