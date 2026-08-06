//! Which code and data a program can reach, and where the survivors move to.
//!
//! # Atoms
//!
//! Dead code sits inside objects that are legitimately needed — an archive
//! member is pulled in for one referenced symbol and brings its other forty
//! functions with it — so the unit has to be smaller than a section. Mach-O
//! says when that is allowed: `MH_SUBSECTIONS_VIA_SYMBOLS` is the compiler
//! asserting that nothing in the object refers to anything except through a
//! symbol, so a section may be cut at symbol boundaries without changing
//! behaviour. Every object in a Rust link sets it.
//!
//! An atom is therefore one defined symbol and the bytes from its address to
//! the next symbol's, or to the end of its section. `__eh_frame` is the one
//! exception: it carries a single anchor symbol per section, and is framed by
//! its own record lengths instead.
//!
//! # Atoms are numbered per object
//!
//! An atom's name is `(object, index within that object)`, and the flat
//! `0..n` numbering the traversal uses is that pair plus the object's base.
//! The distinction matters because a global index is not a property of the
//! atom: inserting one atom into the first object renumbers every atom in the
//! link, so nothing derived from the numbering could be held across a link.
//! Per object, the numbering only moves when that object does — which is why
//! [`ObjectAtoms`] is a pure function of one parse and can be memoised beside
//! its boundaries.
//!
//! [`ObjectAtoms`] holds everything the traversal reads about an object: its
//! atoms, the edges leaving each one, which of them are roots, and how its
//! unwind metadata points back at the code it describes. That set is exactly
//! what its digest hashes, so the memo and the invalidation key
//! describe the same thing rather than two things that have to be kept in
//! agreement.
//!
//! # Atoms do not become the unit of layout
//!
//! They could have: every input section split into one placement per atom,
//! placed individually. It is not necessary. The survivors of a section keep
//! their original relative order, so the same result comes from leaving the
//! section as one contribution and *compacting* it — closing the gaps the dead
//! atoms leave, and recording where each surviving byte moved to. [`Strip`] is
//! that map.
//!
//! What that buys is that every consumer of "where did this input byte end up"
//! keeps working with one extra lookup, instead of every consumer of
//! `Contribution` having to learn what an atom is.
//!
//! # What makes it safe
//!
//! Three rules, each checked rather than assumed:
//!
//! - **Every reference lands exactly on a symbol.** Measured across a real
//!   Rust link: all 1832 pointer relocations in `__const`, and every one
//!   elsewhere, carry an inline addend of zero. A reference into the *middle*
//!   of an atom would follow the bytes it meant only by accident once they
//!   moved, so a section that has one is kept whole.
//! - **Metadata describes code rather than using it.** `__eh_frame`,
//!   `__compact_unwind` and `__gcc_except_tab` name every function in their
//!   object; treating those edges as uses makes everything live. They are live
//!   when their subject is, which is a reverse edge.
//! - **No live atom refers to a dead one.** The propagation is supposed to
//!   guarantee that, and a final pass verifies it: anything still reachable
//!   from live bytes is revived, and the count is reported. A hole in the model
//!   then shows up as a number rather than as a corrupt binary.

use std::ops::Range;
use std::sync::Arc;

use crate::hashing::{FastMap as HashMap, FastSet as HashSet};
use crate::LoadedObject;
use blinker_macho::{
    Arm64RelocationKind, InputRelocation, InputSection, ObjectId, RelocationTarget, SectionId,
    SectionKind, SymbolId, SymbolVisibility,
};
use blinker_symbols::SymbolNameId;

/// One symbol's worth of bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Atom {
    pub object: ObjectId,
    pub section: SectionId,
    /// Offset of this atom within its input section.
    pub offset: u64,
    pub size: u64,
}

impl Atom {
    fn end(&self) -> u64 {
        self.offset + self.size
    }

    fn key(&self) -> (u32, u32) {
        (self.object.0, self.section.0)
    }
}

/// What the analysis found, counted over `__text` alone.
///
/// `__text` is two thirds of the size gap, and the only part whose number can
/// be compared against a linker that already strips correctly. What the rest
/// is worth is reported by the image the link produces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub live_atoms: usize,
    pub total_atoms: usize,
    pub live_bytes: u64,
    pub total_bytes: u64,
    /// Bytes in input sections where *no* atom is live.
    ///
    /// A strict subset of `dead_bytes`, and the part that would be reachable
    /// without compacting anything: a section nothing reaches could be dropped
    /// from placement whole. Measured because it decides whether a cheaper
    /// version of this is worth landing — it is not, at 1K of 319K (finding 71).
    pub fully_dead_section_bytes: u64,
    /// Atoms the propagation left dead that a live atom then referred to.
    ///
    /// Should be zero. Anything else is a rule in the model that the input
    /// does not obey, reported rather than trusted.
    pub revived: usize,
}

impl Report {
    pub fn dead_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.live_bytes)
    }
}

/// Whether a section describes code rather than using it.
fn is_metadata(name: &str) -> bool {
    matches!(
        name,
        "__eh_frame" | "__compact_unwind" | "__gcc_except_tab" | "__unwind_info"
    ) || name.starts_with("__debug")
        || name.starts_with("__zdebug")
}

/// Sections whose contents may be cut into atoms.
///
/// Deliberately a list rather than a rule. Everything on it is code, read-only
/// data, or a literal pool: content addressed only through the symbols that
/// name it. Writable data, the thread-local block and `__bss` are left whole —
/// they are small enough that stripping them buys nothing, and they are the
/// sections most likely to be reached by something other than a relocation.
fn is_atomizable(name: &str) -> bool {
    matches!(
        name,
        "__text"
            | "__const"
            | "__cstring"
            | "__literal4"
            | "__literal8"
            | "__literal16"
            | "__gcc_except_tab"
            | "__eh_frame"
    )
}

/// Bytes in one `__LD,__compact_unwind` record, and the fields this file reads.
const COMPACT_UNWIND_RECORD: u64 = 32;
const CU_FUNCTION: u64 = 0;
const CU_LSDA: u64 = 24;

/// Whether a relocation reads a plain addend from the bytes it patches.
///
/// Only a pointer-sized data reference does. In an instruction those bytes are
/// the instruction, and ARM64 Mach-O spells a non-zero addend there with a
/// separate `ARM64_RELOC_ADDEND` entry, which the parser refuses outright.
fn stores_addend(relocation: &InputRelocation) -> bool {
    relocation.kind == Arm64RelocationKind::Unsigned
}

/// Boundaries at which a section may be cut, or `None` to keep it whole.
fn boundaries(object: &LoadedObject, section: &InputSection) -> Option<Vec<u64>> {
    if !object.parsed.subsections_via_symbols
        || section.no_dead_strip
        || !is_atomizable(&section.name)
    {
        return None;
    }
    if section.name == "__eh_frame" {
        return eh_frame_boundaries(object, section);
    }

    let mut offsets: Vec<u64> = object
        .parsed
        .symbols
        .iter()
        .filter(|s| s.section == Some(section.id) && s.strength.is_definition())
        .map(|s| s.value.saturating_sub(section.vm_address))
        .filter(|offset| *offset < section.size)
        .collect();
    if offsets.is_empty() {
        return None;
    }
    offsets.sort_unstable();
    offsets.dedup();
    // Bytes before the first symbol are named by nothing, so nothing can refer
    // to them on their own; they are carried by the atom that follows.
    if offsets[0] != 0 {
        offsets.insert(0, 0);
    }
    Some(offsets)
}

/// `__eh_frame` is framed by its own record lengths, not by symbols.
///
/// It carries one `ltmpN` anchor per section and nothing else, so the symbol
/// rule would give it a single atom covering everything. Returning `None` when
/// the framing does not add up exactly keeps a section we cannot parse whole.
pub(crate) fn eh_frame_boundaries(
    object: &LoadedObject,
    section: &InputSection,
) -> Option<Vec<u64>> {
    let file_offset = section.file_offset?;
    let mut offsets = Vec::new();
    let mut position = 0u64;
    while position < section.size {
        if position + 8 > section.size {
            return None;
        }
        offsets.push(position);
        let at = (file_offset + position) as usize;
        let length = u32::from_le_bytes(object.data.get(at..at + 4)?.try_into().ok()?) as u64;
        if length == 0 || position + 4 + length > section.size {
            return None;
        }
        position += 4 + length;
    }
    (position == section.size).then_some(offsets)
}

/// Where a symbol sits within its own section.
fn offset_of(
    object: &LoadedObject,
    symbol: &blinker_macho::InputSymbol,
) -> Option<(SectionId, u64)> {
    let id = symbol.section?;
    let section = object.parsed.section(id)?;
    Some((id, symbol.value.saturating_sub(section.vm_address)))
}

/// One edge out of an atom, named without reference to the whole link.
///
/// A reference either lands inside the object it was written in — in which
/// case the target is one of that object's own atoms, and stays that atom
/// however the link is renumbered — or it goes out through a name, and which
/// atom that reaches is not knowable until every object has been read. Keeping
/// the second case as the *symbol* rather than as a resolved index is what
/// makes the whole edge list a pure function of one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// An atom of this same object, by its index within it.
    Local(u32),
    /// Whatever defines a name, decided globally.
    ///
    /// An index into the block's `names`, not the symbol id itself. The same
    /// name is referred to 3.1 times on average within one object — 1,195,652
    /// name edges over 390,606 distinct ones on a debug rust-analyzer link —
    /// and resolving one costs a string hash into the owners map. Indexing a
    /// deduplicated list lets the link resolve each *distinct* name once and
    /// then answer every edge by subscript.
    Name(u32),
}

/// One `__eh_frame` record and the function it describes.
#[derive(Debug, Clone, Copy)]
struct EhRecord {
    atom: u32,
    /// A CIE, which every live FDE needs and which is kept unconditionally.
    cie: bool,
    function: Option<Edge>,
}

/// Everything reachability reads about one object, in that object's numbering.
///
/// A pure function of the parse, and held beside its boundaries in the session
/// memo. On an edit, one object of a thousand has a different one (finding
/// 133) — so building this per link, for every object, was rebuilding the same
/// answer a thousand times to get one new one.
pub(crate) struct ObjectAtoms {
    /// Ascending within each section; sections in the order the object lists.
    atoms: Vec<Atom>,
    /// Section id -> that section's atoms, as local indices.
    by_section: HashMap<u32, Range<u32>>,
    /// Per atom, its edges as a range of `edges`.
    spans: Vec<(u32, u32)>,
    edges: Vec<Edge>,
    /// Per atom: its own edges do not keep anything alive.
    ///
    /// Metadata describes code rather than using it. Precomputed because the
    /// alternative is a section lookup and a string match inside the traversal,
    /// once per live atom.
    suppress: Vec<bool>,
    /// Edges out of sections this link does not split, which are roots.
    unsplit: Vec<Edge>,
    /// Atoms a symbol forbids stripping.
    never_strip: Vec<u32>,
    eh_frame: Vec<EhRecord>,
    /// `__compact_unwind`: the function a record describes, and its LSDA.
    unwind: Vec<(Option<Edge>, Edge)>,
    /// Sections of this object that must be kept whole.
    opaque: Vec<u32>,
    /// Symbols whose *defining* section must be kept whole, wherever it is.
    opaque_via: Vec<SymbolId>,
    /// Non-local definitions, and the atom that owns each.
    owned: Vec<(SymbolId, u32)>,
    /// The distinct symbols `Edge::Name` refers to, in first-use order.
    names: Vec<SymbolId>,
    /// A hash of everything above.
    ///
    /// Taken from the projection rather than from the object, so "the digest
    /// did not move" means "this object contributes exactly the atoms, edges
    /// and roots it contributed last time" by construction. The earlier digest
    /// walked the object separately and hashed what it believed reachability
    /// read, which is a claim that has to stay true as the projection changes —
    /// and was already false for the inline addends that decide opacity.
    digest: u64,
}

