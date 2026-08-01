//! State that outlives one link.
//!
//! Finding 103 measured every stage of a 30 ms edit relink and found no big
//! item — eleven jobs of 1–7 ms, all of them work a *cold* link genuinely has
//! to do. What makes them waste is that an edit changes ~2 MB of 60, and all
//! sixty are read, parsed and analysed again from scratch because the process
//! that did it last time has exited.
//!
//! Nothing here is a cache in the sense finding 101 warns about. That warning
//! is about caches that *serialise* an answer and read it back: relocated bytes
//! lost to memcpy because relocating them was cheaper than copying them. This
//! keeps answers in memory, in the shape they were computed in, at the cost of
//! a `stat` to prove they are still the answer. There is no encode, no decode,
//! and no copy.
//!
//! # What proves an entry still valid
//!
//! The same test the incremental cache uses (`blinker_cache::InputKey`), and
//! for the same reasons: rustc's own object files carry a per-build session
//! component in their names, so only their content identifies them, while
//! toolchain rlibs live at content-addressed paths that cannot change without
//! the path changing.
//!
//! # Why the whole session is dropped when the input list changes
//!
//! Object ids are assigned by position in the argument vector. Two links whose
//! inputs are the same files in the same order agree about every id; one that
//! inserts an input renumbers everything after it, and a cached
//! `ParsedObject` carries its id inside. Rather than renumber — which would
//! mean rewriting the one field that everything else is keyed by — a changed
//! input list starts over. That is the "same top-level input sequence"
//! precondition, and a build system that adds a crate pays one cold link for
//! it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use blinker_macho::ParsedObject;

use crate::hashing::FastMap;
use crate::mapping::Backing;

/// One input, as this process last saw it.
enum Entry {
    Object(Arc<ParsedObject>, Arc<Backing>),
    Archive(blinker_archive::ArchiveIndex, Arc<Backing>),
}

/// A parsed archive member, by the archive it came from and its position in it.
///
/// Held separately from the archive rather than inside it because members are
/// parsed lazily, in extraction rounds, long after the archive was indexed —
/// and because an archive that changes must take its members with it, which is
/// what the generation counter below does in one assignment.
type MemberKey = (PathBuf, u32);

/// The SDK stub files an exports set was read from, and the set itself.
type HeldStubs = (Vec<(PathBuf, blinker_cache::InputKey)>, Arc<StubExports>);

/// Parsed inputs held across links.
///
/// Created once by a resident linker and passed to every link it performs. A
/// one-shot link uses [`Session::default`] and gets exactly the previous
/// behaviour: every probe misses, and nothing is retained past the call.
#[derive(Default)]
pub struct Session {
    /// The argument vector these entries were parsed for. See the module docs
    /// on why a change to it empties everything.
    inputs: Vec<PathBuf>,
    entries: FastMap<PathBuf, (blinker_cache::InputKey, Entry)>,
    /// The SDK's exported symbols, and the stub files they came from.
    ///
    /// Kept whole rather than per file: it is one answer to one question —
    /// "which names does the system provide?" — and the files behind it are
    /// part of the SDK, so they change when Xcode changes and not otherwise.
    stubs: Option<HeldStubs>,
    /// Members already pulled out of archives and parsed. A Rust link extracts
    /// several hundred of them and parses every one on every link; holding the
    /// archive's bytes without holding what was parsed out of them leaves the
    /// larger half of the work in place.
    /// The parse *and* the byte window it describes. Both, because a member is
    /// a slice of its archive: the first version of this handed back the parsed
    /// member with the whole archive as its bytes, and the next link read every
    /// section from the wrong offset. It failed loudly — a misaligned
    /// relocation on the second link — which is the good case; a member whose
    /// sections happened to land at plausible offsets would have produced a
    /// binary instead.
    members: FastMap<MemberKey, (Arc<ParsedObject>, std::ops::Range<usize>)>,
    hits: usize,
    misses: usize,
}

/// What the SDK's `.tbd` stubs say the system exports.
pub type StubExports = std::collections::BTreeSet<String>;

impl Session {
    /// Begin a link over `inputs`, discarding anything that cannot apply.
    pub fn begin(&mut self, inputs: &[PathBuf]) {
        if self.inputs != inputs {
            self.entries.clear();
            self.members.clear();
            self.inputs = inputs.to_vec();
        }
        self.hits = 0;
        self.misses = 0;
    }

