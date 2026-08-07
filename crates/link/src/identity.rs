//! What a contribution is, as against where it happened to be.
//!
//! [`ObjectId`] is assigned by input order and archive extraction round. It is
//! the right handle *within* one link and useless *between* two: pull one
//! fewer member out of `libstd.rlib` and every id after it names a different
//! object. Anything that carries information from one link to the next — a
//! placement table, a fixup record, a liveness result — needs a name that
//! survives that, and this is where it is computed.
//!
//! The parts
//! ---------
//!
//! ```text
//!   /path/to/libblinker_diagnostics-fc7020f7.rlib      the file it came from
//!   blinker_diagnostics-fc7020f7.rcgu.o                 the member within it
//!   __TEXT,__text                                       which of its sections
//!   0                                                   which of those, if several
//! ```
//!
//! The section is named rather than numbered because a `SectionId` is an index
//! into the object's own section list, and recompiling an object can reorder
//! it. Getting that wrong does not produce a wrong binary — a contribution
//! that keeps a slot it should not have is still relocated against the address
//! it actually landed at — but it silently costs the address stability the
//! table exists to provide, which is the kind of failure that shows up as a
//! benchmark result rather than a test.
//!
//! Why a hash rather than the parts
//! --------------------------------
//!
//! The table is written to the cache and read on every link, and a real link
//! has thousands of contributions whose parts are three strings each. What is
//! stored is 8 bytes. A collision puts two contributions in one slot, which
//! the allocator survives — a slot is claimed once and the second claimant
//! allocates fresh — so the cost of one is a moved contribution, not a corrupt
//! image.

// Nothing consumes this yet: the allocator that will (`compute_layout_reusing`)
// needs the previous placement table, and that table has to survive in the
// cache before it can be read back. This is the half that can be built and
// tested on its own, and it is the half everything else waits on — a placement
// table, a fixup record and a liveness result all need the same answer to
// "which contribution is this?". Landed with its tests rather than written
// inside the commit that needs it, so that when it is wired the only new thing
// being debugged is the wiring. `placement.rs` was landed the same way and for
// the same reason.
#![allow(dead_code)]

use blinker_layout::ContributionKey;
use blinker_macho::{ObjectId, ParsedObject, SectionId};
use std::hash::{Hash, Hasher};

use crate::hashing::{FastHasher, FastMap};
use crate::LoadedObject;

/// Identity for every contribution in a link, by the ids this link uses.
///
/// Built once and read by the layout allocator, which is handed a closure over
/// it: the layout crate must not be able to see an `ObjectId` through this, or
/// it could come to depend on one.
#[derive(Debug, Default)]
pub struct ContributionKeys {
    keys: FastMap<(u32, u32), ContributionKey>,
}

impl ContributionKeys {
    pub(crate) fn build(objects: &[LoadedObject]) -> ContributionKeys {
        let mut keys = FastMap::default();
        // How many members of this name this archive has already produced.
        //
        // `ar` does not require member names to be unique, and real archives
        // are not: `libllvm_sys.rlib` holds three members called `COFF.cpp.o`,
        // one per source directory that built a file with that name, and
        // pulsevm's inputs contain 48 such duplicates. Keyed on path and name
        // alone all three are one contribution, all three claim the previous
        // link's single slot, and the layout places them at one offset — an
        // overlap `carve` refuses, which is how every pulsevm relink failed
        // (finding 241).
        let mut duplicates: FastMap<(&std::path::Path, &str), u32> = FastMap::default();
        for object in objects {
            // The path this link read it from, not the one baked into a shared
            // parse. Content reuse (finding 145) hands the same parse back
            // under a new name, and taking the name from the parse would make
            // a contribution's identity depend on whether the session happened
            // to be holding it — so a warm link and a cold one would disagree
            // about what the same bytes are called.
            let copy = match object.member.as_deref() {
                Some(member) => {
                    let count = duplicates
                        .entry((object.path.as_ref(), member))
                        .or_insert(0);
                    let copy = *count;
                    *count += 1;
                    copy
                }
                None => 0,
            };
            let input = input_identity(object.path.as_ref(), object.member.as_deref(), copy);
            for (ordinal, section) in numbered_sections(&object.parsed) {
                keys.insert(
                    (object.parsed.id.0, section.id.0),
                    contribution_key(input, &section.segment, &section.name, ordinal),
                );
            }
        }
        ContributionKeys { keys }
    }