/// Project one object into the form the traversal consumes.
pub(crate) fn project(object: &LoadedObject) -> ObjectAtoms {
    let mut atoms: Vec<Atom> = Vec::new();
    let mut by_section: HashMap<u32, Range<u32>> = HashMap::default();
    for section in &object.parsed.sections {
        let Some(offsets) = boundaries(object, section) else {
            continue;
        };
        let start = atoms.len() as u32;
        for (index, offset) in offsets.iter().copied().enumerate() {
            let end = offsets.get(index + 1).copied().unwrap_or(section.size);
            atoms.push(Atom {
                object: object.parsed.id,
                section: section.id,
                offset,
                size: end.saturating_sub(offset),
            });
        }
        by_section.insert(section.id.0, start..atoms.len() as u32);
    }

    // The atom holding a byte, within this object alone.
    let containing = |section: SectionId, offset: u64| -> Option<u32> {
        let range = by_section.get(&section.0)?.clone();
        let slice = &atoms[range.start as usize..range.end as usize];
        let index = slice
            .partition_point(|a| a.offset <= offset)
            .checked_sub(1)?;
        (offset < slice[index].end()).then(|| range.start + index as u32)
    };

    // The atom a relocation points at, as far as this object can say.
    let edge = |relocation: &InputRelocation| -> Option<Edge> {
        let RelocationTarget::Symbol(id) = relocation.target else {
            // A section-relative target keeps its section whole, so there is no
            // single atom to name and nothing to propagate to.
            return None;
        };
        let symbol = object.parsed.symbol(id)?;
        if symbol.strength.is_definition() {
            if let Some((section, offset)) = offset_of(object, symbol) {
                if let Some(local) = containing(section, offset) {
                    return Some(Edge::Local(local));
                }
            }
        }
        // A reference resolves to whatever object defines the name.
        Some(Edge::Name(id.0))
    };

    let grouped = group_relocations(object);

    let mut spans = Vec::with_capacity(atoms.len());
    let mut edges = Vec::new();
    let mut suppress = Vec::with_capacity(atoms.len());
    for atom in &atoms {
        let start = edges.len() as u32;
        edges.extend(within(&grouped, atom).iter().filter_map(|r| edge(r)));
        spans.push((start, edges.len() as u32));
        suppress.push(object.parsed.section(atom.section).is_some_and(|s| {
            // `__eh_frame` is metadata whose references do count forward: a
            // surviving FDE brings its exception table with it.
            is_metadata(&s.name) && s.name != "__eh_frame"
        }));
    }

    // A section this link does not split still holds references, and they
    // still reach. `__data`, the thread-local block and any section in an
    // object without `MH_SUBSECTIONS_VIA_SYMBOLS` arrive here.
    let mut unsplit = Vec::new();
    for section in &object.parsed.sections {
        if by_section.contains_key(&section.id.0)
            || is_metadata(&section.name)
            || section.kind == SectionKind::Debug
        {
            continue;
        }
        if let Some(list) = grouped.get(&section.id.0) {
            unsplit.extend(list.iter().filter_map(|r| edge(r)));
        }
    }

    let mut never_strip = Vec::new();
    let mut owned = Vec::new();
    for symbol in &object.parsed.symbols {
        let Some((section, offset)) = offset_of(object, symbol) else {
            continue;
        };
        if !symbol.strength.is_definition() {
            continue;
        }
        let Some(local) = containing(section, offset) else {
            continue;
        };
        if symbol.no_dead_strip {
            never_strip.push(local);
        }
        if symbol.visibility != SymbolVisibility::Local {
            owned.push((symbol.id, local));
        }
    }

    // Sections that must be kept whole: a reference that does not land on a
    // symbol would follow the bytes it meant only by accident once the atoms
    // moved. References *from* metadata do not count — those are resolved by
    // the code that understands the format, and are remapped rather than
    // copied.
    let mut opaque = Vec::new();
    let mut opaque_via = Vec::new();
    for relocation in &object.parsed.relocations {
        let from_metadata = object
            .parsed
            .section(relocation.section)
            .is_some_and(|s| is_metadata(&s.name) || s.kind == SectionKind::Debug);
        if from_metadata {
            continue;
        }
        match relocation.target {
            RelocationTarget::Section(section) => opaque.push(section.0),
            RelocationTarget::Symbol(id) => {
                if !stores_addend(relocation) || crate::inline_addend(object, relocation) == 0 {
                    continue;
                }
                let Some(symbol) = object.parsed.symbol(id) else {
                    continue;
                };
                // The addend is measured from the symbol, so the section
                // holding that symbol is the one at risk — here, and wherever
                // else the name is defined.
                if let Some((section, _)) = offset_of(object, symbol) {
                    opaque.push(section.0);
                }
                opaque_via.push(id);
            }
        }
    }
    opaque.sort_unstable();
    opaque.dedup();
    opaque_via.sort_unstable_by_key(|id| id.0);
    opaque_via.dedup();

    let mut eh_frame = Vec::new();
    let mut unwind = Vec::new();
    for section in &object.parsed.sections {
        match section.name.as_str() {
            "__eh_frame" => {
                let (Some(range), Some(file_offset)) =
                    (by_section.get(&section.id.0).cloned(), section.file_offset)
                else {
                    continue;
                };
                for local in range {
                    let atom = &atoms[local as usize];
                    let at = (file_offset + atom.offset) as usize;
                    let id = object
                        .data
                        .get(at + 4..at + 8)
                        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                        .unwrap_or(0);
                    if id == 0 {
                        eh_frame.push(EhRecord {
                            atom: local,
                            cie: true,
                            function: None,
                        });
                        continue;
                    }
                    // `PC begin` follows the CIE pointer, and carries the
                    // relocation naming the function. Both halves of the
                    // `SUBTRACTOR` pair share that offset; the one that is not
                    // the subtractor names the function.
                    let function = within(&grouped, atom)
                        .iter()
                        .find(|r| {
                            r.offset == atom.offset + 8 && r.kind != Arm64RelocationKind::Subtractor
                        })
                        .and_then(|r| edge(r));
                    eh_frame.push(EhRecord {
                        atom: local,
                        cie: false,
                        function,
                    });
                }
            }
            "__compact_unwind" => {
                let Some(list) = grouped.get(&section.id.0) else {
                    continue;
                };
                let mut functions: HashMap<u64, Option<Edge>> = HashMap::default();
                let mut lsdas: Vec<(u64, Edge)> = Vec::new();
                for relocation in list {
                    let record = relocation.offset / COMPACT_UNWIND_RECORD;
                    match relocation.offset % COMPACT_UNWIND_RECORD {
                        CU_FUNCTION => {
                            // Unlike everything else in the link this is a
                            // *section* relocation with the function's address
                            // stored inline, so the offset has to be recovered
                            // before the atom can be found.
                            let function = match relocation.target {
                                RelocationTarget::Section(id) => object
                                    .parsed
                                    .section(id)
                                    .and_then(|s| {
                                        let inline =
                                            crate::inline_addend(object, relocation) as u64;
                                        containing(id, inline.saturating_sub(s.vm_address))
                                    })
                                    .map(Edge::Local),
                                RelocationTarget::Symbol(_) => edge(relocation),
                            };
                            functions.insert(record, function);
                        }
                        CU_LSDA => {
                            if let Some(target) = edge(relocation) {
                                lsdas.push((record, target));
                            }
                        }
                        _ => {}
                    }
                }
                for (record, lsda) in lsdas {
                    unwind.push((functions.get(&record).copied().flatten(), lsda));
                }
            }
            _ => {}
        }
    }

    // Deduplicate the names the edges refer to, and rewrite the edges to index
    // that list. Built here rather than while the edges are: the edge closure
    // is called from five places and threading mutable state through all of
    // them buys nothing over one pass at the end.
    let mut names: Vec<SymbolId> = Vec::new();
    let mut index_of: HashMap<u32, u32> = HashMap::default();
    let mut intern = |edge: &mut Edge| {
        if let Edge::Name(symbol) = edge {
            let next = names.len() as u32;
            let at = *index_of.entry(*symbol).or_insert_with(|| {
                names.push(SymbolId(*symbol));
                next
            });
            *edge = Edge::Name(at);
        }
    };
    for edge in edges.iter_mut().chain(unsplit.iter_mut()) {
        intern(edge);
    }
    for record in &mut eh_frame {
        if let Some(edge) = record.function.as_mut() {
            intern(edge);
        }
    }
    for (function, lsda) in &mut unwind {
        if let Some(edge) = function.as_mut() {
            intern(edge);
        }
        intern(lsda);
    }

    let mut block = ObjectAtoms {
        atoms,
        by_section,
        spans,
        edges,
        suppress,
        unsplit,
        never_strip,
        eh_frame,
        unwind,
        opaque,
        opaque_via,
        owned,
        names,
        digest: 0,
    };
    block.digest = block.compute_digest();
    block
}

impl ObjectAtoms {
    /// Hash every field that the traversal reads.
    fn compute_digest(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = blinker_hashing::FastHasher::default();
        self.atoms.len().hash(&mut hasher);
        for atom in &self.atoms {
            atom.section.0.hash(&mut hasher);
            atom.offset.hash(&mut hasher);
            atom.size.hash(&mut hasher);
        }
        // `by_section` is derived from `atoms` and the section ids above, so
        // it is not hashed separately.
        self.spans.hash(&mut hasher);
        hash_edges(&self.edges, &mut hasher);
        self.suppress.hash(&mut hasher);
        hash_edges(&self.unsplit, &mut hasher);
        self.never_strip.hash(&mut hasher);
        for record in &self.eh_frame {
            record.atom.hash(&mut hasher);
            record.cie.hash(&mut hasher);
            hash_edge(record.function, &mut hasher);
        }
        for (function, lsda) in &self.unwind {
            hash_edge(*function, &mut hasher);
            hash_edge(Some(*lsda), &mut hasher);
        }
        self.opaque.hash(&mut hasher);
        for symbol in &self.opaque_via {
            symbol.0.hash(&mut hasher);
        }
        for (symbol, local) in &self.owned {
            symbol.0.hash(&mut hasher);
            local.hash(&mut hasher);
        }
        // The edges above hash as indices into this, so the list itself has to
        // be hashed for the digest to mean anything about which names they are.
        for symbol in &self.names {
            symbol.0.hash(&mut hasher);
        }
        hasher.finish()
    }
}

fn hash_edges(edges: &[Edge], hasher: &mut impl std::hash::Hasher) {
    for edge in edges {
        hash_edge(Some(*edge), hasher);
    }
}

fn hash_edge(edge: Option<Edge>, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    match edge {
        None => 0u8.hash(hasher),
        Some(Edge::Local(local)) => {
            1u8.hash(hasher);
            local.hash(hasher);
        }
        Some(Edge::Name(at)) => {
            2u8.hash(hasher);
            at.hash(hasher);
        }
    }
}

/// A name nothing in the link defines, in `Atoms::resolved`.
///
/// A sentinel rather than an `Option`, because the array is streamed by the
/// traversal and four bytes per entry is what makes that cheap. Atom indices
/// are bounded by the atom count, which is six figures.
const UNRESOLVED: u32 = u32::MAX;

/// Where each object's atoms sit in the flat numbering, and how much room it
/// has to grow.
///
/// # Why atoms are not simply numbered in order
///
/// They were, and it made every incremental answer above them worthless.
/// A running total means an object that gains one atom shifts every index
/// after it, so the retained live set, the support counts and every edge in
/// the reachability graph stop describing the program — and rebasing them
/// costs more than recomputing from scratch (finding 194). One added function
/// is the ordinary edit, not the rare one.
///
/// So an object keeps its base for as long as its atoms fit in the room it was
/// given, and only an object that outgrows its range moves — to the end, where
/// it disturbs nobody. Objects that did not change do not move at all, which is
/// the property the whole delta rests on.
///
/// This is the same trick the layout uses for addresses under
/// `with_stable_layout`, and it is the same trick `__LINKEDIT` will need for
/// symbol slots and string offsets. Stable identity is not a property of one
/// stage; it is what makes any of them incremental.
#[derive(Clone, Default)]
pub(crate) struct Numbering {
    /// Per object, where its atoms start.
    base: Vec<u32>,
    /// Per object, how many indices it owns from there.
    capacity: Vec<u32>,
    /// One past the highest index any object owns.
    total: u32,
}

/// How much room an object of `atoms` atoms is given.
///
/// An eighth, and at least four. The slack is what an edit spends: adding a
/// function to a small object has to fit, and an eighth of a large one is a lot
/// of functions. Too little and every edit renumbers; too much and the live
/// set, the support counts and the graph's row table all carry the padding.
fn room_for(atoms: usize) -> u32 {
    atoms as u32 + (atoms as u32 / 8).max(4)
}

/// Past this ratio of indices to atoms, the numbering is laid out again.
///
/// Objects that outgrow their range go to the end and leave their old range
/// behind, so a session that relinks one target a hundred times would
/// otherwise number atoms into the millions. Renumbering costs the next link
/// its delta, which is the same price as any other link that cannot align.
const SPREAD: u32 = 2;

impl Numbering {
    fn held_bytes(&self) -> usize {
        (self.base.len() + self.capacity.len()) * std::mem::size_of::<u32>()
    }

