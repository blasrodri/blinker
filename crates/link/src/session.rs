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
use blinker_symbols::{SymbolNameId, SymbolNames};

use crate::hashing::{FastMap, FastSet};
use crate::mapping::Backing;

/// What has been worked out about one held parse.
struct Memo {
    /// Held so the pointer this is keyed by cannot be reused. See `memo`.
    _parse: Arc<ParsedObject>,
    /// Atom boundaries by section id; `None` for a section that is not split.
    boundaries: FastMap<u32, Option<Arc<Vec<u64>>>>,
    /// This object's atoms and the edges leaving them, in its own numbering.
    atoms: Option<Arc<crate::reachability::ObjectAtoms>>,
    /// Each of this object's symbol names, interned against `Session::names`.
    /// Indexed by `SymbolId`, which is a position in `parsed.symbols`.
    interned: Option<Arc<Vec<SymbolNameId>>>,
}

impl Memo {
    fn new(parse: &Arc<ParsedObject>) -> Memo {
        Memo {
            _parse: Arc::clone(parse),
            boundaries: FastMap::default(),
            atoms: None,
            interned: None,
        }
    }
}

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

/// The archives an extraction order is indexed against, and the order itself.
///
/// `(archive position, member)` means nothing against a different set of
/// archives, so the two travel together.
type ExtractionOrder = (Vec<PathBuf>, Vec<(usize, u32)>);

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
    /// Which archive members the last link pulled in, in the order it pulled
    /// them.
    ///
    /// The frontier that decides this is a fixed point: rounds of "which names
    /// are still undefined, which archive defines one" over every symbol of
    /// every object. With the parses held it is the whole of what `read+parse`
    /// still costs — and it is a pure function of the objects' symbols, so if
    /// no input changed it cannot come out differently.
    ///
    /// Held only for a link where *nothing* was re-read. One changed object can
    /// define a name that stops a member being wanted, or want one that was
    /// not, and there is no cheap way to know which; the plan is discarded and
    /// recomputed.
    /// The archive sub-list this order indexes into, and the order.
    ///
    /// The order is `(archive position, member)`, so it means nothing against
    /// a different set of archives. Storing them together is what lets the
    /// order survive an input list that changed *elsewhere* — which is every
    /// debug rebuild, because rustc renames the loose objects (finding 144).
    extraction: Option<ExtractionOrder>,
    /// A digest of each input's *symbol interface* — what it defines and what
    /// it leaves undefined — from when it was last parsed.
    ///
    /// The extraction frontier reads nothing else. An edit that changes a
    /// function's body changes the object's bytes, so it must be re-parsed;
    /// it does not change which names the object offers or asks for, so it
    /// cannot change which archive members get pulled in. Without this, one
    /// re-read input disables the replay for the whole link, which is the
    /// common case rather than a corner: an edit that changes nothing else
    /// still changes something.
    interfaces: FastMap<PathBuf, u64>,
    /// Each archive's symbol index as last read. Compared rather than trusted:
    /// it is what decides which member defines a name.
    /// Each archive's symbol table as last read: name to defining member, and
    /// nothing else.
    ///
    /// Not the whole `ArchiveIndex`. That carries every member's offset and
    /// size, which move whenever any member's *content* changes — so comparing
    /// it rejected every edit, which is correct and useless. What the frontier
    /// asks an archive is "which member defines this name", and that is this
    /// table.
    indexes: FastMap<PathBuf, Vec<(String, blinker_archive::MemberId)>>,
    /// Whether any input's interface differs from the one held for it.
    interfaces_changed: bool,
    /// The cache path this session has already written a file for.
    ///
    /// Per path, not per session. It was a single boolean, and that was wrong in
    /// a way only a daemon shows: one resident session serves *many different
    /// links*, and after the first of them had written its file, no other cache
    /// path was ever written again. The no-op fast path reads the file, so
    /// every subsequent program lost it permanently — a resident linker that
    /// got slower the longer it ran.
    cache_written: Option<PathBuf>,
    /// Whether this session outlives the link that is using it.
    ///
    /// Set by the daemon, and by nothing else. It is not "am I warm yet" — it
    /// is "will there be a next link through me", which is what decides whether
    /// work done *for* the next link is an investment or a waste. Recording
    /// what each object read is the case that turns on it: pure cost to a
    /// process that is about to exit, and 2.8 ms a link to one that is not.
    resident: bool,
    /// Facts derived from one parsed object, held for as long as that parse is.
    ///
    /// Keyed by the identity of the `Arc<ParsedObject>` itself — its pointer —
    /// which is exact in both directions: the same parse gives the same key, and
    /// a re-parsed input gets a fresh allocation and therefore a miss. The `Arc`
    /// is *held inside the entry* so the allocation cannot be freed and its
    /// address handed to the next parse, which would serve one object's derived
    /// facts for another's.
    ///
    /// Everything in here must be a pure function of the object alone. Atom
    /// boundaries are: where a section divides into independently-strippable
    /// pieces depends on that object's symbols and relocations and on nothing
    /// else in the link.
    /// Parses by the *content* of the file they came from.
    ///
    /// rustc renames every object of a recompiled crate on every debug build
    /// while leaving the bytes identical — 132 of rust-analyzer's 341 inputs
    /// (finding 144). Keyed by path, all of those are misses; keyed by
    /// content, they are hits, and `InputKey::probe` already hashed them
    /// because rustc's paths are not evidence of what is in them.
    ///
    /// Only whole objects. An `.rlib` path *is* content-addressed — it carries
    /// a 16-hex-digit hash — so an archive that is renamed genuinely changed.
    by_content: FastMap<[u8; 32], (Arc<ParsedObject>, Arc<Backing>)>,
    /// Contents this link touched, so the next one can drop the rest.
    ///
    /// `by_content` cannot be pruned by the input *list*, because the whole
    /// point is to survive a link whose paths all changed. It is pruned by
    /// use instead — the same rule as `forget_unused_memos`, and for the same
    /// reason: without it a resident linker's memory grows with every build
    /// rather than with the program.
    used_content: crate::hashing::FastSet<[u8; 32]>,
    /// The last link's dead-strip answer, and the per-object projection
    /// digests it was computed from.
    ///
    /// Whole-link state, so it is keyed by the whole vector: if every object
    /// contributes the atoms, edges and roots it contributed last time, then
    /// the owners map, the opaque set, the live set and the compaction are all
    /// the same, and so is this.
    strip: Option<(Vec<u64>, std::sync::Arc<crate::reachability::Strip>)>,
    memo: FastMap<usize, Memo>,
    /// Every symbol name this session has ever seen, interned once.
    ///
    /// Held across links, and never renumbered, because the ids it hands out
    /// are what makes a held object's names free the second time: the id
    /// vector memoised beside a parse is only meaningful against the table
    /// that produced it. A link resolves half a million distinct names out of
    /// a million symbols, and hashing those strings was the whole cost of the
    /// resolve stage — but only the first time each is seen.
    ///
    /// It therefore grows monotonically. Names belonging to inputs that are no
    /// longer in the link stay, because dropping one would mean renumbering
    /// and invalidating every surviving id vector. What that costs is bounded
    /// by how many *new* names later links introduce: a Rust rebuild renames
    /// the symbols of the crates it recompiles, so it is the edited crates'
    /// symbols per rebuild, not the program's.
    names: SymbolNames,
    /// Each object's reachability digest, as the last link computed it.
    ///
    /// Keyed by object id, which is positional and stable for as long as the
    /// input list is — the same precondition the whole session runs under.
    reach: FastMap<u32, u64>,
    /// How many digests moved on this link, and how many were compared.
    reach_moved: u64,
    reach_total: u64,
    /// The cache this session's last link produced, and the path it belongs to.
    ///
    /// The cache file is a *restart* mechanism: it exists so a cold process can
    /// pick up where a previous one left off. Between two links handled by the
    /// same resident process it is pure overhead — the link encodes a few
    /// megabytes, writes them, and the next link reads and decodes them back
    /// into the structure it just discarded.
    ///
    /// So a session keeps it. The first link through a session still writes the
    /// file, because a daemon that never wrote one would make every restart
    /// cold; every link after that keeps it in memory and leaves the disk
    /// alone.
    cache: Option<(PathBuf, blinker_cache::LinkCache)>,
    /// How many inputs' interfaces moved, and the first one that did.
    ///
    /// A boolean says the replay was refused; this says by whom. Three of this
    /// session's wrong turns were inferences about which input was to blame.
    interface_changes: u64,
    first_interface_change: Option<PathBuf>,
    /// The imports the last link resolved, and whether the stub exports behind
    /// them were re-read.
    ///
    /// Resolution asks two questions — which undefined names a dylib provides,
    /// and whether any is left over — and both read nothing but names and
    /// strengths. That is precisely what [`interface_digest`] covers, so the
    /// answer stands whenever no interface moved *and* the SDK's exports are
    /// the ones it was computed against.
    imports: Option<Vec<String>>,
    stubs_reparsed: bool,
    hits: usize,
    misses: usize,
    /// Inputs served by content after their path missed. See `by_content`.
    content_hits: usize,
    /// What this link was able to reuse, for the record rather than for the
    /// linker. Every rule here is a claim about when an answer still holds,
    /// and a claim nobody can see the effect of is a claim nobody checks —
    /// three of this session's wrong turns were exactly that.
    replayed_extraction: bool,
    held_resolution: bool,
}