    /// The entry for `path`, if this process has it and it is still current.
    ///
    /// Probing costs a `stat`, or a read and a hash for the inputs whose paths
    /// do not identify them. That is the price of not trusting memory about a
    /// file another process writes.
    ///
    /// Every exit from here is counted. The first version returned early on a
    /// missing entry and on a failed probe, so those two — the *interesting*
    /// misses — were counted as neither hit nor miss, and a link that re-read a
    /// changed archive reported reading nothing. A counter that undercounts
    /// misses is worse than no counter: it makes a cache look perfect exactly
    /// when it is not working.
    fn current(&mut self, path: &Path) -> Option<&Entry> {
        let known = self.entries.contains_key(path);
        let current = known
            && blinker_cache::InputKey::probe(path)
                .is_some_and(|now| self.entries.get(path).is_some_and(|(key, _)| *key == now));
        self.count(current);
        current.then(|| &self.entries.get(path).expect("just checked").1)
    }

    /// A parsed object for `path`, or `None` to parse it.
    pub fn object(&mut self, path: &Path) -> Option<(Arc<ParsedObject>, Arc<Backing>)> {
        match self.current(path)? {
            Entry::Object(parsed, backing) => Some((Arc::clone(parsed), Arc::clone(backing))),
            Entry::Archive(..) => None,
        }
    }

    /// An indexed archive for `path`, or `None` to index it.
    pub fn archive(
        &mut self,
        path: &Path,
    ) -> Option<(blinker_archive::ArchiveIndex, Arc<Backing>)> {
        match self.current(path)? {
            Entry::Archive(index, backing) => Some((index.clone(), Arc::clone(backing))),
            Entry::Object(..) => None,
        }
    }