    /// Lay out `counts` atoms per object, keeping `previous` wherever it fits.
    fn assign(previous: Option<&Numbering>, counts: &[usize]) -> Numbering {
        let occupied: u32 = counts.iter().map(|count| *count as u32).sum();
        let held = previous.filter(|held| {
            // A different input list is a different program as far as this is
            // concerned: object ids are positions in it.
            held.base.len() == counts.len() && held.total <= SPREAD * occupied.max(1)
        });
        let Some(held) = held else {
            return Numbering::fresh(counts);
        };

        let mut numbering = Numbering {
            base: Vec::with_capacity(counts.len()),
            capacity: Vec::with_capacity(counts.len()),
            total: held.total,
        };
        for (slot, count) in counts.iter().enumerate() {
            if *count as u32 <= held.capacity[slot] {
                numbering.base.push(held.base[slot]);
                numbering.capacity.push(held.capacity[slot]);
                continue;
            }
            // Outgrew its range. Appended rather than shuffled: moving it up
            // would move everything after it, which is the thing this exists
            // to avoid.
            let room = room_for(*count);
            numbering.base.push(numbering.total);
            numbering.capacity.push(room);
            numbering.total += room;
        }
        numbering
    }

    fn fresh(counts: &[usize]) -> Numbering {
        let mut numbering = Numbering {
            base: Vec::with_capacity(counts.len()),
            capacity: Vec::with_capacity(counts.len()),
            total: 0,
        };
        for count in counts {
            let room = room_for(*count);
            numbering.base.push(numbering.total);
            numbering.capacity.push(room);
            numbering.total += room;
        }
        numbering
    }
}

/// Every atom of a link, as the objects' own numbering plus a base each.
pub(crate) struct Atoms<'a> {
    objects: &'a [LoadedObject],
    blocks: Vec<Arc<ObjectAtoms>>,
    /// Where each block's atoms start in the flat numbering.
    base: Vec<usize>,
    /// Per flat index, the block it belongs to. One `u32` per atom, filled in
    /// a linear pass, so the traversal never binary-searches `base`.
    slot_of: Vec<u32>,
    /// Object id -> block index.
    slot: HashMap<u32, usize>,
    /// Where each block's distinct referenced names resolve to, end to end.
    ///
    /// One flat array and a base per block, not a `Vec` per block. The
    /// traversal reads this once per edge — 1.2 million times — and a nested
    /// `Vec` makes that two dependent loads: the inner vector's pointer, then
    /// the entry. It also stored an `Option<usize>`, sixteen bytes to carry a
    /// value that fits in four, so the array the traversal streams was four
    /// times larger than the answers in it.
    resolved: Vec<u32>,
    /// Where each block's names start in `resolved`.
    resolved_base: Vec<u32>,
    total: usize,
    /// The layout the flat indices above came from, so the next link can keep
    /// it. See [`Numbering`].
    numbering: Numbering,
    /// Sections kept whole because a reference into their middle would not
    /// survive their atoms being moved.
    opaque: HashSet<(u32, u32)>,
    /// Atoms defining each externally visible name.
    ///
    /// A weak symbol may have several definitions, and all of them are kept:
    /// which one wins is decided elsewhere, and guessing here would strip the
    /// one that does.
    ///
    /// Keyed by interned id rather than by the name's text. The map holds every
    /// non-local definition in the link and is probed once per distinct
    /// referenced name — hundreds of thousands of each — and the session has
    /// already given every one of those names a number. Hashing the text again
    /// here was paying for an answer that was worked out when the object was
    /// first read.
    owners: HashMap<SymbolNameId, Owners>,
}

impl<'a> Atoms<'a> {
    /// Split three ways because the three are incremental to different degrees:
    /// a block is a pure function of one object and is usually reused whole,
    /// owners need every object's definitions at once, and opacity resolves
    /// names across the link.
    pub(crate) fn build(
        objects: &'a [LoadedObject],
        parts: &mut [f64; 3],
        session: &mut crate::session::Session,
        previous: Option<&Numbering>,
    ) -> Atoms<'a> {
        let step = std::time::Instant::now();
        let mut blocks: Vec<Arc<ObjectAtoms>> = Vec::with_capacity(objects.len());
        let mut base = Vec::with_capacity(objects.len());
        let mut slot = HashMap::default();
        let mut slot_of: Vec<u32> = Vec::new();
        // Each object's names as ids, in `SymbolId` order. Held only for the
        // length of this function: everything the traversal needs is resolved
        // to a flat atom index before it returns.
        let mut interned: Vec<Arc<Vec<SymbolNameId>>> = Vec::with_capacity(objects.len());

        // Which projections are already held, asked before any is computed.
        // A projection is a pure function of one object, so the ones that are
        // missing can all be built at once — which on a cold link is every one
        // of them, and on an edit is the handful that moved.
        let held: Vec<Option<Arc<ObjectAtoms>>> = objects
            .iter()
            .map(|object| session.held_atoms(&object.parsed))
            .collect();
        if held.iter().any(Option::is_none) {
            let computed = crate::parallel::map_chunks(objects, |base, chunk| {
                chunk
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| held[base + at].is_none())
                    .map(|(at, object)| (base + at, Arc::new(project(object))))
                    .collect::<Vec<_>>()
            });
            for (at, projection) in computed.into_iter().flatten() {
                session.store_atoms(&objects[at].parsed, projection);
            }
        }

        for object in objects {
            let block = session.atoms(&object.parsed, || project(object));
            interned.push(session.interned(&object.parsed));
            slot.insert(object.parsed.id.0, blocks.len());
            blocks.push(block);
        }

        // Laid out, not counted up: an object keeps the indices it had unless
        // it outgrew them. See [`Numbering`].
        let numbering = Numbering::assign(
            previous,
            &blocks
                .iter()
                .map(|block| block.atoms.len())
                .collect::<Vec<_>>(),
        );
        let total = numbering.total as usize;
        base.extend(numbering.base.iter().map(|at| *at as usize));
        // The gaps belong to no object. Nothing reads them — `Atoms::indices`
        // is what walks real atoms — and the filler is the neighbour on the
        // left, which is the only slot that could be blamed for a stray read.
        slot_of.resize(total, 0);
        for (index, block) in blocks.iter().enumerate() {
            let at = base[index];
            slot_of[at..at + block.atoms.len()].fill(index as u32);
        }

        parts[0] = step.elapsed().as_secs_f64() * 1000.0;
        let step = std::time::Instant::now();

        // Sized up front. The map ends up holding every non-local definition
        // in the link — 77,000 of them on the debug workload — and growing
        // into that from empty is seventeen rehashes of an ever-larger table.
        let mut owners: HashMap<SymbolNameId, Owners> = HashMap::with_capacity_and_hasher(
            blocks.iter().map(|b| b.owned.len()).sum(),
            Default::default(),
        );
        for index in 0..objects.len() {
            let ids = &interned[index];
            for (symbol, local) in &blocks[index].owned {
                let Some(name) = ids.get(symbol.0 as usize).copied() else {
                    continue;
                };
                let atom = base[index] + *local as usize;
                match owners.entry(name) {
                    std::collections::hash_map::Entry::Occupied(mut held) => {
                        held.get_mut().push(atom)
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(Owners {
                            first: atom,
                            rest: Vec::new(),
                        });
                    }
                }
            }
        }

        parts[1] = step.elapsed().as_secs_f64() * 1000.0;
        let step = std::time::Instant::now();

        let mut result = Atoms {
            objects,
            blocks,
            base,
            slot_of,
            slot,
            total,
            numbering,
            opaque: HashSet::default(),
            owners,
            resolved: Vec::new(),
            resolved_base: Vec::new(),
        };
        (result.resolved, result.resolved_base) = result.resolve_names(&interned);
        result.opaque = result.find_opaque(&interned);
        parts[2] = step.elapsed().as_secs_f64() * 1000.0;
        result
    }

    /// Where each block's distinct referenced names resolve to, and where each
    /// block's run starts.
    ///
    /// One owners probe per (object, name) rather than per edge — 390,606
    /// instead of 1,195,652 on a debug rust-analyzer link.
    ///
    /// On every core: a probe reads the owners map and the block, and writes
    /// nothing. The runs are concatenated in block order, so the bases are the
    /// running lengths and neither depends on which thread finished first.
    fn resolve_names(&self, interned: &[Arc<Vec<SymbolNameId>>]) -> (Vec<u32>, Vec<u32>) {
        let (owners, blocks) = (&self.owners, &self.blocks);
        let chunks = crate::parallel::map_chunks(blocks, |start, chunk| {
            chunk
                .iter()
                .enumerate()
                .map(|(at, block)| {
                    let ids = &interned[start + at];
                    block
                        .names
                        .iter()
                        .map(|symbol| {
                            ids.get(symbol.0 as usize)
                                .and_then(|name| owners.get(name))
                                .map_or(UNRESOLVED, |owners| owners.first as u32)
                        })
                        .collect::<Vec<u32>>()
                })
                .collect::<Vec<_>>()
        });
        let mut resolved = Vec::new();
        let mut bases = Vec::with_capacity(blocks.len());
        for names in chunks.into_iter().flatten() {
            bases.push(resolved.len() as u32);
            resolved.extend(names);
        }
        (resolved, bases)
    }

    /// Sections that must be kept whole.
    fn find_opaque(&self, interned: &[Arc<Vec<SymbolNameId>>]) -> HashSet<(u32, u32)> {
        let mut opaque = HashSet::default();
        for (index, object) in self.objects.iter().enumerate() {
            let block = &self.blocks[index];
            let ids = &interned[index];
            for section in &block.opaque {
                opaque.insert((object.parsed.id.0, *section));
            }
            for symbol in &block.opaque_via {
                let Some(name) = ids.get(symbol.0 as usize) else {
                    continue;
                };
                for target in self.owners.get(name).into_iter().flat_map(Owners::all) {
                    opaque.insert(self.atom(target).key());
                }
            }
        }
        opaque
    }

    /// The size of the flat numbering, which is not the number of atoms.
    ///
    /// Everything indexed by atom — the live set, the support counts, the
    /// graph's rows — is sized by this, and there are gaps in it. See
    /// [`Numbering`].
    fn len(&self) -> usize {
        self.total
    }

    /// Every atom of the link, as its flat index.
    ///
    /// Not `0..len()`. The numbering leaves each object room to grow, so an
    /// index between two objects belongs to no atom and `atom()` on it reads
    /// whatever is next door. Four scans used the range and are now the only
    /// reason this exists.
    fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.blocks
            .iter()
            .enumerate()
            .flat_map(move |(slot, block)| {
                let at = self.base[slot];
                (0..block.atoms.len()).map(move |local| at + local)
            })
    }

    /// The atom at a flat index.
    fn atom(&self, index: usize) -> &Atom {
        let slot = self.slot_of[index] as usize;
        &self.blocks[slot].atoms[index - self.base[slot]]
    }

    /// That section's atoms, as flat indices.
    fn section_range(&self, object: ObjectId, section: SectionId) -> Option<Range<usize>> {
        let slot = *self.slot.get(&object.0)?;
        let range = self.blocks[slot].by_section.get(&section.0)?;
        let base = self.base[slot];
        Some(base + range.start as usize..base + range.end as usize)
    }

    /// Where an edge out of block `slot` lands, in the flat numbering.
    fn resolve(&self, slot: usize, edge: Edge) -> Option<usize> {
        match edge {
            Edge::Local(local) => Some(self.base[slot] + local as usize),
            // Answered by subscript. Every distinct name was resolved once,
            // below; doing it here meant a string hash per edge walked, and
            // the traversal walks 1.2 million of them.
            Edge::Name(at) => {
                match self.resolved[self.resolved_base[slot] as usize + at as usize] {
                    UNRESOLVED => None,
                    index => Some(index as usize),
                }
            }
        }
    }

    /// Every atom defining any of `names`, for use as roots.
    ///
    /// A name nothing in the link ever mentioned has no id and contributes
    /// nothing, which is the same case as a name no object defines: no roots,
    /// and the missing definition is reported by resolution rather than here.
    ///
    /// Plural because the root set is plural. An executable enters at one
    /// symbol; a dylib is entered at every symbol it exports, and stripping a
    /// dylib from a single root deletes live code from it.
    fn defining<'n>(&'n self, names: &'n [SymbolNameId]) -> impl Iterator<Item = usize> + 'n {
        names
            .iter()
            .filter_map(|name| self.owners.get(name))
            .flat_map(Owners::all)
    }
}

/// The relocations of one object, grouped by section and sorted by offset.
type ByOffset<'a> = HashMap<u32, Vec<&'a InputRelocation>>;