/// What the `.tbd` stubs say the dynamic libraries export, and which exports
/// each name. See [`crate::libraries::StubExports`].
pub type StubExports = crate::libraries::StubExports;

impl Session {
    /// Begin a link over `inputs`, discarding anything that cannot apply.
    ///
    /// A changed input list used to discard *everything*, on the grounds that
    /// object ids are positional and a held parse carries the id it was parsed
    /// with. That is true, and throwing the session away is a heavy way to
    /// enforce it: rustc renames every object of a recompiled crate on every
    /// debug build — 132 of rust-analyzer's 341 inputs, with byte-identical
    /// contents — so the list differs on every edit a developer makes, and a
    /// resident linker went cold every time (finding 144).
    ///
    /// So the per-path facts are kept for the paths that survived, and the
    /// positional hazard is handled where it actually arises: `load_objects`
    /// serves a held parse only when its id is the one this link would assign.
    /// What cannot be kept is anything derived from the list *as a whole* —
    /// the extraction order and the import set, both of which are answers
    /// about every input at once.
    pub fn begin(&mut self, inputs: &[PathBuf]) {
        if self.inputs != inputs {
            let surviving: FastSet<&Path> = inputs.iter().map(PathBuf::as_path).collect();
            self.entries
                .retain(|path, _| surviving.contains(path.as_path()));
            self.members
                .retain(|(archive, _), _| surviving.contains(archive.as_path()));
            self.interfaces.retain(|path, _| {
                // A member's interface is filed under a synthetic path inside
                // its archive, so it survives with the archive rather than on
                // its own.
                surviving.contains(path.as_path())
                    || path
                        .parent()
                        .is_some_and(|parent| surviving.contains(parent))
            });
            self.indexes
                .retain(|path, _| surviving.contains(path.as_path()));
            self.inputs = inputs.to_vec();
        }
        // Contents the previous link never looked at are dropped now, so a
        // parse survives exactly as long as something keeps linking it.
        let used = std::mem::take(&mut self.used_content);
        self.by_content.retain(|hash, _| used.contains(hash));
        self.hits = 0;
        self.misses = 0;
        self.content_hits = 0;
        self.interfaces_changed = false;
        self.interface_changes = 0;
        self.first_interface_change = None;
        self.stubs_reparsed = false;
        self.replayed_extraction = false;
        self.held_resolution = false;
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
        if let Some((blinker_cache::InputKey::Content(hash), _)) = self.entries.get(path) {
            let hash = *hash;
            self.used_content.insert(hash);
        }
        if let Some(entry) = self.current(path) {
            return match entry {
                Entry::Object(parsed, backing) => Some((Arc::clone(parsed), Arc::clone(backing))),
                Entry::Archive(..) => None,
            };
        }
        // Missed by path — which for one of rustc's objects means very little,
        // because it renames them all on every build. `current` has just
        // probed this file, and for a path that is not evidence that probe is
        // a hash of its bytes, so asking the content index costs nothing more
        // than the lookup.
        let blinker_cache::InputKey::Content(hash) = blinker_cache::InputKey::probe(path)? else {
            return None;
        };
        let (parsed, backing) = self.by_content.get(&hash)?;
        let (parsed, backing) = (Arc::clone(parsed), Arc::clone(backing));
        self.used_content.insert(hash);
        self.note_interface_unchanged(path, &parsed);
        // Filed under its new name too, so the next link finds it by path and
        // the entry is dropped with the input list when it stops being linked.
        self.entries.insert(
            path.to_path_buf(),
            (
                blinker_cache::InputKey::Content(hash),
                Entry::Object(Arc::clone(&parsed), Arc::clone(&backing)),
            ),
        );
        self.content_hits += 1;
        Some((parsed, backing))
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
        self.note_interface(path, parsed);
        if let blinker_cache::InputKey::Content(hash) = &key {
            self.used_content.insert(*hash);
            self.by_content
                .insert(*hash, (Arc::clone(parsed), Arc::clone(data)));
        }
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
        // A member's interface is noted under a path of its own, so two
        // members of one archive cannot overwrite each other's digest.
        let named = member_path(archive, member);
        self.note_interface(&named, parsed);
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
        // The archive's *index* is one of the frontier's two inputs: it decides
        // which member defines a name. If it moved, no replay can be trusted;
        // if it did not, the replay is still on the hook for the other input —
        // the interfaces of the members it names — which `member_interface_is`
        // checks as each one is parsed.
        //
        // The counters caught this being wrong: `extraction replayed` on a link
        // that had re-read eleven archives, back when neither input was
        // checked. Nothing failed, because none of the eleven had changed a
        // symbol — the bug was invisible, waiting for the edit that added one.
        let symbol_map = external_symbol_map(&index.symbol_map);
        if self.indexes.get(path) != Some(&symbol_map) {
            self.interfaces_changed = true;
            self.interface_changes += 1;
            if self.first_interface_change.is_none() {
                self.first_interface_change = Some(path.to_path_buf());
            }
        }
        self.indexes.insert(path.to_path_buf(), symbol_map);
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

    /// The imports the last link resolved, if they must still be the same.
    pub fn imports(&mut self) -> Option<&[String]> {
        if self.interfaces_changed || self.stubs_reparsed || self.imports.is_none() {
            return None;
        }
        self.held_resolution = true;
        self.imports.as_deref()
    }

    /// Remember what resolution decided.
    pub fn store_imports(&mut self, imports: &[String]) {
        self.imports = Some(imports.to_vec());
    }

    /// Remember the SDK's exports.
    pub fn store_stub_exports(&mut self, stubs: &[PathBuf], exports: Arc<StubExports>) {
        // Re-reading the SDK means the set of importable names may have moved,
        // and resolution's answer with it.
        self.stubs_reparsed = true;
        let mut recorded = Vec::with_capacity(stubs.len());
        for path in stubs {
            let Some(key) = blinker_cache::InputKey::probe(path) else {
                return;
            };
            recorded.push((path.clone(), key));
        }
        self.stubs = Some((recorded, exports));
    }

    /// The extraction order the last link settled on, if it must still hold.
    ///
    /// It holds when no input's symbol interface changed — not when no input
    /// changed. A re-read object whose defined and undefined names are what
    /// they were cannot make any archive want a different member, because
    /// those two sets are the frontier's only input.
    pub fn extraction(&mut self, archives: &[PathBuf]) -> Option<&[(usize, u32)]> {
        if self.interfaces_changed {
            return None;
        }
        let (recorded, order) = self.extraction.as_ref()?;
        if recorded != archives {
            return None;
        }
        self.replayed_extraction = true;
        Some(order)
    }

    /// Record an interface that is known not to have moved.
    ///
    /// A content hit is the same bytes under a new name, so its interface is
    /// the one already held — but it is held under the *old* name, which the
    /// input list no longer contains. Filing it under the new one keeps the
    /// map complete without `note_interface`'s "absent means changed" rule
    /// firing on a rename, which would invalidate every answer that a rename
    /// is supposed to leave alone.
    fn note_interface_unchanged(&mut self, path: &Path, parsed: &ParsedObject) {
        self.interfaces
            .insert(path.to_path_buf(), interface_digest(parsed));
    }

    /// Whether a freshly parsed member's interface is the one held for it.
    ///
    /// The second half of the replay's safety argument, and the half that can
    /// only be checked *during* the replay: a member's interface is not known
    /// until it is parsed, and parsing it is what the plan decides. So the plan
    /// is followed optimistically and abandoned the moment a member comes back
    /// different — before anything downstream has seen the result.
    pub fn member_interface_is(&self, archive: &Path, member: u32, parsed: &ParsedObject) -> bool {
        let named = member_path(archive, member);
        self.interfaces.get(&named) == Some(&interface_digest(parsed))
    }

    /// Discard a replay that turned out not to hold, so this link recomputes
    /// and the next one is not offered the same wrong answer.
    pub fn abandon_extraction(&mut self) {
        self.extraction = None;
        self.replayed_extraction = false;
        self.interfaces_changed = true;
    }

    /// Note an input's symbol interface, and whether it moved.
    fn note_interface(&mut self, path: &Path, parsed: &ParsedObject) {
        let digest = interface_digest(parsed);
        match self.interfaces.insert(path.to_path_buf(), digest) {
            Some(previous) if previous == digest => {}
            // Absent as well as different: an input this process has not seen
            // before could define anything, so the frontier has to run.
            _ => {
                self.interfaces_changed = true;
                self.interface_changes += 1;
                if self.first_interface_change.is_none() {
                    self.first_interface_change = Some(path.to_path_buf());
                }
            }
        }
    }

    /// Remember which members were extracted, and in what order.
    pub fn store_extraction(&mut self, archives: Vec<PathBuf>, order: Vec<(usize, u32)>) {
        self.extraction = Some((archives, order));
    }

    /// Inputs served from memory, and inputs that had to be read.
    ///
    /// A content hit was counted as a miss when its path was looked up, so it
    /// moves across: it was served from memory, which is what this reports.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.hits + self.content_hits,
            self.misses.saturating_sub(self.content_hits),
        )
    }

    /// Of those held, how many were found by content after their path missed.
    pub fn content_hits(&self) -> usize {
        self.content_hits
    }

    /// Whether the extraction order and the resolution were reused.
    pub fn reused(&self) -> (bool, bool) {
        (self.replayed_extraction, self.held_resolution)
    }

    /// Declare that this session will serve more than one link.
    pub fn set_resident(&mut self, resident: bool) {
        self.resident = resident;
    }

    /// Whether more links are expected through this session; see `resident`.
    pub fn is_resident(&self) -> bool {
        self.resident
    }

    /// The key this session proved `path` by, when it has one.
    ///
    /// Every input was probed during loading — a `stat` for a content-addressed
    /// path, a read and a hash for one of rustc's, which is the expensive kind.
    /// Anything later in the link that wants to know whether a file changed
    /// should ask here rather than probe it a second time.
    pub fn key_for(&self, path: &Path) -> Option<blinker_cache::InputKey> {
        self.entries.get(path).map(|(key, _)| key.clone())
    }

    /// This object's atom boundaries for `section`, computing them once.
    ///
    /// `compute` is called only on a miss. It must be a pure function of the
    /// object — see `memo`.
    pub fn boundaries(
        &mut self,
        parse: &Arc<ParsedObject>,
        section: u32,
        compute: impl FnOnce() -> Option<Vec<u64>>,
    ) -> Option<Arc<Vec<u64>>> {
        let key = Arc::as_ptr(parse) as usize;
        let memo = self.memo.entry(key).or_insert_with(|| Memo::new(parse));
        memo.boundaries
            .entry(section)
            .or_insert_with(|| compute().map(Arc::new))
            .clone()
    }

    /// This object's whole reachability projection, computing it once.
    ///
    /// Its atoms in its own numbering, the edges leaving each of them, and
    /// which of them are roots. Pure in the object — which is the point of
    /// numbering atoms per object rather than per link, because a flat index
    /// is a fact about the link and would be wrong the moment any earlier
    /// object gained an atom.
    pub(crate) fn atoms(
        &mut self,
        parse: &Arc<ParsedObject>,
        compute: impl FnOnce() -> crate::reachability::ObjectAtoms,
    ) -> Arc<crate::reachability::ObjectAtoms> {
        let key = Arc::as_ptr(parse) as usize;
        let memo = self.memo.entry(key).or_insert_with(|| Memo::new(parse));
        Arc::clone(memo.atoms.get_or_insert_with(|| Arc::new(compute())))
    }

    /// This object's symbol names as ids, interning each one once ever.
    ///
    /// The vector is indexed by `SymbolId`, so a caller holding a symbol can
    /// subscript rather than hash. Computed on the link that first parses the
    /// object and reused by every link after it — which is the whole point,
    /// since a held object's names are by definition the ones already in the
    /// table.
    pub(crate) fn interned(&mut self, parse: &Arc<ParsedObject>) -> Arc<Vec<SymbolNameId>> {
        let key = Arc::as_ptr(parse) as usize;
        // Split borrow: the memo entry and the interning table are separate
        // fields, and filling the first needs the second.
        let Session { memo, names, .. } = self;
        let entry = memo.entry(key).or_insert_with(|| Memo::new(parse));
        Arc::clone(entry.interned.get_or_insert_with(|| {
            Arc::new(
                parse
                    .symbols
                    .iter()
                    .map(|symbol| names.intern(&symbol.name))
                    .collect(),
            )
        }))
    }

    /// The interning table these ids belong to.
    pub(crate) fn names(&self) -> &SymbolNames {
        &self.names
    }

    /// The interning table, for a caller that needs to add to it.
    ///
    /// Lent rather than taken: a table that left the session and did not come
    /// back — an error path between the two — would leave the memoised id
    /// vectors describing names the table no longer holds, and the next link
    /// would hand out those same ids for different names.
    pub(crate) fn names_mut(&mut self) -> &mut SymbolNames {
        &mut self.names
    }

    /// Drop derived facts about parses this link did not use.
    ///
    /// Without this the memo holds every object any link ever saw, and holds
    /// their `Arc`s alive with it — a resident linker's memory would grow with
    /// every rebuild rather than with the program being linked.
    pub fn forget_unused_memos(&mut self, used: &FastMap<usize, ()>) {
        self.memo.retain(|key, _| used.contains_key(key));
    }

    /// The previous link's strip, if every object's projection is unchanged.
    pub(crate) fn strip(
        &self,
        digests: &[u64],
    ) -> Option<std::sync::Arc<crate::reachability::Strip>> {
        let (recorded, strip) = self.strip.as_ref()?;
        (recorded == digests).then(|| std::sync::Arc::clone(strip))
    }

    /// Remember this link's strip against the projections that produced it.
    pub(crate) fn store_strip(
        &mut self,
        digests: Vec<u64>,
        strip: std::sync::Arc<crate::reachability::Strip>,
    ) {
        self.strip = Some((digests, strip));
    }

    /// Record this link's reachability digests and report how many moved.
    ///
    /// A digest that moved means some atom boundary or some edge in that object
    /// changed; if none moved, the live set cannot have changed and the whole
    /// strip is reusable.
    pub fn note_reachability(&mut self, digests: &[(u32, u64)]) -> (u64, u64) {
        let mut moved = 0;
        let comparable = !self.reach.is_empty();
        for (object, digest) in digests {
            if comparable && self.reach.get(object) != Some(digest) {
                moved += 1;
            }
        }
        self.reach = digests.iter().copied().collect();
        self.reach_moved = if comparable {
            moved
        } else {
            digests.len() as u64
        };
        self.reach_total = digests.len() as u64;
        (self.reach_moved, self.reach_total)
    }

    /// How many objects' reachability projection moved on the last link.
    pub fn reachability_moved(&self) -> (u64, u64) {
        (self.reach_moved, self.reach_total)
    }

    /// Take the cache held for `path`, if this session produced one.
    ///
    /// Taken rather than borrowed: the link consumes the previous cache and
    /// produces the next, and handing out a reference would mean cloning a
    /// structure that contains the whole output image.
    pub fn take_cache(&mut self, path: &Path) -> Option<blinker_cache::LinkCache> {
        match &self.cache {
            Some((held, _)) if held == path => self.cache.take().map(|(_, cache)| cache),
            _ => None,
        }
    }

    /// The cache held for `path`, for writing it out.
    pub fn cache_for(&self, path: &Path) -> Option<&(PathBuf, blinker_cache::LinkCache)> {
        self.cache.as_ref().filter(|(held, _)| held == path)
    }

    /// Hold the cache this link produced, and say whether it must also be
    /// written to disk.
    ///
    /// True exactly once per session per path — the first link. After that the
    /// file would only ever be re-read by this same process, which now has the
    /// structure in memory. A restart still finds a usable file; it is simply
    /// one link old, which costs one colder link and never a wrong one, because
    /// every cache is validated against its inputs before it is believed.
    pub fn store_cache(&mut self, path: &Path, cache: blinker_cache::LinkCache) -> bool {
        let first = self.cache_written.as_deref() != Some(path);
        self.cache_written = Some(path.to_path_buf());
        self.cache = Some((path.to_path_buf(), cache));
        first
    }

    /// How many interfaces moved, and the first one that did.
    pub fn interface_changes(&self) -> (u64, Option<&Path>) {
        (
            self.interface_changes,
            self.first_interface_change.as_deref(),
        )
    }
}