    fn count(&mut self, hit: bool) {
        if hit {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
    }

    /// Remember a freshly parsed object.
    ///
    /// The key is probed *after* parsing rather than before. A file rewritten
    /// while it was being read would otherwise be stored under the key it had
    /// beforehand and served forever after; probing afterwards records the
    /// state that was actually read, or — if it changed again — a key that will
    /// fail on the next probe, which is the safe direction to be wrong in.
    pub fn store_object(&mut self, path: &Path, parsed: &Arc<ParsedObject>, data: &Arc<Backing>) {
        let Some(key) = blinker_cache::InputKey::probe(path) else {
            return;
        };
        self.entries.insert(
            path.to_path_buf(),
            (key, Entry::Object(Arc::clone(parsed), Arc::clone(data))),
        );
    }

    /// A member of `archive` that this process has already parsed.
    ///
    /// No probe: an archive's members are only reachable through an archive
    /// entry, and [`Session::store_archive`] drops every member of an archive
    /// whose bytes changed. Probing here would `stat` the same rlib once per
    /// member pulled out of it — several hundred times on a Rust link.
    pub fn member(
        &self,
        archive: &Path,
        member: u32,
    ) -> Option<(Arc<ParsedObject>, std::ops::Range<usize>)> {
        self.members
            .get(&(archive.to_path_buf(), member))
            .map(|(parsed, range)| (Arc::clone(parsed), range.clone()))
    }

    /// Remember a freshly parsed archive member and where it sits.
    pub fn store_member(
        &mut self,
        archive: &Path,
        member: u32,
        parsed: &Arc<ParsedObject>,
        range: std::ops::Range<usize>,
    ) {
        self.members
            .insert((archive.to_path_buf(), member), (Arc::clone(parsed), range));
    }

    /// Remember a freshly indexed archive.
    pub fn store_archive(
        &mut self,
        path: &Path,
        index: &blinker_archive::ArchiveIndex,
        data: &Arc<Backing>,
    ) {
        let Some(key) = blinker_cache::InputKey::probe(path) else {
            return;
        };
        // Storing an archive means it was just read, which means its previous
        // contents are gone and so is anything parsed out of them.
        self.members.retain(|(archive, _), _| archive != path);
        self.entries.insert(
            path.to_path_buf(),
            (key, Entry::Archive(index.clone(), Arc::clone(data))),
        );
    }

    /// The SDK's exports, if the stub files behind them are unchanged.
    pub fn stub_exports(&self, stubs: &[PathBuf]) -> Option<Arc<StubExports>> {
        let (recorded, exports) = self.stubs.as_ref()?;
        if recorded.len() != stubs.len() {
            return None;
        }
        for ((path, key), wanted) in recorded.iter().zip(stubs) {
            if path != wanted || blinker_cache::InputKey::probe(path).as_ref() != Some(key) {
                return None;
            }
        }
        Some(Arc::clone(exports))
    }

    /// Remember the SDK's exports.
    pub fn store_stub_exports(&mut self, stubs: &[PathBuf], exports: Arc<StubExports>) {
        let mut recorded = Vec::with_capacity(stubs.len());
        for path in stubs {
            let Some(key) = blinker_cache::InputKey::probe(path) else {
                return;
            };
            recorded.push((path.clone(), key));
        }
        self.stubs = Some((recorded, exports));
    }

    /// Inputs served from memory, and inputs that had to be read.
    pub fn counts(&self) -> (usize, usize) {
        (self.hits, self.misses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_test_support::Scratch;

    fn exports(names: &[&str]) -> Arc<StubExports> {
        Arc::new(names.iter().map(|n| n.to_string()).collect())
    }

    /// The stubs are held across links while their files are untouched.
    #[test]
    fn stub_exports_survive_a_link() {
        let scratch = Scratch::dir("session-stubs").expect("scratch");
        let path = scratch.join("libSystem.tbd");
        std::fs::write(&path, "--- !tapi-tbd\n").expect("written");

        let mut session = Session::default();
        let stubs = vec![path.clone()];
        assert!(session.stub_exports(&stubs).is_none(), "empty session hit");

        session.store_stub_exports(&stubs, exports(&["_malloc"]));
        assert_eq!(
            session.stub_exports(&stubs).map(|e| e.len()),
            Some(1),
            "the exports were not held"
        );
    }

    /// And discarded when the SDK moves under them. A stale answer here is a
    /// link that resolves a name the system no longer provides.
    #[test]
    fn changed_stubs_are_not_served_from_memory() {
        let scratch = Scratch::dir("session-stubs-changed").expect("scratch");
        let path = scratch.join("libSystem.tbd");
        std::fs::write(&path, "--- !tapi-tbd\n").expect("written");
        let stubs = vec![path.clone()];

        let mut session = Session::default();
        session.store_stub_exports(&stubs, exports(&["_malloc"]));

        // A different length as well as different content: `InputKey` may use
        // metadata, and mtime granularity is coarse enough that a same-size
        // rewrite within a tick could otherwise look unchanged.
        std::fs::write(&path, "--- !tapi-tbd\nrewritten\n").expect("rewritten");
        assert!(
            session.stub_exports(&stubs).is_none(),
            "a changed SDK was served from memory"
        );
    }

    /// Asking for a different set of stub files must miss even when every file
    /// in the old set is untouched.
    #[test]
    fn a_different_stub_set_is_not_a_hit() {
        let scratch = Scratch::dir("session-stubs-set").expect("scratch");
        let one = scratch.join("one.tbd");
        let two = scratch.join("two.tbd");
        std::fs::write(&one, "a").expect("written");
        std::fs::write(&two, "b").expect("written");

        let mut session = Session::default();
        session.store_stub_exports(std::slice::from_ref(&one), exports(&["_malloc"]));
        assert!(session.stub_exports(&[one.clone(), two]).is_none());
        assert!(session.stub_exports(&[one]).is_some());
    }

    /// Changing the input list empties the object cache, because ids are
    /// positional and a cached object carries its id.
    #[test]
    fn a_changed_input_list_starts_over() {
        let scratch = Scratch::dir("session-inputs").expect("scratch");
        let path = scratch.join("a.o");
        std::fs::write(&path, vec![0u8; 128]).expect("written");

        let mut session = Session::default();
        session.begin(std::slice::from_ref(&path));
        let backing = Arc::new(Backing::Heap(vec![0u8; 128]));
        let parsed = Arc::new(ParsedObject {
            id: blinker_macho::ObjectId(0),
            architecture: blinker_macho::Architecture::Arm64,
            subsections_via_symbols: true,
            metadata: blinker_macho::ObjectMetadata {
                path: path.clone(),
                member: None,
                file_size: 128,
                has_debug_info: false,
                has_unwind_info: false,
            },
            sections: Vec::new(),
            symbols: Vec::new(),
            relocations: Vec::new(),
        });
        session.store_object(&path, &parsed, &backing);
        assert!(session.object(&path).is_some(), "it was not held");

        session.begin(&[path.clone(), scratch.join("b.o")]);
        assert!(
            session.object(&path).is_none(),
            "a renumbered link served an object carrying its old id"
        );
    }

    /// A rewritten object is re-read even though its path is the same.
    #[test]
    fn a_changed_object_is_not_served_from_memory() {
        let scratch = Scratch::dir("session-object-changed").expect("scratch");
        let path = scratch.join("a.o");
        std::fs::write(&path, vec![1u8; 128]).expect("written");

        let mut session = Session::default();
        session.begin(std::slice::from_ref(&path));
        let backing = Arc::new(Backing::Heap(vec![1u8; 128]));
        let parsed = Arc::new(ParsedObject {
            id: blinker_macho::ObjectId(0),
            architecture: blinker_macho::Architecture::Arm64,
            subsections_via_symbols: true,
            metadata: blinker_macho::ObjectMetadata {
                path: path.clone(),
                member: None,
                file_size: 128,
                has_debug_info: false,
                has_unwind_info: false,
            },
            sections: Vec::new(),
            symbols: Vec::new(),
            relocations: Vec::new(),
        });
        session.store_object(&path, &parsed, &backing);

        std::fs::write(&path, vec![2u8; 256]).expect("rewritten");
        assert!(
            session.object(&path).is_none(),
            "a rewritten object was served from memory"
        );
    }
}