fn group_relocations(object: &LoadedObject) -> ByOffset<'_> {
    let mut grouped: ByOffset<'_> = HashMap::default();
    for relocation in &object.parsed.relocations {
        grouped
            .entry(relocation.section.0)
            .or_default()
            .push(relocation);
    }
    for list in grouped.values_mut() {
        list.sort_unstable_by_key(|r| r.offset);
    }
    grouped
}

/// The relocations whose patched field lies inside `atom`.
fn within<'g, 'a>(grouped: &'g ByOffset<'a>, atom: &Atom) -> &'g [&'a InputRelocation] {
    let Some(list) = grouped.get(&atom.section.0) else {
        return &[];
    };
    let start = list.partition_point(|r| r.offset < atom.offset);
    let end = list.partition_point(|r| r.offset < atom.end());
    &list[start..end]
}

/// The set of live atoms, as a bit per atom.
///
/// Atoms are numbered `0..n` and every one of them is asked about — several
/// times, once per edge that points at it. A `HashSet<usize>` hashes an integer
/// that is already a perfect index, and scatters the answer across memory; the
/// traversal was 2.22 ms of a 16.8 ms link and almost all of it was this.
///
/// The API is deliberately the same three operations the `HashSet` offered, so
/// the traversal below reads unchanged.
#[derive(Debug, Clone, Default)]
pub(crate) struct LiveSet {
    bits: Vec<u64>,
}

impl LiveSet {
    fn held_bytes(&self) -> usize {
        self.bits.len() * std::mem::size_of::<u64>()
    }

    fn with_capacity(atoms: usize) -> LiveSet {
        LiveSet {
            bits: vec![0; atoms.div_ceil(64)],
        }
    }

    /// Mark `index` live, returning whether it was not already.
    fn insert(&mut self, index: usize) -> bool {
        let (word, bit) = (index / 64, 1u64 << (index % 64));
        let slot = &mut self.bits[word];
        let new = *slot & bit == 0;
        *slot |= bit;
        new
    }

    /// Mark `index` dead. Only the region recompute does this: the phased
    /// traversal only ever adds, which is why the type had no need of it.
    fn remove(&mut self, index: usize) {
        self.bits[index / 64] &= !(1u64 << (index % 64));
    }

    pub(crate) fn contains(&self, index: usize) -> bool {
        self.bits
            .get(index / 64)
            .is_some_and(|word| word & (1u64 << (index % 64)) != 0)
    }
}

/// The atoms defining one name.
///
/// The first inline, the rest in a `Vec` that stays empty — and an empty `Vec`
/// does not allocate. Nearly every name has exactly one definition; a weak
/// symbol may have several and all of them are kept, because which one wins is
/// decided elsewhere. Storing a `Vec` for every name meant a heap allocation
/// per externally visible symbol, around seven thousand of them, to hold one
/// `usize` each.
#[derive(Debug)]
pub(crate) struct Owners {
    first: usize,
    rest: Vec<usize>,
}

impl Owners {
    fn push(&mut self, index: usize) {
        self.rest.push(index);
    }

    fn all(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.first).chain(self.rest.iter().copied())
    }
}

/// The reachability graph, as a graph.
///
/// [`liveness`] below computes the same answer as a sequence of phases — roots,
/// propagate, a metadata pass, propagate again, then a revival loop over atoms
/// whose edges were deliberately not followed — and each phase reads the
/// objects' projections directly. That is fine for computing an answer once and
/// impossible to update, because there is no edge set to add to or remove from.
///
/// This is the same relation written down. The claim it rests on is that the
/// live set is exactly the closure of the roots under these edges — see
/// [`Graph::agrees_with`] for why that is a claim and not a definition.
///
/// # Why it is retained rather than derived
///
/// Building it costs 30.2 ms against the 17.2 ms traversal it replaces
/// (finding 193). Deriving the edge set each link and updating it would be a
/// straight loss, so the graph lives in [`ReachState`] across links and only
/// the changed objects' rows are rebuilt.
///
/// # Two edge sets, not one
///
/// An atom's own edges come from its object's relocations, so an object owns
/// every row it can change and its rows can be patched in isolation.
///
/// Metadata edges cannot: they run *backwards* — a function keeps its FDE
/// alive rather than the FDE keeping its function alive, which is what makes
/// the phased version's suppression unnecessary — and the function an object's
/// FDE describes may live in another object. Rebuilding one object's rows
/// would drop metadata edges another object put there. They are 11% of the
/// edges (303,918 of 2,791,216), so they are kept per producing object and
/// re-indexed whole, which costs a pass over a ninth of the graph rather than
/// an index that would have to be patched from both ends.
#[derive(Default)]
pub(crate) struct Graph {
    /// Own edges, as rows in an arena: `arena[start[a] .. start[a] + len[a]]`.
    ///
    /// An arena and not a prefix-sum table, because a row that grows has to go
    /// somewhere. A patched row is appended and the old space left as a hole.
    start: Vec<u32>,
    len: Vec<u32>,
    arena: Vec<u32>,
    /// Entries of `arena` that some row still points at. The rest are holes.
    used: usize,

    /// Metadata edges as `(source, target)`, grouped by the object that
    /// produced them, so a changed object's contribution can be replaced.
    produced: Vec<Vec<(u32, u32)>>,
    /// The same edges indexed by source: `meta[base[a] .. base[a + 1]]`.
    meta: Vec<u32>,
    meta_base: Vec<u32>,

    /// Roots as a set, for the delta, and as a list, for a full closure.
    root: LiveSet,
    roots: Vec<u32>,
}

/// A live set and the support that justifies each bit.
///
/// `support[a]` counts a's live predecessors. Settled, `live[a]` holds exactly
/// when `a` is a root or `support[a] > 0` — the forward direction because a
/// live predecessor propagates, the backward one because an atom only became
/// live as a root or through one.
///
/// The counts are what make removal possible. They are not sufficient on their
/// own: a cycle no root reaches supports itself, and would keep itself alive
/// forever under pure reference counting. [`Graph::update`] handles that by
/// recomputing a bounded region rather than by trusting the count.
#[derive(Clone, Default)]
pub(crate) struct Live {
    set: LiveSet,
    support: Vec<u32>,
}

impl Live {
    fn held_bytes(&self) -> usize {
        self.set.held_bytes() + self.support.len() * std::mem::size_of::<u32>()
    }
}

impl From<LiveSet> for Live {
    /// A live set with no support behind it, for the path that computed one
    /// without a graph. Nothing may update it — `Graph::update` needs counts —
    /// and nothing does: the same condition that produced it stops the delta.
    fn from(set: LiveSet) -> Live {
        Live {
            set,
            support: Vec::new(),
        }
    }
}

impl Graph {
    fn held_bytes(&self) -> usize {
        let words = |count: usize| count * std::mem::size_of::<u32>();
        words(self.start.len())
            + words(self.len.len())
            + words(self.arena.len())
            + words(self.meta.len())
            + words(self.meta_base.len())
            + words(self.roots.len())
            + self.root.held_bytes()
            + self
                .produced
                .iter()
                .map(|edges| {
                    std::mem::size_of::<Vec<(u32, u32)>>()
                        + edges.len() * std::mem::size_of::<(u32, u32)>()
                })
                .sum::<usize>()
    }

    /// The edges every atom of one object contributes, as `(source, target)`.
    ///
    /// Own edges and metadata edges are collected together because both are
    /// read off the same projection, and separated by the caller: `source` is
    /// inside the object for the first kind and anywhere for the second.
    fn object_edges(
        atoms: &Atoms<'_>,
        slot: usize,
        own: &mut Vec<Vec<u32>>,
        produced: &mut Vec<(u32, u32)>,
    ) {
        let block = &atoms.blocks[slot];
        let at = atoms.base[slot];
        own.clear();
        own.resize(block.atoms.len(), Vec::new());
        for (local, row) in own.iter_mut().enumerate() {
            let (start, end) = block.spans[local];
            row.clear();
            for edge in &block.edges[start as usize..end as usize] {
                if let Some(target) = atoms.resolve(slot, *edge) {
                    row.push(target as u32);
                }
            }
        }
        produced.clear();
        for record in &block.eh_frame {
            if let Some(function) = record.function.and_then(|edge| atoms.resolve(slot, edge)) {
                produced.push((function as u32, (at + record.atom as usize) as u32));
            }
        }
        for (function, lsda) in &block.unwind {
            if let (Some(function), Some(table)) = (
                function.and_then(|edge| atoms.resolve(slot, edge)),
                atoms.resolve(slot, *lsda),
            ) {
                produced.push((function as u32, table as u32));
            }
        }
    }

    fn build(atoms: &Atoms<'_>, entry: &[SymbolNameId]) -> Graph {
        let total = atoms.len();
        let mut graph = Graph {
            start: vec![0; total],
            len: vec![0; total],
            arena: Vec::new(),
            used: 0,
            produced: vec![Vec::new(); atoms.blocks.len()],
            meta: Vec::new(),
            meta_base: vec![0; total + 1],
            root: LiveSet::with_capacity(total),
            roots: Vec::new(),
        };

        // On every core, and correct to do so: an object's own edges are a
        // function of its own projection and the resolution table, both read
        // only. The rows concatenate in object order, so the arena is the
        // chunks end to end and neither it nor the lengths depend on which
        // thread finished first.
        let built = crate::parallel::map_chunks(&atoms.blocks, |base, chunk| {
            let (mut own, mut produced) = (Vec::new(), Vec::new());
            let mut flat: Vec<u32> = Vec::new();
            let mut lengths: Vec<u32> = Vec::new();
            let mut metadata: Vec<Vec<(u32, u32)>> = Vec::with_capacity(chunk.len());
            for slot in base..base + chunk.len() {
                Graph::object_edges(atoms, slot, &mut own, &mut produced);
                for row in &own {
                    lengths.push(row.len() as u32);
                    flat.extend_from_slice(row);
                }
                metadata.push(std::mem::take(&mut produced));
            }
            (flat, lengths, metadata)
        });
        let mut slot = 0usize;
        for (flat, lengths, metadata) in built {
            let chunk = graph.arena.len() as u32;
            let (mut within, mut taken) = (0u32, 0usize);
            for edges in metadata {
                // Rows are addressed by atom index, and atom indices are not
                // consecutive across objects — the numbering leaves each one
                // room to grow. Walking them with a counter is how this was
                // wrong: it worked exactly until the layout stopped being a
                // running total, and then produced a graph missing most of its
                // edges and a link short of most of its symbols.
                let at = atoms.base[slot];
                for local in 0..atoms.blocks[slot].atoms.len() {
                    let length = lengths[taken + local];
                    graph.start[at + local] = chunk + within;
                    graph.len[at + local] = length;
                    within += length;
                }
                taken += atoms.blocks[slot].atoms.len();
                graph.produced[slot] = edges;
                slot += 1;
            }
            graph.arena.extend_from_slice(&flat);
        }
        graph.used = graph.arena.len();
        graph.index_metadata();
        graph.set_roots(roots(atoms, entry));
        graph
    }

    /// Rebuild the by-source index over every object's metadata edges.
    fn index_metadata(&mut self) {
        let total = self.start.len();
        self.meta_base.clear();
        self.meta_base.resize(total + 1, 0);
        for edges in &self.produced {
            for (source, _) in edges {
                self.meta_base[*source as usize + 1] += 1;
            }
        }
        for index in 1..self.meta_base.len() {
            self.meta_base[index] += self.meta_base[index - 1];
        }
        self.meta = vec![0; self.meta_base[total] as usize];
        let mut cursor = self.meta_base.clone();
        for edges in &self.produced {
            for (source, target) in edges {
                self.meta[cursor[*source as usize] as usize] = *target;
                cursor[*source as usize] += 1;
            }
        }
    }

    fn set_roots(&mut self, roots: Vec<u32>) {
        self.root = LiveSet::with_capacity(self.start.len());
        for root in &roots {
            self.root.insert(*root as usize);
        }
        self.roots = roots;
    }

    /// Where atom `a`'s edges are, as its own row and its metadata row.
    fn edges(&self, a: usize) -> (&[u32], &[u32]) {
        let own = self.start[a] as usize..self.start[a] as usize + self.len[a] as usize;
        let meta = self.meta_base[a] as usize..self.meta_base[a + 1] as usize;
        (&self.arena[own], &self.meta[meta])
    }

    /// How many indices object `slot` owns, so the delta can find the tail an
    /// object left behind when it shrank.
    fn capacity_of(&self, atoms: &Atoms<'_>, slot: usize) -> usize {
        atoms.numbering.capacity[slot] as usize
    }