/// A member's identity as a path, so it cannot collide with its archive's.
fn member_path(archive: &Path, member: u32) -> PathBuf {
    archive.join(format!("({member})"))
}

/// A digest of what an object defines and what it leaves undefined.
///
/// Names only. Addresses, sizes, section contents and relocations are all
/// invisible here by design: they are what an edit changes, and none of them
/// can change which archive member defines a name.
/// Whether a name is one LLVM invented and made unique per module.
///
/// LLVM promotes an internal constant that must be addressable to a module-level
/// symbol, and gives it a name nobody chose:
/// `_anon.ed7a2420ca2a47dccd3066b2a97f7049.4.llvm.16877640159684202088`. The
/// trailing component is a hash of *the module*, so editing one line of one
/// function renames every such symbol in the crate.
///
/// This is why an ordinary body edit looked like an interface change. Editing a
/// single literal in `blinker-diagnostics` renamed 29 of the rlib's 134 global
/// symbols — none of which any other crate could name, because the names are
/// unpredictable by construction. The extraction frontier was refusing to
/// replay because a set of symbols changed that no consumer outside the crate
/// can reference.
///
/// The exclusion is sound for the same reason the local exclusion above is:
/// nothing outside this object's own crate can refer to one, so renaming them
/// cannot change which archive member satisfies anybody's reference. The
/// definition and every reference to it live in the same rlib and are recompiled
/// together, so they are renamed consistently or not at all.
fn is_module_unique(name: &str) -> bool {
    name.contains(".llvm.")
}