    pub fn get(&self, object: ObjectId, section: SectionId) -> Option<ContributionKey> {
        self.keys.get(&(object.0, section.0)).copied()
    }

    /// The key to use when a contribution has none.
    ///
    /// Zero rather than a panic: a section this link placed but did not index
    /// must not match a recorded slot, and must not stop the link either.
    pub fn key_or_fresh(&self, object: ObjectId, section: SectionId) -> ContributionKey {
        self.get(object, section).unwrap_or(ContributionKey(0))
    }

    /// The same table as a plain map, for the output crate.
    pub fn as_map(&self) -> std::collections::HashMap<(u32, u32), ContributionKey> {
        self.keys.iter().map(|(k, v)| (*k, *v)).collect()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Sections paired with their ordinal among those of the same qualified name.
///
/// Nearly always zero. An object with two `__TEXT,__const` sections is legal
/// and rare, and without the ordinal both would hash the same and fight over
/// one slot on every link.
fn numbered_sections(
    object: &ParsedObject,
) -> impl Iterator<Item = (u32, &blinker_macho::InputSection)> {
    let mut seen: FastMap<(&str, &str), u32> = FastMap::default();
    object.sections.iter().map(move |section| {
        let count = seen
            .entry((section.segment.as_str(), section.name.as_str()))
            .or_insert(0);
        let ordinal = *count;
        *count += 1;
        (ordinal, section)
    })
}

/// The file an object came from, as a number.
///
/// An archive member is its archive's path, its member name, *and* which copy
/// of that name it is: the path alone names an rlib holding two hundred
/// objects, and the name is not unique within one archive.
///
/// `copy` is the member's ordinal among those of the same name in the same
/// archive, in extraction order — the same device `numbered_sections` uses for
/// an object with two `__TEXT,__const` sections, and needed for the same
/// reason. It is stable across a relink as long as the archive's member order
/// is, which it is for an archive its build system rebuilt. When it is not,
/// two members trade slots: they keep the addresses of each other's previous
/// contribution, which costs relocation reuse and nothing else, because a
/// contribution is always relocated against the address it actually landed at.
///
/// All three come from the *link*, never from the parse. A held parse can now
/// outlive the archive it was read from — rustc renames every codegen unit of
/// a recompiled crate, and the member cache proves the bytes are unchanged and
/// serves the old parse under the new name — so taking the member name from
/// `metadata` would make a warm link and a cold one disagree about what the
/// same bytes are called.
fn input_identity(path: &std::path::Path, member: Option<&str>, copy: u32) -> u64 {
    let mut hasher = FastHasher::default();
    path.hash(&mut hasher);
    match member {
        Some(member) => {
            1u8.hash(&mut hasher);
            member.hash(&mut hasher);
            copy.hash(&mut hasher);
        }
        None => 0u8.hash(&mut hasher),
    }
    hasher.finish()
}

fn contribution_key(input: u64, segment: &str, name: &str, ordinal: u32) -> ContributionKey {
    let mut hasher = FastHasher::default();
    input.hash(&mut hasher);
    segment.hash(&mut hasher);
    name.hash(&mut hasher);
    ordinal.hash(&mut hasher);
    // Zero is reserved for "no identity", so a real one must never be it.
    ContributionKey(hasher.finish() | 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_macho::{InputSection, ObjectMetadata, SectionKind};
    use std::path::PathBuf;

    fn section(id: u32, segment: &str, name: &str) -> InputSection {
        InputSection {
            id: SectionId(id),
            segment: segment.into(),
            name: name.into(),
            kind: SectionKind::Code,
            size: 64,
            vm_address: 0,
            alignment: 4,
            file_offset: Some(0),
            no_dead_strip: false,
        }
    }

    fn object(id: u32, path: &str, member: Option<&str>) -> ParsedObject {
        ParsedObject {
            id: ObjectId(id),
            architecture: blinker_macho::Architecture::Arm64,
            subsections_via_symbols: true,
            metadata: ObjectMetadata {
                path: PathBuf::from(path),
                member: member.map(str::to_string),
                file_size: 0,
                has_debug_info: false,
                has_unwind_info: false,
            },
            sections: vec![
                section(0, "__TEXT", "__text"),
                section(1, "__DATA", "__data"),
            ],
            symbols: Vec::new(),
            relocations: Vec::new(),
        }
    }

    /// Deliberately the production path rather than a restatement of it: an
    /// earlier version of this helper recomputed the hash itself, which would
    /// have kept passing had the real one changed underneath it.
    fn key(object: &ParsedObject, section: u32) -> ContributionKey {
        key_of_copy(object, section, 0)
    }

    /// The same, for the `copy`th member of its name in its archive.
    fn key_of_copy(object: &ParsedObject, section: u32, copy: u32) -> ContributionKey {
        let input = input_identity(
            &object.metadata.path.clone(),
            object.metadata.member.as_deref(),
            copy,
        );
        let (ordinal, section) = numbered_sections(object)
            .find(|(_, s)| s.id.0 == section)
            .expect("the section exists");
        contribution_key(input, &section.segment, &section.name, ordinal)
    }

    /// The property everything else rests on: the same file's same section is
    /// the same contribution, whatever id this link happened to give it.
    #[test]
    fn the_same_input_keeps_its_key_when_its_object_id_changes() {
        let first = object(3, "/lib/libstd.rlib", Some("std.o"));
        let second = object(17, "/lib/libstd.rlib", Some("std.o"));
        assert_eq!(key(&first, 0), key(&second, 0));
    }

    /// And the property that makes an rlib edit cheap rather than total: two
    /// members of one archive are two contributions, not one.
    #[test]
    fn two_members_of_one_archive_are_distinct() {
        let a = object(0, "/lib/libstd.rlib", Some("std.0.o"));
        let b = object(1, "/lib/libstd.rlib", Some("std.1.o"));
        assert_ne!(key(&a, 0), key(&b, 0));
    }

    /// And two members of one archive that share a *name* are still two.
    ///
    /// `ar` does not require member names to be unique. `libllvm_sys.rlib`
    /// holds three members called `COFF.cpp.o`, and pulsevm's inputs contain
    /// 48 such duplicates. Keyed on path and name alone all of them are one
    /// contribution, they all claim one slot in the previous layout, and the
    /// allocator places them at one offset — an overlapping layout, which is
    /// exactly what made every pulsevm relink fail (finding 241).
    #[test]
    fn two_members_of_one_archive_sharing_a_name_are_distinct() {
        let first = object(0, "/lib/libllvm_sys.rlib", Some("COFF.cpp.o"));
        let second = object(1, "/lib/libllvm_sys.rlib", Some("COFF.cpp.o"));
        assert_ne!(
            key_of_copy(&first, 0, 0),
            key_of_copy(&second, 0, 1),
            "two members of one archive with one name shared a contribution"
        );
    }

    /// The copy ordinal is what separates them, and it is what has to be
    /// stable: the same copy of the same name is the same contribution.
    #[test]
    fn the_same_copy_of_a_repeated_member_keeps_its_key() {
        let before = object(4, "/lib/libllvm_sys.rlib", Some("COFF.cpp.o"));
        let after = object(91, "/lib/libllvm_sys.rlib", Some("COFF.cpp.o"));
        assert_eq!(key_of_copy(&before, 0, 2), key_of_copy(&after, 0, 2));
    }

    /// A loose object and an archive member of the same name are different.
    #[test]
    fn a_member_is_not_the_archive_it_came_from() {
        let member = object(0, "/lib/libstd.rlib", Some("std.o"));
        let loose = object(1, "/lib/libstd.rlib", None);
        assert_ne!(key(&member, 0), key(&loose, 0));
    }

    /// Sections of one object are distinct, or every section of it would claim
    /// one slot.
    #[test]
    fn sections_of_one_object_are_distinct() {
        let object = object(0, "/tmp/a.o", None);
        assert_ne!(key(&object, 0), key(&object, 1));
    }

    /// Two sections with the same name in one object are still two.
    #[test]
    fn same_named_sections_are_separated_by_their_ordinal() {
        let mut object = object(0, "/tmp/a.o", None);
        object.sections = vec![
            section(0, "__TEXT", "__const"),
            section(1, "__TEXT", "__const"),
        ];
        assert_ne!(key(&object, 0), key(&object, 1));
    }

    /// Renaming the file is a different input, which is what makes rustc's
    /// per-session object names safe to key on.
    #[test]
    fn a_different_path_is_a_different_input() {
        assert_ne!(
            key(&object(0, "/tmp/a.o", None), 0),
            key(&object(0, "/tmp/b.o", None), 0)
        );
    }

    /// Zero means "no identity", so nothing real may collide with it.
    #[test]
    fn no_real_key_is_the_reserved_one() {
        for id in 0..64u32 {
            let object = object(id, &format!("/tmp/{id}.o"), None);
            assert_ne!(key(&object, 0), ContributionKey(0));
        }
    }
}