    /// Replace atom `a`'s own row, leaving the old space as a hole.
    fn set_row(&mut self, a: usize, row: &[u32]) {
        self.used -= self.len[a] as usize;
        self.start[a] = self.arena.len() as u32;
        self.len[a] = row.len() as u32;
        self.arena.extend_from_slice(row);
        self.used += row.len();
    }

    /// Move every row back into a dense arena.
    ///
    /// Patching leaves holes, and a session that relinks a target a hundred
    /// times would otherwise grow the arena without bound. Called when the
    /// holes outnumber the rows.
    fn compact(&mut self) {
        let mut arena = Vec::with_capacity(self.used);
        for a in 0..self.start.len() {
            let row = self.start[a] as usize..self.start[a] as usize + self.len[a] as usize;
            self.start[a] = arena.len() as u32;
            arena.extend_from_slice(&self.arena[row]);
        }
        self.arena = arena;
        self.used = self.arena.len();
    }

    /// Everything reachable from the roots, and the support behind it.
    fn closure(&self) -> Live {
        let total = self.start.len();
        let mut live = Live {
            set: LiveSet::with_capacity(total),
            support: vec![0; total],
        };
        let mut worklist: Vec<u32> = Vec::new();
        for root in &self.roots {
            if live.set.insert(*root as usize) {
                worklist.push(*root);
            }
        }
        self.propagate(&mut live, &mut worklist);
        live
    }

    /// Mark everything the worklist reaches, counting support as it goes.
    fn propagate(&self, live: &mut Live, worklist: &mut Vec<u32>) {
        while let Some(index) = worklist.pop() {
            let (own, meta) = self.edges(index as usize);
            for target in own.iter().chain(meta) {
                live.support[*target as usize] += 1;
                if live.set.insert(*target as usize) {
                    worklist.push(*target);
                }
            }
        }
    }

    /// Whether the closure is the live set the phased traversal produced.
    ///
    /// The two are written to agree and are not obviously the same thing. The
    /// phased version suppresses metadata's own edges during propagation and
    /// then revives their targets in a later loop, and it runs the metadata
    /// pass once rather than to a fixed point — so a function that only became
    /// live during revival would keep its FDE in this closure and not there.
    /// `revived` counts exactly the cases where that could differ, and it is
    /// zero on every workload measured; this is what says so on the ones that
    /// have not been.
    fn agrees_with(&self, live: &LiveSet) -> bool {
        self.closure().set.bits == live.bits
    }

    /// Fold one link's changes into this graph and its live set.
    ///
    /// `moved` lists the objects whose edges may differ. Returns `None` when
    /// the region that has to be recomputed grows past a bound, so the caller
    /// falls back to a full closure rather than doing its work badly.
    ///
    /// # Why support counts are not enough
    ///
    /// Adding an edge is the easy direction: it can only make atoms live, and
    /// propagating forward from the new edge is exactly right.
    ///
    /// Removing one is not. `support[a]` counts a's live predecessors, and
    /// reaching zero is necessary for a to die but not sufficient the other
    /// way round — a cycle no root reaches supports itself, and pure reference
    /// counting would keep that dead loop alive for the life of the session.
    ///
    /// So the counts are used only to find *suspects*: live atoms that are not
    /// roots and have no live predecessor left. That set cannot be wrong in the
    /// direction that matters — an atom that should die always ends up in it —
    /// and the answer for the suspects is recomputed rather than deduced.
    ///
    /// The region recomputed is everything forward-reachable from the
    /// suspects. It is closed under successors by construction: if `a` is in it
    /// and `a -> t` then `t` is reachable from a suspect too. That is what
    /// makes the recompute local — no edge leaves the region, so cutting it out
    /// cannot disturb the support of anything outside. Clear it, and every atom
    /// in it that is a root or still has support from outside seeds a
    /// propagation that stays inside.
    fn update(
        &mut self,
        atoms: &Atoms<'_>,
        moved: &[usize],
        live: &mut Live,
        entry: &[SymbolNameId],
    ) -> Option<()> {
        let total = self.start.len();

        // Phase one: withdraw the edges of every object that moved, from the
        // support of everything they were holding up.
        let (mut own, mut produced) = (Vec::new(), Vec::new());
        let mut rows: Vec<(usize, Vec<u32>)> = Vec::new();
        let mut metadata_moved = false;
        // Every atom that lost an edge from a live atom. Support reaching zero
        // is *not* enough to find these: a group of atoms that point at each
        // other keeps its own counts up, so when the edge that reached the
        // group from the roots goes away, every member still has a supporter
        // and none of them ever looks suspicious. That is not a corner case —
        // it is a call graph with a loop in it, which is most of them, and it
        // is what made this leak 246 atoms while every count stayed positive.
        let mut withdrawn: Vec<u32> = Vec::new();
        for slot in moved.iter().copied() {
            Graph::object_edges(atoms, slot, &mut own, &mut produced);
            let at = atoms.base[slot];
            for (local, row) in own.iter().enumerate() {
                let index = at + local;
                if self.edges(index).0 == row.as_slice() {
                    continue;
                }
                if live.set.contains(index) {
                    for target in self.edges(index).0.to_vec() {
                        live.support[target as usize] -= 1;
                        withdrawn.push(target);
                    }
                }
                rows.push((index, row.clone()));
            }
            // An object that shrank leaves live bits and rows behind inside
            // its own range — the indices are still its own, but the atoms are
            // gone. Retired here, because nothing else walks them: every scan
            // over real atoms stops at the object's current count.
            for index in at + own.len()..at + self.capacity_of(atoms, slot) {
                if self.len[index] == 0 && !live.set.contains(index) {
                    continue;
                }
                if live.set.contains(index) {
                    for target in self.edges(index).0.to_vec() {
                        live.support[target as usize] -= 1;
                        withdrawn.push(target);
                    }
                    live.set.remove(index);
                }
                self.set_row(index, &[]);
            }
            if self.produced[slot] != produced {
                for (source, target) in &self.produced[slot] {
                    if live.set.contains(*source as usize) {
                        live.support[*target as usize] -= 1;
                        withdrawn.push(*target);
                    }
                }
                self.produced[slot] = std::mem::take(&mut produced);
                metadata_moved = true;
            }
        }

        // Phase two: install them, and count what the new ones hold up.
        for (index, row) in &rows {
            self.set_row(*index, row);
        }
        if metadata_moved {
            self.index_metadata();
        }
        // Roots are recomputed rather than tracked: a root can appear from an
        // opacity decision made in another object, so there is no object whose
        // change would announce it. This is the scan the phased traversal did
        // anyway.
        self.set_roots(roots(atoms, entry));

        let mut seeds: Vec<u32> = Vec::new();
        for (index, _) in &rows {
            if live.set.contains(*index) {
                for target in self.edges(*index).0.to_vec() {
                    live.support[target as usize] += 1;
                    if !live.set.contains(target as usize) {
                        seeds.push(target);
                    }
                }
            }
        }
        if metadata_moved {
            for slot in moved.iter().copied() {
                for (source, target) in self.produced[slot].clone() {
                    if live.set.contains(source as usize) {
                        live.support[target as usize] += 1;
                        if !live.set.contains(target as usize) {
                            seeds.push(target);
                        }
                    }
                }
            }
        }

        // One pass says both what a new root owes its life to and what has run
        // out of support. Targeted versions of these were tried first and were
        // wrong in both directions: a root can be gained by an atom no changed
        // object mentions, and an atom can lose its last supporter through a
        // metadata edge that came from somewhere else entirely.
        let mut suspects: Vec<u32> = Vec::new();
        for index in 0..total {
            let (alive, root) = (live.set.contains(index), self.root.contains(index));
            if root && !alive {
                seeds.push(index as u32);
            } else if alive && !root && live.support[index] == 0 {
                suspects.push(index as u32);
            }
        }

        let mut worklist: Vec<u32> = Vec::new();
        for seed in seeds {
            if live.set.insert(seed as usize) {
                worklist.push(seed);
            }
        }
        self.propagate(live, &mut worklist);
        // Propagation only adds support, so a suspect it rescued is no longer
        // one. Checked here rather than filtered above, because a suspect can
        // also be rescued by another suspect's rescue.
        // Anything that lost an edge is suspect, whatever its count says. The
        // region recompute below is what decides; the count only decides which
        // atoms are *obviously* gone.
        suspects.extend(
            withdrawn
                .into_iter()
                .filter(|index| live.set.contains(*index as usize)),
        );
        suspects.retain(|index| !self.root.contains(*index as usize));
        if suspects.is_empty() {
            return Some(());
        }

        // Phase three: the region that might have died, recomputed exactly.
        let mut region = LiveSet::with_capacity(total);
        let mut members: Vec<u32> = Vec::new();
        let mut frontier: Vec<u32> = Vec::new();
        for suspect in suspects {
            if live.set.contains(suspect as usize) && region.insert(suspect as usize) {
                members.push(suspect);
                frontier.push(suspect);
            }
        }
        // Bounded, because past some size the region is the program and
        // walking it twice — once to find it, once to rebuild it — is worse
        // than starting over. Starting over is cheap here in a way it was not
        // before: the graph is already updated, so it is a closure and not a
        // build, and a closure of the whole graph still beats the phased
        // traversal it replaced.
        //
        // This bound is reached on an ordinary edit, not a rare one. The
        // objects a body edit touches are usually near the roots of the call
        // graph, and everything they reach is most of what is live.
        // A sixty-fourth, and not a quarter as it was first written. The
        // region is almost always the whole live set on this workload, so the
        // bound's job is to notice that quickly rather than to permit a large
        // recompute: walking a quarter of the program before giving up cost
        // more than the closure it gave up in favour of.
        let limit = total / 64;
        while let Some(index) = frontier.pop() {
            if members.len() > limit {
                *live = self.closure();
                return Some(());
            }
            let (own, meta) = self.edges(index as usize);
            for target in own.iter().chain(meta) {
                if live.set.contains(*target as usize) && region.insert(*target as usize) {
                    members.push(*target);
                    frontier.push(*target);
                }
            }
        }

        // Cut it out. Every edge out of the region lands inside it, so this
        // cannot disturb the support of anything that stays.
        for index in members.iter().copied() {
            live.set.remove(index as usize);
        }
        for index in members.iter().copied() {
            let (own, meta) = self.edges(index as usize);
            for target in own.iter().chain(meta) {
                live.support[*target as usize] -= 1;
            }
        }
        // Then put back whatever the rest of the program still holds up.
        for index in members.iter().copied() {
            if (self.root.contains(index as usize) || live.support[index as usize] > 0)
                && live.set.insert(index as usize)
            {
                worklist.push(index);
            }
        }
        self.propagate(live, &mut worklist);
        // Patched rows leave the old space behind, and a session that relinks
        // one target two hundred times would otherwise grow an arena of holes.
        if self.arena.len() > 2 * self.used {
            self.compact();
        }
        Some(())
    }
}

/// Atoms that are live before anything points at them.
fn roots(atoms: &Atoms<'_>, entry: &[SymbolNameId]) -> Vec<u32> {
    let mut roots: Vec<u32> = atoms.defining(entry).map(|at| at as u32).collect();
    for index in atoms.indices() {
        if atoms.opaque.contains(&atoms.atom(index).key()) {
            roots.push(index as u32);
        }
    }
    for (slot, block) in atoms.blocks.iter().enumerate() {
        let at = atoms.base[slot];
        for local in &block.never_strip {
            roots.push((at + *local as usize) as u32);
        }
        for edge in &block.unsplit {
            if let Some(index) = atoms.resolve(slot, *edge) {
                roots.push(index as u32);
            }
        }
        // A CIE is live because an FDE that refers to it is, and the FDE's own
        // edges are metadata — so nothing would ever reach it.
        for record in &block.eh_frame {
            if record.cie {
                roots.push((at + record.atom as usize) as u32);
            }
        }
    }
    roots
}