/// An archive's symbol table, less the names only its own crate can use.
///
/// What the frontier asks an archive is "which member defines this name", and
/// it only ever asks about names some *other* input left undefined. A
/// module-unique name can have no such reference, so its entry is noise that
/// changes on every edit. See `is_module_unique`.
fn external_symbol_map(
    map: &[(String, blinker_archive::MemberId)],
) -> Vec<(String, blinker_archive::MemberId)> {
    map.iter()
        .filter(|(name, _)| !is_module_unique(name))
        .cloned()
        .collect()
}

fn interface_digest(parsed: &ParsedObject) -> u64 {
    use std::hash::{Hash, Hasher};
    // Order-independent, because a re-parse may list symbols in a different
    // order without meaning anything different — combined with `wrapping_add`
    // rather than a sequential hash for exactly that reason.
    let mut total = 0u64;
    for symbol in &parsed.symbols {
        // Locals are excluded, and that is the whole point rather than an
        // optimisation. A local cannot satisfy another object's reference and
        // cannot be referenced from outside, so it is invisible to the
        // frontier — but it is exactly what an ordinary edit adds and removes.
        // Counting them made every edit look like an interface change, which
        // is correct and answers a different question than the one being asked.
        if symbol.visibility == blinker_macho::SymbolVisibility::Local {
            continue;
        }
        // And names LLVM made unique per module; see `is_module_unique`.
        if is_module_unique(&symbol.name) {
            continue;
        }
        let mut hasher = crate::hashing::FastHasher::default();
        symbol.name.hash(&mut hasher);
        symbol.strength.is_definition().hash(&mut hasher);
        total = total.wrapping_add(hasher.finish());
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_test_support::Scratch;

    fn exports(names: &[&str]) -> Arc<StubExports> {
        let mut exports = StubExports::default();
        let library = exports.library("/usr/lib/libSystem.B.dylib");
        for name in names {
            exports.export(library, name.to_string());
        }
        Arc::new(exports)
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
            session.stub_exports(&stubs).map(|e| e.count()),
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

        // A changed input list no longer throws the session away. rustc
        // renames every object of a recompiled crate on every debug build, so
        // "the list changed" is the normal case and not an exceptional one
        // (finding 144). What protects the positional ids is the check in
        // `load_objects`, which serves a held parse only under the id this
        // link would assign it.
        session.begin(&[path.clone(), scratch.join("b.o")]);
        assert!(
            session.object(&path).is_some(),
            "an input that is still in the list was discarded with the list"
        );

        // What does not survive is the input that stops being linked. It
        // takes two links to go: the content index is pruned by *use*, not by
        // the input list, because surviving a link whose paths all changed is
        // the whole reason it exists (finding 145). So one link that does not
        // touch it marks it, and the next drops it.
        session.begin(&[scratch.join("b.o")]);
        session.begin(&[scratch.join("b.o")]);
        assert!(
            session.object(&path).is_none(),
            "an input nothing has linked for two links was still held — the \
             content index grows with every build rather than with the program"
        );
    }

    /// The property the content index exists for: the same bytes under a new
    /// name are the same parse.
    ///
    /// rustc renames every object of a recompiled crate on every debug build,
    /// so this is not an edge case — it is what the inner loop does.
    #[test]
    fn the_same_bytes_under_a_new_name_are_the_same_parse() {
        let scratch = Scratch::dir("session-renamed-content").expect("scratch");
        let first = scratch.join("a.0aaaaaa.rcgu.o");
        let second = scratch.join("a.0bbbbbb.rcgu.o");
        std::fs::write(&first, vec![7u8; 128]).expect("written");
        std::fs::write(&second, vec![7u8; 128]).expect("same bytes, new name");

        let mut session = Session::default();
        session.begin(std::slice::from_ref(&first));
        let backing = Arc::new(Backing::Heap(vec![7u8; 128]));
        let parsed = Arc::new(ParsedObject {
            id: blinker_macho::ObjectId(0),
            architecture: blinker_macho::Architecture::Arm64,
            subsections_via_symbols: true,
            metadata: blinker_macho::ObjectMetadata {
                path: first.clone(),
                member: None,
                file_size: 128,
                has_debug_info: false,
                has_unwind_info: false,
            },
            sections: Vec::new(),
            symbols: Vec::new(),
            relocations: Vec::new(),
        });
        session.store_object(&first, &parsed, &backing);

        session.begin(std::slice::from_ref(&second));
        let held = session.object(&second);
        assert!(
            held.is_some(),
            "a renamed object with identical bytes was parsed again"
        );
        assert!(
            Arc::ptr_eq(&held.expect("held").0, &parsed),
            "it was served, but not as the same parse — the per-parse memo \
             keys on the pointer, so a copy would miss every derived fact"
        );
        assert_eq!(session.content_hits(), 1);
    }

    #[test]
    fn the_extraction_order_survives_a_rename_but_not_a_changed_archive_set() {
        let scratch = Scratch::dir("session-whole-list").expect("scratch");
        let object = scratch.join("a.0aaaaaa.rcgu.o");
        let archive = scratch.join("libx-0123456789abcdef.rlib");
        std::fs::write(&object, vec![0u8; 128]).expect("written");
        std::fs::write(&archive, vec![0u8; 128]).expect("written");

        let mut session = Session::default();
        session.begin(&[object.clone(), archive.clone()]);
        session.store_extraction(vec![archive.clone()], vec![(0, 0)]);
        assert!(
            session.extraction(std::slice::from_ref(&archive)).is_some(),
            "it was not held"
        );

        // The loose object is renamed, which is what a debug rebuild does. The
        // archives are untouched, so the order still means what it meant.
        let renamed = scratch.join("a.0bbbbbb.rcgu.o");
        session.begin(&[renamed, archive.clone()]);
        assert!(
            session.extraction(std::slice::from_ref(&archive)).is_some(),
            "a rename of something that is not an archive discarded the \
             extraction order"
        );

        // A different set of archives, though, renumbers what the order is
        // written in terms of.
        let other = scratch.join("liby-fedcba9876543210.rlib");
        assert!(
            session.extraction(&[other, archive]).is_none(),
            "an order indexed against one archive list was replayed against \
             another"
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
