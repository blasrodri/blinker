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

use crate::hashing::FastMap;
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
    /// This parse's [`interface_digest`]. See [`Session::interface_of`].
    interface: Option<u64>,
    /// The last link that used this parse, for the same window `used_at`
    /// applies to inputs. Holding the parse and dropping what was derived from
    /// it leaves the second-largest half of the work in place: the atoms and
    /// the interned name vectors are what `dead_strip` and `resolve` read.
    used_at: u64,
}

impl Memo {
    fn new(parse: &Arc<ParsedObject>) -> Memo {
        Memo {
            _parse: Arc::clone(parse),
            boundaries: FastMap::default(),
            atoms: None,
            interned: None,
            used_at: 0,
            interface: None,
        }
    }
}

/// Hash whatever names the interning table has gained since the last call.
///
/// `SymbolNames` only ever appends, so the length difference *is* the set of
/// new names — no bookkeeping beyond the two lengths.
///
/// Done once per link rather than once per object, which is what makes it
/// worth spreading over the cores: a cold link introduces 477,532 names and
/// 148 ms of BLAKE3, and an object introduces about a hundred and seventy —
/// far too few to be worth starting a thread for.
fn catch_up(digests: &mut Arc<Vec<blinker_cache::NameHash>>, names: &SymbolNames) {
    if digests.len() == names.len() {
        return;
    }
    let fresh: Vec<&str> = (digests.len()..names.len())
        .map(|index| {
            names
                .resolve(SymbolNameId(index as u32))
                .unwrap_or_default()
        })
        .collect();
    let hashed = crate::parallel::map_chunks(&fresh, |_, chunk| {
        chunk
            .iter()
            .map(|name| blinker_cache::name_digest(name))
            .collect::<Vec<_>>()
    });
    let digests = Arc::make_mut(digests);
    for chunk in hashed {
        digests.extend(chunk);
    }
}

/// One input, as this process last saw it.
enum Entry {
    Object(Arc<ParsedObject>, Arc<Backing>),
    Archive(Arc<blinker_archive::ArchiveIndex>, Arc<Backing>),
}

/// A parsed archive member, by the archive it came from and its position in it.
///
/// Held separately from the archive rather than inside it because members are
/// parsed lazily, in extraction rounds, long after the archive was indexed —
/// and because an archive that changes must take its members with it, which is
/// what the generation counter below does in one assignment.
type MemberKey = (PathBuf, u32);

/// A parsed member, where it sits in its archive, and the archive's bytes.
///
/// The bytes travel with the parse because that is what lets a later link prove
/// the member is still the same member. See [`Session::member`].
type HeldMember = (Arc<ParsedObject>, std::ops::Range<usize>, Arc<Backing>);

/// The archives an extraction order is indexed against, and the order itself.
///
/// `(archive position, member)` means nothing against a different set of
/// archives, so the two travel together.
type ExtractionOrder = (Vec<PathBuf>, Vec<(usize, u32)>);

/// The SDK stub files an exports set was read from, and the set itself.
type HeldStubs = (Vec<(PathBuf, blinker_cache::InputKey)>, Arc<StubExports>);

/// How many links an input survives without being mentioned.
///
/// Not one, which is what it was: a daemon serving a workspace alternates
/// between its targets, and a window of one means each switch empties what the
/// other just filled. Not unbounded either — a session that never forgets holds
/// every program the daemon has ever linked, and these are mapped input files.
///
/// Four covers the shape the number exists for — a test binary, a build script,
/// an executable, an example — and still drops a target that a build has moved
/// on from within a few links.
const RETAINED_LINKS: u64 = 4;

/// A few answers from recent links, keyed by which link asked.
///
/// Every one of these was a single slot, which is correct for a linker that
/// serves one program and silently wrong for a daemon that serves several: the
/// slot holds whichever target linked last, so an alternating pair misses it
/// every time. Two of them were *only* a miss; `imports` was worse, because its
/// guard asks whether any interface moved and nothing there knows the target
/// changed at all. What kept that safe was the eviction this finding removed —
/// a different target emptied the session, so nothing was held to serve
/// wrongly (finding 188).
///
/// Small and linear because it is: three entries, compared by a `u64`.
struct Recent<V> {
    held: Vec<(u64, V)>,
}