/// Which atoms a program can reach from its roots.
fn liveness(atoms: &Atoms<'_>, entry: &[SymbolNameId], parts: &mut [f64; 2]) -> (LiveSet, usize) {
    let step = std::time::Instant::now();
    let mut live = LiveSet::with_capacity(atoms.len());
    let mut worklist: Vec<usize> = Vec::new();
    let mark = |index: usize, live: &mut LiveSet, worklist: &mut Vec<usize>| {
        if live.insert(index) {
            worklist.push(index);
        }
    };

    // Roots. The entry point, anything the object marked as never strippable,
    // every atom of a section that has to be kept whole, and everything the
    // sections this link does not split refer to.
    for index in atoms.defining(entry).collect::<Vec<_>>() {
        mark(index, &mut live, &mut worklist);
    }
    for index in atoms.indices() {
        if atoms.opaque.contains(&atoms.atom(index).key()) {
            mark(index, &mut live, &mut worklist);
        }
    }
    for (slot, block) in atoms.blocks.iter().enumerate() {
        for local in &block.never_strip {
            mark(atoms.base[slot] + *local as usize, &mut live, &mut worklist);
        }
        for edge in &block.unsplit {
            if let Some(index) = atoms.resolve(slot, *edge) {
                mark(index, &mut live, &mut worklist);
            }
        }
    }
    parts[0] = step.elapsed().as_secs_f64() * 1000.0;
    let step = std::time::Instant::now();

    // Atoms whose own edges were deliberately not followed. Every other live
    // atom passes through the worklist exactly once and has its targets marked,
    // so these are the only places the invariant can be violated — and the only
    // places the verification below has to look.
    let mut suppressed: Vec<usize> = Vec::new();
    let propagate = |live: &mut LiveSet, worklist: &mut Vec<usize>, suppressed: &mut Vec<usize>| {
        while let Some(index) = worklist.pop() {
            let slot = atoms.slot_of[index] as usize;
            let block = &atoms.blocks[slot];
            let local = index - atoms.base[slot];
            // Metadata's own references do not keep anything alive; it is kept
            // alive *by* what it describes, below.
            if block.suppress[local] {
                suppressed.push(index);
                continue;
            }
            let (start, end) = block.spans[local];
            for edge in &block.edges[start as usize..end as usize] {
                if let Some(target) = atoms.resolve(slot, *edge) {
                    if live.insert(target) {
                        worklist.push(target);
                    }
                }
            }
        }
    };
    propagate(&mut live, &mut worklist, &mut suppressed);

    // Metadata comes alive with its subject, not before it. An `__eh_frame`
    // FDE is live when the function it describes is, and a `__compact_unwind`
    // record's exception table is live when its function is — the record
    // itself never reaches the output, but the `__gcc_except_tab` atom it
    // points at does, and nothing else refers to that atom.
    for (slot, block) in atoms.blocks.iter().enumerate() {
        let base = atoms.base[slot];
        for record in &block.eh_frame {
            let alive = record.cie
                || record
                    .function
                    .and_then(|edge| atoms.resolve(slot, edge))
                    .is_some_and(|f| live.contains(f));
            if alive {
                mark(base + record.atom as usize, &mut live, &mut worklist);
            }
        }
        for (function, lsda) in &block.unwind {
            let alive = function
                .and_then(|edge| atoms.resolve(slot, edge))
                .is_some_and(|f| live.contains(f));
            if alive {
                if let Some(index) = atoms.resolve(slot, *lsda) {
                    mark(index, &mut live, &mut worklist);
                }
            }
        }
    }
    propagate(&mut live, &mut worklist, &mut suppressed);

    // The invariant, verified: everything a live atom refers to must be live.
    //
    // Only the suppressed atoms can break it. Propagation marks the targets of
    // every atom it visits, and every live atom is visited exactly once — so
    // checking the whole live set is a second walk over work already done. On
    // a 47-object fixture that cost 0.9 ms and was left alone; on blinker's own
    // binary it is a fifth of the dead-strip stage (77).
    let mut revived = 0usize;
    loop {
        let mut found = Vec::new();
        for index in suppressed.iter().copied() {
            let slot = atoms.slot_of[index] as usize;
            let block = &atoms.blocks[slot];
            let (start, end) = block.spans[index - atoms.base[slot]];
            for edge in &block.edges[start as usize..end as usize] {
                if let Some(target) = atoms.resolve(slot, *edge) {
                    if !live.contains(target) {
                        found.push(target);
                    }
                }
            }
        }
        if found.is_empty() {
            break;
        }
        revived += found.len();
        // Revived atoms go through the worklist, so their own edges are
        // followed and the invariant extends to them too.
        for index in found {
            mark(index, &mut live, &mut worklist);
        }
        propagate(&mut live, &mut worklist, &mut suppressed);
    }

    parts[1] = step.elapsed().as_secs_f64() * 1000.0;
    (live, revived)
}

/// One run of surviving bytes, and where it moved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Piece {
    pub from: u64,
    pub size: u64,
    pub to: u64,
}

/// A section that lost bytes, and the map from old offsets to new ones.
#[derive(Debug, Clone, Default, PartialEq)]
struct Compacted {
    /// Ascending by `from`, non-overlapping.
    pieces: Vec<Piece>,
    size: u64,
}

impl Strip {
    fn held_bytes(&self) -> usize {
        self.sections
            .values()
            .map(|compacted| {
                std::mem::size_of::<((u32, u32), Compacted)>()
                    + compacted.pieces.len() * std::mem::size_of::<Piece>()
            })
            .sum()
    }
}

/// Where every surviving input byte moved to.
///
/// An absent section is one that lost nothing, and maps to itself. That is the
/// common case even in a link that strips heavily, and it keeps the lookup on
/// the relocation path to one failed hash probe.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Strip {
    sections: crate::hashing::FastMap<(u32, u32), Compacted>,
}

/// Where an atom originally at `offset` may be put, at or after `cursor`.
///
/// # Why congruence and not alignment
///
/// This used to compute the *atom's* own alignment — the largest power of two
/// dividing its offset, capped at the section's — and align the cursor to that.
/// It is wrong, and it produced programs that could not be linked at all.
///
/// An atom is not one symbol. `__const` contributions routinely hold several
/// constants, and an atom beginning at offset 0x74 of a 16-byte-aligned section
/// can contain a symbol at 0x80 that clang has already decided is 16-byte
/// aligned — it emits `LDR Q` against it, whose 12-bit immediate is *scaled by
/// 16* and simply cannot encode an address that is not. Aligning that atom to
/// 4, which is all its own offset justifies, moved the symbol inside it to a
/// place where no encoding exists. The link then failed on the relocation:
///
/// ```text
///   cannot apply PageOff12 against l_anon.….48: value 2942 is not 16-byte aligned
/// ```
///
/// What every symbol in an atom actually depends on is its offset *modulo the
/// section's alignment*, because that is the only thing the assembler could
/// rely on when it chose the instruction. Preserving the whole congruence
/// preserves every symbol inside the atom at once, without needing to know
/// where any of them are: an atom that was `k` past a 16-byte boundary is put
/// back `k` past one.
///
/// Where the offset is a multiple of the section's alignment — the common case,
/// and every case a single-symbol atom has — this is exactly the old
/// behaviour. It costs at most `alignment - 1` bytes per atom, and only for
/// atoms that were not aligned to begin with.
fn placement_of(cursor: u64, offset: u64, section: u64) -> u64 {
    let modulus = section.max(1);
    let wanted = offset % modulus;
    let here = cursor % modulus;
    // The least `to >= cursor` with `to % modulus == wanted`.
    match here <= wanted {
        true => cursor + (wanted - here),
        false => cursor + modulus - here + wanted,
    }
}

impl Strip {
    /// Strip nothing: every byte stays where it was.
    pub(crate) fn none() -> Strip {
        Strip::default()
    }

    /// Whether this section lost anything.
    fn compacted(&self, object: ObjectId, section: SectionId) -> Option<&Compacted> {
        self.sections.get(&(object.0, section.0))
    }

    /// The size a section contributes to the output.
    pub(crate) fn size_of(&self, object: ObjectId, section: SectionId, original: u64) -> u64 {
        self.compacted(object, section)
            .map(|c| c.size)
            .unwrap_or(original)
    }

    /// Where `offset` bytes into a section ended up, or `None` if those bytes
    /// were stripped.
    pub(crate) fn remap(&self, object: ObjectId, section: SectionId, offset: u64) -> Option<u64> {
        let Some(compacted) = self.compacted(object, section) else {
            return Some(offset);
        };
        let index = compacted
            .pieces
            .partition_point(|p| p.from <= offset)
            .checked_sub(1)?;
        let piece = compacted.pieces[index];
        (offset < piece.from + piece.size).then(|| piece.to + (offset - piece.from))
    }

    /// The surviving runs of a section, for copying its bytes.
    pub(crate) fn pieces(&self, object: ObjectId, section: SectionId) -> Option<&[Piece]> {
        self.compacted(object, section).map(|c| c.pieces.as_slice())
    }

    /// How a relative field's stored addend has to change.
    ///
    /// A `SUBTRACTOR` pair writes `minuend - subtrahend + addend`. Where the
    /// subtrahend is a symbol in the field's own section — an `__eh_frame`
    /// record's `ltmpN` anchor, always — the addend encodes the distance from
    /// that anchor to the field, so that the result comes out relative to the
    /// field itself. Compaction moves both, and the stored distance is then
    /// the one they used to be apart.
    pub(crate) fn pair_correction(
        &self,
        object: ObjectId,
        section: SectionId,
        field: u64,
        anchor: u64,
    ) -> i64 {
        if self.compacted(object, section).is_none() {
            return 0;
        }
        let (Some(field_now), Some(anchor_now)) = (
            self.remap(object, section, field),
            self.remap(object, section, anchor),
        ) else {
            return 0;
        };
        (field as i64 - anchor as i64) - (field_now as i64 - anchor_now as i64)
    }

    /// Build the map from a liveness result.
    fn build(objects: &[LoadedObject], atoms: &Atoms<'_>, live: &LiveSet) -> Strip {
        let mut sections = crate::hashing::FastMap::default();
        for object in objects {
            for section in &object.parsed.sections {
                let key = (object.parsed.id.0, section.id.0);
                let Some(range) = atoms.section_range(object.parsed.id, section.id) else {
                    continue;
                };
                if range.clone().all(|index| live.contains(index)) {
                    continue;
                }
                let mut pieces: Vec<Piece> = Vec::new();
                let mut cursor = 0u64;
                for index in range {
                    if !live.contains(index) {
                        continue;
                    }
                    let atom = atoms.atom(index);
                    // Adjacent survivors keep their relative positions, which
                    // costs nothing and keeps any alignment they had.
                    if let Some(last) = pieces.last_mut() {
                        if last.from + last.size == atom.offset {
                            last.size += atom.size;
                            cursor = last.to + last.size;
                            continue;
                        }
                    }
                    let to = placement_of(cursor, atom.offset, section.alignment);
                    pieces.push(Piece {
                        from: atom.offset,
                        size: atom.size,
                        to,
                    });
                    cursor = to + atom.size;
                }
                sections.insert(
                    key,
                    Compacted {
                        pieces,
                        size: cursor,
                    },
                );
            }
        }
        Strip { sections }
    }
}

/// How the three halves of dead-stripping divide its cost.
///
/// They are not equally incremental and the difference decides what is worth
/// building. `Atoms::build` reads one object at a time and reads nothing else,
/// so a session that holds the object can hold its atoms; `liveness` is a
/// traversal of the whole graph and cannot be split that way. Measuring them
/// separately is how that choice gets made on evidence instead of on which one
/// sounds heavier.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StripTimings {
    pub atoms_ms: f64,
    pub liveness_ms: f64,
    pub build_ms: f64,
    /// Inside `liveness_ms`: collecting roots, then traversing from them.
    pub group_ms: f64,
    pub traverse_ms: f64,
    /// Computing every object's reachability digest, and what it found.
    pub digest_ms: f64,
    pub reach_moved: u64,
    pub reach_total: u64,
    /// Whether the previous link's answer was reused whole.
    pub reused_strip: bool,
    /// Whether the live set was updated rather than recomputed, and over how
    /// many objects' edges.
    pub delta_used: bool,
    pub delta_objects: u64,
    /// Atoms the propagation left dead that a live atom then referred to.
    pub revived: u64,
}