impl<V> Default for Recent<V> {
    /// Written out because the derive would require `V: Default`, which an
    /// empty map does not need and a held reachability graph cannot supply.
    fn default() -> Self {
        Recent { held: Vec::new() }
    }
}

impl<V> Recent<V> {
    fn get(&self, key: u64) -> Option<&V> {
        self.held
            .iter()
            .find(|(held, _)| *held == key)
            .map(|(_, value)| value)
    }

    /// Remove an entry and hand it over.
    ///
    /// The reachability state is updated in place and put back, which is worth
    /// twenty megabytes of copying a link — and it is also what keeps it. This
    /// map evicts the oldest *insertion*, so a target whose answer stays valid
    /// and is only ever read would be the one thrown away; taking and putting
    /// back makes every use an insertion.
    fn take(&mut self, key: u64) -> Option<V> {
        let at = self.held.iter().position(|(held, _)| *held == key)?;
        Some(self.held.remove(at).1)
    }

    fn put(&mut self, key: u64, value: V) {
        self.held.retain(|(held, _)| *held != key);
        self.held.push((key, value));
        if self.held.len() > RECENT_TARGETS {
            self.held.remove(0);
        }
    }

    fn clear(&mut self) {
        self.held.clear();
    }
}

/// How many targets keep their per-link answers.
const RECENT_TARGETS: usize = 3;

/// How many finished images a session holds at once.
///
/// Much smaller than [`RETAINED_LINKS`] because the unit is different: an input
/// is a mapped file the page cache owns anyway, and one of these is a 178 MB
/// binary on the heap. Two is what an alternating pair needs; three leaves room
/// for the third target of a workspace without holding half a gigabyte for the
/// fourth.
const RETAINED_CACHES: usize = 3;