/// What one target's reachability was, last time this session linked it.
///
/// # Why this exists
///
/// Everything reachability retained before this was a *shortcut*: a memo of one
/// object's projection, and an all-or-nothing copy of the previous `Strip` under
/// a key that was the entire projection vector. One object out of 5,637 moving
/// discarded the second one, so the common case — a body edit — rebuilt the
/// whole graph and traversed it whole. Retention was reuse of an answer, and
/// answers are all-or-nothing by nature.
///
/// This is the other thing: the target's reachability *state*, which a link
/// updates rather than recomputes. It is deliberately introduced with no
/// updating in it — every field here is still filled by the same full traversal
/// that filled the local variables before, and this commit makes nothing
/// faster. What it changes is who owns the graph. A delta update needs the
/// previous live set to subtract from, the numbering that live set was built
/// in, and which object contributed which part of it; none of those survived a
/// link before, so there was nowhere for an incremental liveness pass to stand.
///
/// # The numbering
///
/// [`LiveSet`] is indexed by flat atom number, which is a running total over
/// the objects *in this link's order*. That numbering is not stable across
/// links: an object gaining an atom shifts every atom after it. So the state
/// records the numbering it was built in — object ids and their bases — and a
/// reader has to check it before believing a bit. [`ReachState::aligned_with`]
/// is that check, and it is what a delta pass will use to decide between
/// remapping and starting over.
pub(crate) struct ReachState {
    /// Object ids in link order, and each one's projection digest.
    ///
    /// Parallel arrays rather than a map: the whole-vector comparison is the
    /// hot use, and it is a `memcmp` on the digests.
    objects: Vec<u32>,
    projections: Vec<u64>,
    /// Where each object's atoms begin in the flat numbering `live` uses.
    bases: Vec<u32>,
    /// Atoms in that numbering.
    total: u32,
    /// Where each object's distinct referenced names resolved to, so a link
    /// can tell an object whose *targets* moved from one whose bytes did.
    ///
    /// A digest cannot answer that. It hashes the object's own structure —
    /// atoms, local edges, the symbol *indices* it refers to — and not what
    /// those indices resolve to, which is decided by every other object in the
    /// link. rustc renames every codegen unit of a recompiled crate, and a
    /// rename that is consistent between a definition and its references
    /// leaves both the digests and these values untouched, which is exactly
    /// why the pair is worth keeping and the digest alone is not.
    resolved: Vec<u32>,
    resolved_base: Vec<u32>,
    /// The layout the flat indices are in. Handed to the next link's
    /// `Atoms::build` so unchanged objects keep their atom indices.
    numbering: Numbering,
    /// The graph, and the live set with the support behind it.
    graph: Graph,
    live: Live,
    /// What that live set was compacted into.
    strip: Arc<Strip>,
}

impl ReachState {
    /// Roughly what holding this costs a resident linker.
    ///
    /// The graph's edge arena dominates — 1.5 million local edges and 1.2
    /// million by name on a debug rust-analyzer link — followed by the strip,
    /// which is a piece list per section that lost bytes.
    pub(crate) fn held_bytes(&self) -> usize {
        let words = |count: usize| count * std::mem::size_of::<u32>();
        words(self.objects.len())
            + self.projections.len() * std::mem::size_of::<u64>()
            + words(self.bases.len())
            + words(self.resolved.len())
            + words(self.resolved_base.len())
            + self.numbering.held_bytes()
            + self.graph.held_bytes()
            + self.live.held_bytes()
            + self.strip.held_bytes()
    }

    /// Whether `digests` is exactly what produced this state.
    ///
    /// The identity check the old `Session::strip` performed, minus the
    /// question of *which* objects: two links whose object lists differ cannot
    /// have equal-length digest vectors unless the counts match, and this is
    /// also asked about object ids, which are positions in the input list.
    fn matches(&self, objects: &[u32], digests: &[u64]) -> bool {
        self.objects == objects && self.projections == digests
    }

    /// Whether this state's atom numbering is still the current one.
    ///
    /// Weaker than [`ReachState::matches`] and the condition a delta update
    /// needs: the objects and their atom counts are the same, so a flat atom
    /// index means the same atom, even though some object's *edges* moved.
    #[allow(dead_code, reason = "the delta pass this was retained for")]
    fn aligned_with(&self, objects: &[u32], bases: &[u32], total: u32) -> bool {
        self.objects == objects && self.bases == bases && self.total == total
    }

    /// How many objects' projections differ from the ones recorded here.
    ///
    /// Positional, because the digests are recorded in link order and this is
    /// only reached when the object lists are equal.
    fn moved_against(&self, objects: &[u32], digests: &[u64]) -> u64 {
        if self.objects != objects {
            return digests.len() as u64;
        }
        self.projections
            .iter()
            .zip(digests)
            .filter(|(held, now)| held != now)
            .count() as u64
    }

    /// Which objects' edges may differ from the ones in this state.
    ///
    /// Two questions, because an object's edges move for two reasons. Its own
    /// projection moving is the obvious one. The other is that its targets
    /// moved without it: an edge is stored as "the name at index 7", and where
    /// index 7 lands is a fact about the whole link.
    fn moved(&self, digests: &[u64], resolved: &[u32], base: &[u32]) -> Vec<usize> {
        (0..digests.len())
            .filter(|slot| {
                let slot = *slot;
                if self.projections[slot] != digests[slot] {
                    return true;
                }
                let now = base[slot] as usize..base[slot + 1] as usize;
                let held = self.resolved_base[slot] as usize..self.resolved_base[slot + 1] as usize;
                held.len() != now.len() || self.resolved[held] != resolved[now]
            })
            .collect()
    }
}

/// Decide what a link keeps.
pub(crate) fn plan(
    objects: &[LoadedObject],
    entry: &[String],
    session: &mut crate::session::Session,
) -> (Strip, Report, StripTimings) {
    let mut timings = StripTimings::default();

    // Taken before the atoms are built, because the layout they are given is
    // the one the previous link used: an object that has not outgrown its
    // range keeps its indices, and that is what makes anything retained here
    // still mean something. See [`Numbering`].
    let previous = session.reach_state();
    let step = std::time::Instant::now();
    let mut parts = [0.0f64; 3];
    let atoms = Atoms::build(
        objects,
        &mut parts,
        session,
        previous.as_ref().map(|state| &state.numbering),
    );
    // Interned after the build, which is what puts every input's names in the
    // table. A root symbol nothing mentions has no id, and no atoms.
    let entry: Vec<SymbolNameId> = entry
        .iter()
        .filter_map(|name| session.names().get(name))
        .collect();
    timings.atoms_ms = step.elapsed().as_secs_f64() * 1000.0;

    // How much of the graph moved, taken from the projections themselves.
    //
    // This used to be a second digest, computed before the projections existed
    // and hashing what it believed reachability reads. Two digests of the same
    // thing is one digest and one claim about it, and the claim was already
    // wrong (154). The projection is built by now and carries its own.
    let digest_step = std::time::Instant::now();
    let projections: Vec<u64> = atoms.blocks.iter().map(|block| block.digest).collect();
    let identities: Vec<u32> = objects.iter().map(|object| object.parsed.id.0).collect();
    // Both tables carry a terminator, so an object's run is `[i]..[i + 1]` with
    // no special case for the last one.
    let mut bases: Vec<u32> = atoms.base.iter().map(|at| *at as u32).collect();
    bases.push(atoms.len() as u32);
    let mut resolved_base = atoms.resolved_base.clone();
    resolved_base.push(atoms.resolved.len() as u32);

    let moved = match previous.as_ref() {
        Some(state) => state.moved_against(&identities, &projections),
        // No previous state for this target is not "nothing moved". It is the
        // cold case, and reporting zero would make a first link indistinguishable
        // from a perfectly reused one in every stat this feeds.
        None => projections.len() as u64,
    };
    session.note_reachability(moved, projections.len() as u64);
    timings.digest_ms = digest_step.elapsed().as_secs_f64() * 1000.0;
    timings.reach_moved = moved;
    timings.reach_total = projections.len() as u64;

    // Every object contributes what it contributed last time, so the owners
    // map, the opaque set, the live set and the compaction are all the same as
    // last time. Verified rather than asserted under `--blinker-verify`, which
    // recomputes and compares: stripping an atom that is still reachable
    // produces a binary that links, runs, and crashes somewhere else later, so
    // this is not a place to trust an argument.
    let reusable = previous
        .as_ref()
        .is_some_and(|state| state.matches(&identities, &projections));
    // "The held answer was valid", not "the shortcut was taken". Under
    // verification the work is done anyway and the two answers compared, and a
    // flag that flipped with the verification mode would mean every test of it
    // measured the mode rather than the linker — which is how this was found.
    timings.reused_strip = reusable;
    if reusable && !verify_liveness() {
        let state = previous.expect("reusable implies there is one");
        let report = report_from(objects, &atoms, &state.strip);
        let strip = Strip::clone(&state.strip);
        // Stored again, unchanged. Reading it is not what keeps it — see
        // `Recent::take` — and a target whose answer is always right would
        // otherwise be the one eviction drops.
        session.store_reach(state);
        return (strip, report, timings);
    }

    // The delta. Everything above is bookkeeping; this is the thing the state
    // exists for. `update` returns `None` when the region it would have to
    // recompute is large enough that a full closure is the better answer, so a
    // fallback here is a decision rather than a failure.
    let step = std::time::Instant::now();
    let mut live_parts = [0.0f64; 2];
    let updated = previous.filter(|_| delta_liveness()).and_then(|mut state| {
        if !state.aligned_with(&identities, &bases, atoms.len() as u32) {
            return None;
        }
        let moved = state.moved(&projections, &atoms.resolved, &resolved_base);
        // Moved out rather than borrowed: `update` rewrites both in place, and
        // the state they came from is on its way to being replaced.
        let mut graph = std::mem::take(&mut state.graph);
        let mut live = std::mem::take(&mut state.live);
        graph
            .update(&atoms, &moved, &mut live, &entry)
            .map(|()| (graph, live, moved.len() as u64))
    });
    timings.delta_used = updated.is_some();
    timings.delta_objects = updated.as_ref().map_or(0, |(_, _, moved)| *moved);

    // The full path is still the phased traversal, and the graph is only built
    // where something will update it. Finding 194 is why: a retained graph that
    // cannot be updated is 7 ms a link of pure cost, and today it cannot be —
    // an edit that changes one object's atom count shifts every atom index
    // after it, and `aligned_with` refuses. Under `BLINKER_DELTA_LIVENESS` the
    // whole apparatus runs, so it stays exercised and measurable while the
    // numbering it needs is built.
    let (graph, live) = match updated {
        Some((graph, live, _)) => (graph, live),
        None if delta_liveness() => {
            let graph = Graph::build(&atoms, &entry);
            let live = graph.closure();
            (graph, live)
        }
        None => {
            let (set, revived) = liveness(&atoms, &entry, &mut live_parts);
            timings.revived = revived as u64;
            (Graph::default(), Live::from(set))
        }
    };
    timings.liveness_ms = step.elapsed().as_secs_f64() * 1000.0;
    timings.group_ms = live_parts[0];
    timings.traverse_ms = live_parts[1];

    // The phased traversal, kept as the thing the graph is checked against.
    // It reads the projections directly and shares no code with the closure,
    // which is what makes the comparison worth anything.
    // The delta checks itself, always, and fails hard when it is wrong.
    //
    // It is wrong today: on the large workload it disagrees with a fresh
    // closure by 32 words of 15,296 (finding 195), which is why the flag that
    // reaches it is off. An experimental path that produces a *plausible*
    // binary is worse than one that stops, and this project has now been
    // caught twice by a wrong answer that looked like a right one.
    // The delta's own check, and the one that matters: the updated live set
    // must be the closure of the graph it was updated against. It is a
    // different question from `agrees_with` below, which asks whether the graph
    // describes the same program as the phased traversal — and it is the one
    // that caught a 246-atom leak that every byte comparison had passed.
    if verify_liveness() && timings.delta_used {
        assert!(
            Graph::build(&atoms, &entry).closure().set.bits == live.set.bits,
            "the updated live set is not the closure of the graph it came from"
        );
    }
    if verify_liveness() && delta_liveness() {
        let (expected, revived) = liveness(&atoms, &entry, &mut live_parts);
        timings.revived = revived as u64;
        assert!(
            Graph::build(&atoms, &entry).agrees_with(&expected),
            "the reachability graph and the traversal disagree about what is live"
        );
        assert!(
            live.set.bits == expected.bits,
            "the delta and a full traversal disagree about what is live"
        );
    }
    let revived = timings.revived as usize;

    let step = std::time::Instant::now();
    let strip = Strip::build(objects, &atoms, &live.set);
    timings.build_ms = step.elapsed().as_secs_f64() * 1000.0;

    let report = report(objects, &atoms, &live.set, revived);
    session.store_reach(ReachState {
        objects: identities,
        projections,
        bases,
        total: atoms.len() as u32,
        numbering: atoms.numbering.clone(),
        // Only where something will read them. With the delta off these are
        // 1.5 MB of copying a link to answer a question nobody asks.
        resolved: if delta_liveness() {
            atoms.resolved.clone()
        } else {
            Vec::new()
        },
        resolved_base: if delta_liveness() {
            resolved_base
        } else {
            Vec::new()
        },
        graph,
        live,
        strip: Arc::new(strip.clone()),
    });

    (strip, report, timings)
}

/// Whether to retain the reachability graph and update it across links.
///
/// Off by default, and finding 194 says why: the delta is implemented, agrees
/// with a full traversal bit for bit, and cannot fire on a real edit. Atom
/// indices are a dense running total over the link's objects, so one object
/// gaining one atom shifts every index after it and the retained live set
/// stops meaning anything. Rebasing a dense numbering costs more than the
/// traversal it would save.
///
/// `BLINKER_DELTA_LIVENESS=1` turns it on, so the machinery stays exercised
/// and measurable while stable atom identity is built underneath it.
fn delta_liveness() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BLINKER_DELTA_LIVENESS").is_some())
}

/// Whether to recompute liveness and compare it against the held answer.
///
/// The reuse above is a claim about what an object's projection determines,
/// and the cost of it being wrong is a binary that links and runs and fails
/// somewhere unrelated. `BLINKER_VERIFY_LIVENESS=1` makes every link do the
/// work twice and assert the two agree.
fn verify_liveness() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BLINKER_VERIFY_LIVENESS").is_some())
}

/// The `__text` numbers, recovered from a reused strip rather than a live set.
///
/// A reused strip carries which bytes survived but not which atoms did, and
/// the report is counted over atoms. `Strip::remap` answers the same question:
/// an atom whose offset still maps somewhere is one that survived.
///
/// `revived` is reported as zero rather than recomputed. It counts atoms the
/// propagation left dead that a live atom then referred to, which is a check
/// on the model and not a property of the answer — and the answer being reused
/// is precisely the case where the check already passed.
fn report_from(objects: &[LoadedObject], atoms: &Atoms<'_>, strip: &Strip) -> Report {
    let mut live = LiveSet::with_capacity(atoms.len());
    for index in atoms.indices() {
        let atom = atoms.atom(index);
        if strip
            .remap(atom.object, atom.section, atom.offset)
            .is_some()
        {
            live.insert(index);
        }
    }
    report(objects, atoms, &live, 0)
}

/// Report what analysis alone can say, without changing any output.
pub(crate) fn analyse(objects: &[LoadedObject], entry: &[String]) -> Report {
    plan(objects, entry, &mut crate::session::Session::default()).1
}

/// The `__text` numbers, which are what can be compared against another linker.
fn report(objects: &[LoadedObject], atoms: &Atoms<'_>, live: &LiveSet, revived: usize) -> Report {
    let is_text: HashSet<(u32, u32)> = objects
        .iter()
        .flat_map(|o| {
            o.parsed
                .sections
                .iter()
                .filter(|s| s.name == "__text")
                .map(move |s| (o.parsed.id.0, s.id.0))
        })
        .collect();

    let mut report = Report {
        revived,
        ..Report::default()
    };
    let mut by_section: HashMap<(u32, u32), (u64, bool)> = HashMap::default();
    for index in atoms.indices() {
        let atom = atoms.atom(index);
        if !is_text.contains(&atom.key()) {
            continue;
        }
        report.total_atoms += 1;
        report.total_bytes += atom.size;
        let alive = live.contains(index);
        if alive {
            report.live_atoms += 1;
            report.live_bytes += atom.size;
        }
        let entry = by_section.entry(atom.key()).or_insert((0, false));
        entry.0 += atom.size;
        entry.1 |= alive;
    }
    report.fully_dead_section_bytes = by_section
        .values()
        .filter(|(_, any_live)| !any_live)
        .map(|(size, _)| *size)
        .sum();
    strip_parts(objects, atoms, live);
    report
}

/// Where the dead-strip's leverage went, per section name.
///
/// `Report` counts `__text` alone, which was the right thing to count when the
/// gap against ld64 was two thirds `__text`. On a large C++-flavoured link it
/// is not: `__TEXT,__const` was 54 MB against ld64's 31, and no number here
/// could say whether that was liveness or atomisation.
///
/// `whole` is the column that answers it — bytes in an input section that
/// produced exactly one atom, which is a section the strip could not cut and
/// therefore keeps entirely if anything in it is reachable.
fn strip_parts(objects: &[LoadedObject], atoms: &Atoms<'_>, live: &LiveSet) {
    if std::env::var_os("BLINKER_STRIP_PARTS").is_none() {
        return;
    }
    let mut name_of: HashMap<(u32, u32), &str> = HashMap::default();
    for object in objects {
        for section in &object.parsed.sections {
            name_of.insert((object.parsed.id.0, section.id.0), section.name.as_str());
        }
    }
    // (total, live, opaque) bytes per section name.
    let mut totals: HashMap<&str, (u64, u64, u64)> = HashMap::default();
    for index in atoms.indices() {
        let atom = atoms.atom(index);
        let name = name_of.get(&atom.key()).copied().unwrap_or("?");
        let row = totals.entry(name).or_insert((0, 0, 0));
        row.0 += atom.size;
        if live.contains(index) {
            row.1 += atom.size;
        }
    }
    // Bytes in sections held opaque — kept whole because something points
    // into them without landing on a symbol. Recomputed here rather than read
    // out of the projection: this is a diagnostic, and the loop is the same
    // one `project` runs.
    let mut opaque_bytes: HashMap<&str, u64> = HashMap::default();
    let mut per_object: HashMap<&std::path::Path, u64> = HashMap::default();
    let (mut section_caused, mut symbol_caused) = (0u64, 0u64);
    let (mut contained, mut escaping, mut elsewhere) = (0u64, 0u64, 0u64);
    let mut span_bytes = 0u64;
    let mut spans: HashMap<(u32, u32), Vec<(u64, u64)>> = HashMap::default();
    // Atom spans per (object, section), ascending, so a symbol's atom can be
    // found by offset.
    let mut atom_index: HashMap<(u32, u32), Vec<(u64, u64)>> = HashMap::default();
    for index in atoms.indices() {
        let atom = atoms.atom(index);
        atom_index
            .entry(atom.key())
            .or_default()
            .push((atom.offset, atom.offset + atom.size));
    }
    for spans in atom_index.values_mut() {
        spans.sort_unstable();
    }
    for object in objects {
        let mut held: HashSet<u32> = HashSet::default();
        let mut via_section: HashSet<u32> = HashSet::default();
        for relocation in &object.parsed.relocations {
            let from_metadata = object
                .parsed
                .section(relocation.section)
                .is_some_and(|s| is_metadata(&s.name) || s.kind == SectionKind::Debug);
            if from_metadata {
                continue;
            }
            match relocation.target {
                RelocationTarget::Section(section) => {
                    held.insert(section.0);
                    via_section.insert(section.0);
                }
                RelocationTarget::Symbol(id) => {
                    let addend = crate::inline_addend(object, relocation);
                    if !stores_addend(relocation) || addend == 0 {
                        continue;
                    }
                    let Some(symbol) = object.parsed.symbol(id) else {
                        continue;
                    };
                    match offset_of(object, symbol) {
                        Some((section, offset)) => {
                            held.insert(section.0);
                            // Does the addend leave the atom the symbol names?
                            // If not, nothing has to be pinned: an atom moves
                            // as a unit and `symbol + addend` moves with it.
                            let key = (object.parsed.id.0, section.0);
                            let mine = atom_index.get(&key).map(Vec::as_slice).unwrap_or(&[]);
                            let found = mine
                                .partition_point(|(start, _)| *start <= offset)
                                .checked_sub(1)
                                .map(|i| mine[i]);
                            match found {
                                Some((start, end)) if offset < end => {
                                    let target = offset as i64 + addend;
                                    if target < start as i64 || target >= end as i64 {
                                        escaping += 1;
                                        // How far it reaches: the bytes between
                                        // the symbol and what it points at are
                                        // all that has to hold still.
                                        let (lo, hi) = if target < offset as i64 {
                                            (target.max(0) as u64, offset)
                                        } else {
                                            (offset, target as u64)
                                        };
                                        span_bytes += hi - lo;
                                        spans.entry(key).or_default().push((lo, hi));
                                    } else {
                                        contained += 1;
                                    }
                                }
                                _ => escaping += 1,
                            }
                        }
                        // Defined in another object; this one cannot say.
                        None => elsewhere += 1,
                    }
                }
            }
        }
        for section in &object.parsed.sections {
            if held.contains(&section.id.0) {
                *opaque_bytes.entry(section.name.as_str()).or_default() += section.size;
                *per_object.entry(object.path.as_ref()).or_default() += section.size;
                if via_section.contains(&section.id.0) {
                    section_caused += section.size;
                } else {
                    symbol_caused += section.size;
                }
            }
        }
    }
    for (name, bytes) in opaque_bytes {
        totals.entry(name).or_insert((0, 0, 0)).2 = bytes;
    }
    // Which inputs bring the opacity, because "some objects do this" is not
    // something a fix can be aimed at.
    let mut by_object: Vec<(u64, &std::path::Path)> = per_object
        .into_iter()
        .filter(|(_, bytes)| *bytes > 0)
        .map(|(path, bytes)| (bytes, path))
        .collect();
    by_object.sort_unstable_by_key(|(bytes, _)| std::cmp::Reverse(*bytes));
    let mut rows: Vec<_> = totals.into_iter().collect();
    rows.sort_unstable_by_key(|(_, (total, alive, _))| std::cmp::Reverse(total - alive));
    #[allow(clippy::print_stderr)]
    {
        eprintln!(
            "  strip: {:<20}{:>12}{:>12}{:>12}",
            "section", "total", "dead", "opaque"
        );
        for (name, (total, alive, whole)) in rows.into_iter().take(12) {
            eprintln!(
                "  strip: {name:<20}{total:>12}{:>12}{whole:>12}",
                total - alive
            );
        }
        eprintln!("  strip: opacity bytes: section target {section_caused}, symbol+addend {symbol_caused}");
        eprintln!("  strip: addends: {contained} inside the symbol's atom, {escaping} escaping it, {elsewhere} to another object");
        // The union of the spans, which is what pinning ranges instead of
        // whole sections would actually hold.
        let mut union = 0u64;
        for ranges in spans.values_mut() {
            ranges.sort_unstable();
            let (mut at, mut covered) = (0u64, 0u64);
            for (lo, hi) in ranges.iter().copied() {
                let start = lo.max(at);
                if hi > start {
                    covered += hi - start;
                    at = hi;
                }
            }
            union += covered;
        }
        eprintln!("  strip: same-object spans reach {span_bytes} bytes, {union} of them distinct");
        for (bytes, path) in by_object.into_iter().take(6) {
            eprintln!("  strip: opaque {bytes:>12}  {}", path.display());
        }
    }
}

#[cfg(test)]
mod placement_tests {
    use super::placement_of;

    /// The invariant the whole function exists for: an atom keeps its offset
    /// modulo the section's alignment, so every symbol inside it keeps the
    /// alignment the assembler assumed when it chose an instruction.
    #[test]
    fn an_atom_keeps_its_congruence() {
        for alignment in [1u64, 2, 4, 8, 16, 32] {
            for offset in 0u64..200 {
                for cursor in 0u64..200 {
                    let to = placement_of(cursor, offset, alignment);
                    assert!(to >= cursor, "an atom may not move backwards");
                    assert_eq!(
                        to % alignment.max(1),
                        offset % alignment.max(1),
                        "congruence lost for offset {offset} at cursor {cursor} \
                         with alignment {alignment}"
                    );
                }
            }
        }
    }

    /// It must not waste more than it has to, or a strip that compacts nothing
    /// would still grow the output.
    #[test]
    fn it_takes_the_least_position_that_works() {
        for alignment in [1u64, 2, 4, 8, 16] {
            for offset in 0u64..64 {
                for cursor in 0u64..64 {
                    let to = placement_of(cursor, offset, alignment);
                    assert!(
                        to - cursor < alignment.max(1),
                        "moved {} for alignment {alignment}",
                        to - cursor
                    );
                }
            }
        }
    }

    /// The case every single-symbol atom has, and the one the old
    /// alignment-based version already got right: an atom starting on a
    /// boundary is placed on a boundary.
    #[test]
    fn an_aligned_atom_is_placed_aligned() {
        assert_eq!(placement_of(0, 0, 16), 0);
        assert_eq!(placement_of(1, 0, 16), 16);
        assert_eq!(placement_of(17, 0x80, 16), 32);
    }

    /// The bug, as a test. An atom at 0x74 of a 16-byte-aligned section holds a
    /// symbol at 0x80; the atom must land 4 past a boundary so that the symbol
    /// lands on one.
    #[test]
    fn a_symbol_inside_an_atom_stays_aligned() {
        let (atom, symbol, alignment) = (0x74u64, 0x80u64, 16u64);
        let to = placement_of(46, atom, alignment);
        let moved = to + (symbol - atom);
        assert_eq!(moved % alignment, 0, "the symbol landed at {moved:#x}");
    }
}