/// Parsed inputs held across links.
///
/// Created once by a resident linker and passed to every link it performs. A
/// one-shot link uses [`Session::default`] and gets exactly the previous
/// behaviour: every probe misses, and nothing is retained past the call.
#[derive(Default)]
pub struct Session {
    /// Which link each held input was last part of.
    ///
    /// This was the previous link's argument vector, and anything not in the
    /// *current* one was dropped. That is right for a linker serving one
    /// program and wrong for a daemon, which is what it became: a workspace
    /// alternates between a test binary, a build script and the executable,
    /// and two targets with disjoint inputs empty each other's parses on every
    /// switch. Measured, that took a rust-analyzer relink from 271 ms to 560 —
    /// held 340 inputs to held 0 — and no benchmark here linked more than one
    /// program, so nothing said so (finding 188).
    ///
    /// Keeping a stamp per path instead lets an input survive the links that
    /// do not mention it, and [`RETAINED_LINKS`] decides how many.
    used_at: FastMap<PathBuf, u64>,
    /// How many links this session has served.
    generation: u64,
    /// Which target this link is for: a digest of its input list. What makes
    /// an answer from a previous link belong to this one.
    target: u64,
    entries: FastMap<PathBuf, (blinker_cache::InputKey, Entry)>,
    /// The SDK's exported symbols, and the stub files they came from.
    ///
    /// Kept whole rather than per file: it is one answer to one question —
    /// "which names does the system provide?" — and the files behind it are
    /// part of the SDK, so they change when Xcode changes and not otherwise.
    stubs: Recent<HeldStubs>,
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
    /// The archive's bytes travel with the parse so a later link can prove the
    /// member is still the same bytes. See [`Session::member`].
    members: FastMap<MemberKey, HeldMember>,
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
    extraction: Recent<ExtractionOrder>,
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
    /// A digest of each archive's symbol table as last read: name to defining
    /// member, and nothing else.
    ///
    /// Not the whole `ArchiveIndex`. That carries every member's offset and
    /// size, which move whenever any member's *content* changes — so comparing
    /// it rejected every edit, which is correct and useless. What the frontier
    /// asks an archive is "which member defines this name", and that is this
    /// table.
    ///
    /// A digest and not the table, because the only question ever asked of it
    /// is whether it is the one held. Keeping the table meant cloning every
    /// symbol name of every re-read archive to build the copy to compare
    /// against — and then keeping that copy for the next link to compare
    /// against in turn.
    indexes: FastMap<PathBuf, u64>,
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
    cache_written: crate::hashing::FastSet<PathBuf>,
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
    used_content: FastMap<[u8; 32], u64>,
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
    /// `blake3(name)` per interned name, indexed by `SymbolNameId`.
    ///
    /// Kept in step with `names` and filled at the same moment, so it is the
    /// same "once per distinct name, ever" as the interning itself. The
    /// address table asks for half a million of these per link and the cache's
    /// dependency lists for as many again; computing them from the text each
    /// time was 140 ms of a link whose point is that the text did not change.
    digests: Arc<Vec<blinker_cache::NameHash>>,
    /// This target's reachability, as the last link left it.
    ///
    /// One structure rather than the two it replaces — a strip keyed by the
    /// whole projection vector, and a separate map of digests for the report.
    /// Those answered "is last time's answer still exactly right?", which one
    /// changed object in 5,637 says no to. This holds the state the answer was
    /// derived from, so a later link can update it instead of asking.
    ///
    /// See [`crate::reachability::ReachState`] for why the atom numbering it
    /// carries has to travel with it.
    reach: Recent<crate::reachability::ReachState>,
    /// This target's `__LINKEDIT` string table, as the last link left it.
    ///
    /// Per target for the reason the reachability state is: the offsets in it
    /// are only meaningful alongside the symbol table that refers to them, and
    /// two targets do not share one.
    strings: Recent<blinker_output::symtab::StringTable>,
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
    ///
    /// One *per output*, and it was one slot. Two targets alternating missed
    /// it every time, so every link decoded the file back off disk (29 ms) and
    /// wrote it out again (76-91 ms) — the same single-slot mistake
    /// `cache_written` below describes having already made once, made again in
    /// the field beside it (finding 188).
    ///
    /// Bounded separately from everything else here, and tightly: each of
    /// these holds a finished binary, 178 MB on a debug rust-analyzer link.
    caches: FastMap<PathBuf, (u64, blinker_cache::LinkCache)>,
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
    imports: Recent<Vec<String>>,
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
    /// `target` identifies the program, not the input list. It comes from
    /// `request_hash`, which covers the output's identifier, the entry symbol,
    /// the options and the dylibs — and deliberately not the objects, because
    /// rustc renames those on every debug build and a key that moved with them
    /// would discard the extraction order on exactly the link it exists for
    /// (finding 144). Keying these by the input list was tried first and broke
    /// that test, which is what the test is for.
    pub fn begin(&mut self, inputs: &[PathBuf], target: u64) {
        self.generation += 1;
        let now = self.generation;
        self.target = target;
        for path in inputs {
            match self.used_at.get_mut(path) {
                Some(stamp) => *stamp = now,
                None => {
                    self.used_at.insert(path.clone(), now);
                }
            }
        }
        // An input survives the links that do not mention it, for a while. The
        // window is what makes alternating targets work; the bound is what
        // stops a long-lived daemon holding every program it has ever linked.
        let keep = now.saturating_sub(RETAINED_LINKS);
        self.used_at.retain(|_, stamp| *stamp > keep);
        let held = &self.used_at;
        let surviving = |path: &Path| held.contains_key(path);
        self.entries.retain(|path, _| surviving(path));
        self.members.retain(|(archive, _), _| surviving(archive));
        self.interfaces.retain(|path, _| {
            // A member's interface is filed under a synthetic path inside
            // its archive, so it survives with the archive rather than on
            // its own.
            surviving(path) || path.parent().is_some_and(surviving)
        });
        self.indexes.retain(|path, _| surviving(path));
        // Contents no recent link looked at are dropped now, so a parse
        // survives exactly as long as something keeps linking it — over the
        // same window, because the whole point of this index is to serve an
        // input whose path changed, and a path stamp cannot speak for it.
        self.used_content.retain(|_, stamp| *stamp > keep);
        let recent = &self.used_content;
        self.by_content.retain(|hash, _| recent.contains_key(hash));
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
    fn current(&mut self, path: &Path, now: Option<&blinker_cache::InputKey>) -> Option<&Entry> {
        let current =
            now.is_some_and(|now| self.entries.get(path).is_some_and(|(key, _)| key == now));
        self.count(current);
        current.then(|| &self.entries.get(path).expect("just checked").1)
    }

    /// A parsed object for `path`, or `None` to parse it.
    ///
    /// `key` is what probing `path` produced, which the caller supplies rather
    /// than this working it out. A probe is a `stat` for a content-addressed
    /// path and a read and a BLAKE3 for one of rustc's, and 133 loose objects
    /// is 22 MB hashed — on one thread, before anything else can start,
    /// because the session cannot be shared across the readers that follow.
    /// Hashing a file touches nothing shared, so the caller does every probe
    /// at once and this is left with the map work (finding 182).
    ///
    /// It also stops the file being probed *twice*. The comment below has
    /// always said `current` has just probed it — and then the code probed it
    /// again, hashing the same bytes a second time, for every object whose
    /// path had moved. rustc renames every object of a recompiled crate, so
    /// "every object whose path had moved" is the ordinary case.
    pub fn object(
        &mut self,
        path: &Path,
        key: Option<&blinker_cache::InputKey>,
    ) -> Option<(Arc<ParsedObject>, Arc<Backing>)> {
        if let Some((blinker_cache::InputKey::Content(hash), _)) = self.entries.get(path) {
            let hash = *hash;
            self.used_content.insert(hash, self.generation);
        }
        if let Some(entry) = self.current(path, key) {
            return match entry {
                Entry::Object(parsed, backing) => Some((Arc::clone(parsed), Arc::clone(backing))),
                Entry::Archive(..) => None,
            };
        }
        // Missed by path — which for one of rustc's objects means very little,
        // because it renames them all on every build. The probe the caller
        // made is in hand, and for a path that is not evidence that probe is a
        // hash of its bytes, so asking the content index costs nothing more
        // than the lookup.
        let Some(blinker_cache::InputKey::Content(hash)) = key.cloned() else {
            return None;
        };
        let (parsed, backing) = self.by_content.get(&hash)?;
        let (parsed, backing) = (Arc::clone(parsed), Arc::clone(backing));
        self.used_content.insert(hash, self.generation);
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
        key: Option<&blinker_cache::InputKey>,
    ) -> Option<(Arc<blinker_archive::ArchiveIndex>, Arc<Backing>)> {
        match self.current(path, key)? {
            Entry::Archive(index, backing) => Some((Arc::clone(index), Arc::clone(backing))),
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
            self.used_content.insert(*hash, self.generation);
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
    /// A member of `archive` this process has already parsed, if `fresh` is
    /// byte-for-byte the bytes it was parsed from.
    ///
    /// The comparison is the whole of the safety argument, and it replaces
    /// throwing every member away when its archive was re-read. rustc renames
    /// every codegen unit of a recompiled crate, so a crate *downstream* of an
    /// edit produces an rlib that differs from the last one only in the names
    /// inside it — 256 of 256 members byte-identical, at the same index. That
    /// dropped 3,373 of a link's 5,637 objects and re-parsed them all.
    ///
    /// A `memcmp` and not a digest because both sides are already mapped: there
    /// is nothing to gain by hashing 400 MB to avoid comparing it, and a
    /// comparison cannot collide.
    pub fn member(
        &self,
        archive: &Path,
        member: u32,
        fresh: &[u8],
    ) -> Option<(Arc<ParsedObject>, std::ops::Range<usize>)> {
        let (parsed, range, backing) = self.members.get(&(archive.to_path_buf(), member))?;
        (backing.get(range.clone())? == fresh).then(|| (Arc::clone(parsed), range.clone()))
    }

    /// Remember a freshly parsed archive member and where it sits.
    pub fn store_member(
        &mut self,
        archive: &Path,
        member: u32,
        parsed: &Arc<ParsedObject>,
        range: std::ops::Range<usize>,
        data: &Arc<Backing>,
    ) {
        // A member's interface is noted under a path of its own, so two
        // members of one archive cannot overwrite each other's digest.
        let named = member_path(archive, member);
        self.note_interface(&named, parsed);
        self.members.insert(
            (archive.to_path_buf(), member),
            (Arc::clone(parsed), range, Arc::clone(data)),
        );
    }

    /// Remember a freshly indexed archive.
    pub fn store_archive(
        &mut self,
        path: &Path,
        index: &Arc<blinker_archive::ArchiveIndex>,
        data: &Arc<Backing>,
        symbols: u64,
    ) {
        let Some(key) = blinker_cache::InputKey::probe(path) else {
            return;
        };
        // The members are *not* dropped here. They used to be, on the argument
        // that a re-read archive's contents are gone — which is true of the
        // bytes and not of what was parsed out of them. Each one now carries
        // the bytes it was parsed from, and [`Session::member`] serves it only
        // after proving the new archive holds the same bytes at that index.
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
        if self.indexes.get(path) != Some(&symbols) {
            self.interfaces_changed = true;
            self.interface_changes += 1;
            if self.first_interface_change.is_none() {
                self.first_interface_change = Some(path.to_path_buf());
            }
        }
        self.indexes.insert(path.to_path_buf(), symbols);
        self.entries.insert(
            path.to_path_buf(),
            (key, Entry::Archive(Arc::clone(index), Arc::clone(data))),
        );
    }

    /// The SDK's exports, if the stub files behind them are unchanged.
    pub fn stub_exports(&self, stubs: &[PathBuf]) -> Option<Arc<StubExports>> {
        let (recorded, exports) = self.stubs.get(self.target)?;
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
        // The target key is not an optimisation. The two conditions before it
        // ask whether anything *moved*, and neither can tell that this is a
        // different program: a link whose inputs are all held reports nothing
        // changed, and would be handed the other target's imports.
        if self.interfaces_changed || self.stubs_reparsed {
            return None;
        }
        let target = self.target;
        self.imports.get(target)?;
        self.held_resolution = true;
        self.imports.get(target).map(Vec::as_slice)
    }

    /// Remember what resolution decided.
    pub fn store_imports(&mut self, imports: &[String]) {
        self.imports.put(self.target, imports.to_vec());
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
        self.stubs.put(self.target, (recorded, exports));
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
        let (recorded, order) = self.extraction.get(self.target)?;
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
    fn note_interface_unchanged(&mut self, path: &Path, parsed: &Arc<ParsedObject>) {
        let digest = self.interface_of(parsed);
        self.interfaces.insert(path.to_path_buf(), digest);
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
        self.extraction.clear();
        self.replayed_extraction = false;
        self.interfaces_changed = true;
    }

    /// This parse's interface digest, computed if it is not already held.
    ///
    /// Held per parse rather than per path because it is a function of the
    /// parse and nothing else — the same bytes arriving under a second name
    /// have the same interface, and a parse the session already holds has
    /// already answered this.
    fn interface_of(&mut self, parse: &Arc<ParsedObject>) -> u64 {
        let key = Arc::as_ptr(parse) as usize;
        let entry = self.memo.entry(key).or_insert_with(|| Memo::new(parse));
        *entry
            .interface
            .get_or_insert_with(|| interface_digest(parse))
    }

    /// Work out a batch of parses' interface digests on every core.
    ///
    /// Storing an input notes its interface, and a digest walks every global
    /// symbol the object has — so a cold link's 5,504 archive members were
    /// 46 ms of name hashing done one member at a time, on the thread that had
    /// just finished parsing all of them in parallel. The digests do not depend
    /// on each other and none of them is wanted until the next round, so they
    /// are worked out together and read back out of the memo.
    ///
    /// Seeding is not required for correctness: a parse that misses is digested
    /// where it is asked for, exactly as before.
    pub(crate) fn seed_interfaces(&mut self, parses: &[Arc<ParsedObject>]) {
        let missing: Vec<&Arc<ParsedObject>> = parses
            .iter()
            .filter(|parse| {
                !self
                    .memo
                    .get(&(Arc::as_ptr(parse) as usize))
                    .is_some_and(|held| held.interface.is_some())
            })
            .collect();
        if missing.len() < 2 {
            return;
        }
        let digested = crate::parallel::map_chunks(&missing, |_, chunk| {
            chunk
                .iter()
                .map(|parse| interface_digest(parse))
                .collect::<Vec<_>>()
        });
        for (parse, digest) in missing.iter().zip(digested.into_iter().flatten()) {
            let key = Arc::as_ptr(*parse) as usize;
            let entry = self.memo.entry(key).or_insert_with(|| Memo::new(parse));
            entry.interface = Some(digest);
        }
    }

    /// Note an input's symbol interface, and whether it moved.
    fn note_interface(&mut self, path: &Path, parsed: &Arc<ParsedObject>) {
        let digest = self.interface_of(parsed);
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
        self.extraction.put(self.target, (archives, order));
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

    /// This object's projection if one is already held, without computing it.
    ///
    /// Split from `atoms` so the ones that are *not* held can be computed on
    /// every core: a cold link projects all 5,637 objects, and a projection
    /// reads one object and nothing else. The session cannot be shared across
    /// threads, so the question and the work are asked separately.
    pub(crate) fn held_atoms(
        &self,
        parse: &Arc<ParsedObject>,
    ) -> Option<Arc<crate::reachability::ObjectAtoms>> {
        let key = Arc::as_ptr(parse) as usize;
        self.memo.get(&key)?.atoms.clone()
    }

    /// File a projection computed elsewhere.
    pub(crate) fn store_atoms(
        &mut self,
        parse: &Arc<ParsedObject>,
        atoms: Arc<crate::reachability::ObjectAtoms>,
    ) {
        let key = Arc::as_ptr(parse) as usize;
        let memo = self.memo.entry(key).or_insert_with(|| Memo::new(parse));
        memo.atoms = Some(atoms);
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

    /// Intern a batch of parses' names, answering everything answerable on
    /// every core first.
    ///
    /// [`Session::interned`] interns one name at a time, and that loop is 117 ms
    /// of a cold link's 976,000 names. Almost none of it is work: hashing all
    /// of them takes 7 ms, and the other 110 is *waiting* — a name's bucket, the
    /// span that bucket names, and the arena text that span points at are three
    /// loads that each need the last one's answer, and an iteration that may
    /// insert into the table is a barrier the next name's chain cannot start
    /// across. One name in flight at a time, a million times over.
    ///
    /// So the answerable half is lifted out. Hashing a name and asking
    /// [`SymbolNames::get_hashed`] for it touch nothing and depend on nothing,
    /// so a whole round's worth goes to every core at once; what comes back is
    /// an id for every name the table already held. Only the names that were new
    /// are left, and those are filed in order, serially, because which id each
    /// one gets depends on how many came before it.
    ///
    /// The ids come out exactly as they did. `parses` is in the order the round
    /// settled on before any thread started, the walk below follows it, and a
    /// name that was absent when the cores looked is interned here in that same
    /// order — including the second occurrence of a name this batch introduces,
    /// which finds the id the first occurrence just created.
    ///
    /// Seeding is not required for correctness: a parse that misses is interned
    /// where it is asked for, exactly as before.
    pub(crate) fn seed_interned(&mut self, parses: &[Arc<ParsedObject>]) {
        let missing: Vec<&Arc<ParsedObject>> = parses
            .iter()
            .filter(|parse| {
                !self
                    .memo
                    .get(&(Arc::as_ptr(parse) as usize))
                    .is_some_and(|held| held.interned.is_some())
            })
            .collect();
        if missing.len() < 2 {
            return;
        }
        let Session { memo, names, .. } = self;
        let held: &SymbolNames = names;
        let probed = crate::parallel::map_chunks(&missing, |_, chunk| {
            chunk
                .iter()
                .map(|parse| {
                    parse
                        .symbols
                        .iter()
                        .map(|symbol| {
                            let hash = blinker_symbols::hash_of(&symbol.name);
                            (hash, held.get_hashed(&symbol.name, hash))
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        });
        for (parse, answers) in missing.iter().zip(probed.into_iter().flatten()) {
            let ids: Vec<SymbolNameId> = parse
                .symbols
                .iter()
                .zip(answers)
                .map(|(symbol, (hash, found))| {
                    found.unwrap_or_else(|| names.intern_hashed(&symbol.name, hash))
                })
                .collect();
            let key = Arc::as_ptr(*parse) as usize;
            let entry = memo.entry(key).or_insert_with(|| Memo::new(parse));
            entry.interned = Some(Arc::new(ids));
        }
    }

    /// The interning table these ids belong to.
    pub(crate) fn names(&self) -> &SymbolNames {
        &self.names
    }

    /// `blake3(name)` for every interned name, indexed by `SymbolNameId`.
    ///
    /// Handed out as an `Arc` rather than borrowed: the two callers sit either
    /// side of a step that needs the session mutably, and a shared count is
    /// cheaper than arranging the borrows around it. The clone is released at
    /// the end of the link, so the next one extends in place.
    pub(crate) fn digests(&mut self) -> Arc<Vec<blinker_cache::NameHash>> {
        catch_up(&mut self.digests, &self.names);
        Arc::clone(&self.digests)
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
        let now = self.generation;
        for key in used.keys() {
            if let Some(memo) = self.memo.get_mut(key) {
                memo.used_at = now;
            }
        }
        let keep = now.saturating_sub(RETAINED_LINKS);
        self.memo.retain(|_, memo| memo.used_at > keep);
    }

    /// This target's reachability as the previous link left it.
    ///
    /// Per target, because an object id is a position in *this* link's input
    /// list and means a different object in another's — and because the atom
    /// numbering a live set is expressed in is a running total over one link's
    /// objects. Sharing either across targets would not merely be a wrong
    /// number; it would be a live set read against the wrong graph.
    /// Taken, not borrowed: a link updates the graph in place and puts it
    /// back, which is worth twenty megabytes of copying. A link that reuses the
    /// answer whole still has to store it again — see `store_reach`.
    pub(crate) fn reach_state(&mut self) -> Option<crate::reachability::ReachState> {
        let target = self.target;
        self.reach.take(target)
    }

    /// Replace it with what this link computed.
    pub(crate) fn store_reach(&mut self, state: crate::reachability::ReachState) {
        self.reach.put(self.target, state);
    }

    /// This target's string table, or an empty one for a target never linked
    /// here. Taken rather than borrowed for the reason `reach_state` is: it is
    /// appended to and handed back, not copied.
    pub(crate) fn take_strings(&mut self) -> blinker_output::symtab::StringTable {
        let target = self.target;
        self.strings.take(target).unwrap_or_default()
    }

    pub(crate) fn store_strings(&mut self, strings: blinker_output::symtab::StringTable) {
        if !retain_strings() {
            return;
        }
        self.strings.put(self.target, strings);
    }

    /// Record how much of the reachability graph moved, for the report.
    ///
    /// A digest that moved means some atom boundary or some edge in that object
    /// changed; if none moved, the live set cannot have changed and the whole
    /// strip is reusable.
    pub fn note_reachability(&mut self, moved: u64, total: u64) {
        self.reach_moved = moved;
        self.reach_total = total;
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
        self.caches.remove(path).map(|(_, cache)| cache)
    }

    /// The cache held for `path`, for writing it out.
    pub fn cache_for(&self, path: &Path) -> Option<&blinker_cache::LinkCache> {
        self.caches.get(path).map(|(_, cache)| cache)
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
        let first = self.cache_written.insert(path.to_path_buf());
        self.caches
            .insert(path.to_path_buf(), (self.generation, cache));
        // Least recently stored first, and only ever one over: each of these is
        // a finished binary, so the bound is about memory and not about hit
        // rate.
        while self.caches.len() > RETAINED_CACHES {
            let Some(oldest) = self
                .caches
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.caches.remove(&oldest);
        }
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

/// A digest of an archive's symbol table, less the names only its own crate
/// can use.
///
/// What the frontier asks an archive is "which member defines this name", and
/// it only ever asks about names some *other* input left undefined. A
/// module-unique name can have no such reference, so its entry is noise that
/// changes on every edit. See `is_module_unique`.
///
/// Order-sensitive, unlike [`interface_digest`]: this table is sorted by name
/// and the first entry for a name is the one that defines it, so two tables
/// holding the same pairs in a different order are not the same answer.
pub(crate) fn external_symbol_digest(map: &[(String, blinker_archive::MemberId)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = crate::hashing::FastHasher::default();
    for (name, member) in map.iter().filter(|(name, _)| !is_module_unique(name)) {
        name.hash(&mut hasher);
        member.0.hash(&mut hasher);
    }
    hasher.finish()
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

/// Whether the `__LINKEDIT` string table survives from one link to the next.
///
/// # Why this is not yet the default
///
/// A retained table is *correct* — every offset it hands out points at the
/// name it was given for — but it is not byte-identical to what a cold link
/// produces, and it cannot be. Its offsets are first-reference order over the
/// names of the link that built it: a name that has since gone away still holds
/// its bytes, and a name that has appeared sits at the end rather than where
/// this link first mentioned it. Every downstream byte in `__LINKEDIT` moves
/// with them.
///
/// That matters because the strongest thing the test suite does is link the
/// same program warm and cold and compare the two files byte for byte, which
/// has caught nearly every session bug worth catching. Retention breaks that
/// comparison for a measured 4.3 ms of a 361 ms link — `emit_linkedit` falls
/// from 20.9 ms to 16.5 — which is not a trade worth making on its own.
///
/// It is worth making for what it *enables*. An `NlistEntry` refers to its name
/// by offset, so a retained entry is meaningless unless the offset still is;
/// stable offsets are what would let an object that did not change keep its
/// whole run of entries instead of rebuilding them. That is the 28 ms
/// `symbols` stage and most of the 16.5 ms left in `emit_linkedit`, and when it
/// exists this turns on with it — together with an equivalence check to replace
/// the byte comparison it costs.
///
/// The same shape as `BLINKER_DELTA_LIVENESS`, and for the same reason: the
/// machinery is right and the place it was first built is not where it pays.
fn retain_strings() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BLINKER_RETAIN_STRINGS").is_some())
}

#[cfg(test)]
mod tests {
    /// One program, for the tests that are not about telling two apart.
    const TARGET: u64 = 0xa11ce;

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
        session.begin(std::slice::from_ref(&path), TARGET);
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
        assert!(
            session
                .object(&path, blinker_cache::InputKey::probe(&path).as_ref())
                .is_some(),
            "it was not held"
        );

        // A changed input list no longer throws the session away. rustc
        // renames every object of a recompiled crate on every debug build, so
        // "the list changed" is the normal case and not an exceptional one
        // (finding 144). What protects the positional ids is the check in
        // `load_objects`, which serves a held parse only under the id this
        // link would assign it.
        session.begin(&[path.clone(), scratch.join("b.o")], TARGET);
        assert!(
            session
                .object(&path, blinker_cache::InputKey::probe(&path).as_ref())
                .is_some(),
            "an input that is still in the list was discarded with the list"
        );

        // And it survives the links that do not mention it at all, which is
        // what a daemon alternating between two targets does on every switch.
        // The window is `RETAINED_LINKS`; one link inside it must not drop it.
        session.begin(&[scratch.join("b.o")], TARGET);
        assert!(
            session
                .object(&path, blinker_cache::InputKey::probe(&path).as_ref())
                .is_some(),
            "an input skipped by one link was dropped — a workspace that \
             alternates targets goes cold on every switch (finding 188)"
        );

        // What does not survive is the input that stops being linked. The
        // bound is what keeps a resident linker's memory proportional to the
        // programs it is *currently* building rather than to every program it
        // has ever built.
        for _ in 0..=RETAINED_LINKS {
            session.begin(&[scratch.join("b.o")], TARGET);
        }
        assert!(
            session
                .object(&path, blinker_cache::InputKey::probe(&path).as_ref())
                .is_none(),
            "an input nothing has linked for {RETAINED_LINKS} links was still \
             held — the session grows with every build rather than with the \
             programs being built"
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
        session.begin(std::slice::from_ref(&first), TARGET);
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

        session.begin(std::slice::from_ref(&second), TARGET);
        let held = session.object(&second, blinker_cache::InputKey::probe(&second).as_ref());
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
        session.begin(&[object.clone(), archive.clone()], TARGET);
        session.store_extraction(vec![archive.clone()], vec![(0, 0)]);
        assert!(
            session.extraction(std::slice::from_ref(&archive)).is_some(),
            "it was not held"
        );

        // The loose object is renamed, which is what a debug rebuild does. The
        // archives are untouched, so the order still means what it meant.
        let renamed = scratch.join("a.0bbbbbb.rcgu.o");
        session.begin(&[renamed, archive.clone()], TARGET);
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
        session.begin(std::slice::from_ref(&path), TARGET);
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
            session
                .object(&path, blinker_cache::InputKey::probe(&path).as_ref())
                .is_none(),
            "a rewritten object was served from memory"
        );
    }
}
