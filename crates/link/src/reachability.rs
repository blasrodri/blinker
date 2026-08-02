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
    /// Whatever defines this symbol's name, decided globally.
    Name(SymbolId),
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
        Some(Edge::Name(id))
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
        Some(Edge::Name(id)) => {
            2u8.hash(hasher);
            id.0.hash(hasher);
        }
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
    total: usize,
    /// Sections kept whole because a reference into their middle would not
    /// survive their atoms being moved.
    opaque: HashSet<(u32, u32)>,
    /// Atoms defining each externally visible name.
    ///
    /// A weak symbol may have several definitions, and all of them are kept:
    /// which one wins is decided elsewhere, and guessing here would strip the
    /// one that does.
    ///
    /// Borrowed, not cloned. Every non-local defined symbol in the link went
    /// through `name.clone()` here — around seven thousand `String`
    /// allocations to build a map that is discarded at the end of the same
    /// function, and it was 0.72 ms of a 1.23 ms stage. The names live in the
    /// parsed objects, which outlive this.
    owners: HashMap<&'a str, Owners>,
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
    ) -> Atoms<'a> {
        let step = std::time::Instant::now();
        let mut blocks: Vec<Arc<ObjectAtoms>> = Vec::with_capacity(objects.len());
        let mut base = Vec::with_capacity(objects.len());
        let mut slot = HashMap::default();
        let mut total = 0usize;
        let mut slot_of: Vec<u32> = Vec::new();
        for object in objects {
            let block = session.atoms(&object.parsed, || project(object));
            slot.insert(object.parsed.id.0, blocks.len());
            base.push(total);
            slot_of.resize(total + block.atoms.len(), blocks.len() as u32);
            total += block.atoms.len();
            blocks.push(block);
        }

        parts[0] = step.elapsed().as_secs_f64() * 1000.0;
        let step = std::time::Instant::now();

        // Sized up front. The map ends up holding every non-local definition
        // in the link — 77,000 of them on the debug workload — and growing
        // into that from empty is seventeen rehashes of an ever-larger table.
        let mut owners: HashMap<&'a str, Owners> = HashMap::with_capacity_and_hasher(
            blocks.iter().map(|b| b.owned.len()).sum(),
            Default::default(),
        );
        for (index, object) in objects.iter().enumerate() {
            for (symbol, local) in &blocks[index].owned {
                let Some(name) = object.parsed.symbol(*symbol).map(|s| s.name.as_str()) else {
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
            opaque: HashSet::default(),
            owners,
        };
        result.opaque = result.find_opaque();
        parts[2] = step.elapsed().as_secs_f64() * 1000.0;
        result
    }

    /// Sections that must be kept whole.
    fn find_opaque(&self) -> HashSet<(u32, u32)> {
        let mut opaque = HashSet::default();
        for (index, object) in self.objects.iter().enumerate() {
            let block = &self.blocks[index];
            for section in &block.opaque {
                opaque.insert((object.parsed.id.0, *section));
            }
            for symbol in &block.opaque_via {
                let Some(name) = object.parsed.symbol(*symbol).map(|s| s.name.as_str()) else {
                    continue;
                };
                for target in self.owners.get(name).into_iter().flat_map(Owners::all) {
                    opaque.insert(self.atom(target).key());
                }
            }
        }
        opaque
    }

    fn len(&self) -> usize {
        self.total
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
            Edge::Name(id) => {
                let name = self.objects[slot].parsed.symbol(id)?.name.as_str();
                self.owners.get(name).map(|o| o.first)
            }
        }
    }

    /// Every atom defining `name`, for use as a root.
    fn defining(&self, name: &str) -> impl Iterator<Item = usize> + '_ {
        self.owners.get(name).into_iter().flat_map(Owners::all)
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
#[derive(Debug, Clone)]
pub(crate) struct LiveSet {
    bits: Vec<u64>,
}

impl LiveSet {
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

/// Which atoms a program can reach from `entry`.
fn liveness(atoms: &Atoms<'_>, entry: &str, parts: &mut [f64; 2]) -> (LiveSet, usize) {
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
    for index in 0..atoms.len() {
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

/// Where every surviving input byte moved to.
///
/// An absent section is one that lost nothing, and maps to itself. That is the
/// common case even in a link that strips heavily, and it keeps the lookup on
/// the relocation path to one failed hash probe.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Strip {
    sections: crate::hashing::FastMap<(u32, u32), Compacted>,
}

/// The alignment an atom actually had, which compaction must not weaken.
///
/// An atom at offset 8 of a 16-byte-aligned section was 8-byte aligned, and
/// putting it back at a 16-byte boundary would only waste space. An atom at
/// offset 0 takes the section's own alignment.
fn alignment_of(offset: u64, section: u64) -> u64 {
    let section = section.max(1);
    if offset == 0 {
        return section;
    }
    (1u64 << offset.trailing_zeros().min(63)).min(section)
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
                    let to = blinker_layout::align_up(
                        cursor,
                        alignment_of(atom.offset, section.alignment),
                    );
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
}

/// Decide what a link keeps.
pub(crate) fn plan(
    objects: &[LoadedObject],
    entry: &str,
    session: &mut crate::session::Session,
) -> (Strip, Report, StripTimings) {
    let mut timings = StripTimings::default();

    let step = std::time::Instant::now();
    let mut parts = [0.0f64; 3];
    let atoms = Atoms::build(objects, &mut parts, session);
    timings.atoms_ms = step.elapsed().as_secs_f64() * 1000.0;

    // How much of the graph moved, taken from the projections themselves.
    //
    // This used to be a second digest, computed before the projections existed
    // and hashing what it believed reachability reads. Two digests of the same
    // thing is one digest and one claim about it, and the claim was already
    // wrong (154). The projection is built by now and carries its own.
    let digest_step = std::time::Instant::now();
    let projections: Vec<u64> = atoms.blocks.iter().map(|block| block.digest).collect();
    let identified: Vec<(u32, u64)> = objects
        .iter()
        .zip(&projections)
        .map(|(object, digest)| (object.parsed.id.0, *digest))
        .collect();
    let (moved, total) = session.note_reachability(&identified);
    timings.digest_ms = digest_step.elapsed().as_secs_f64() * 1000.0;
    timings.reach_moved = moved;
    timings.reach_total = total;

    // Every object contributes what it contributed last time, so the owners
    // map, the opaque set, the live set and the compaction are all the same as
    // last time. Verified rather than asserted under `--blinker-verify`, which
    // recomputes and compares: stripping an atom that is still reachable
    // produces a binary that links, runs, and crashes somewhere else later, so
    // this is not a place to trust an argument.
    let held = session.strip(&projections);
    if let Some(strip) = held.as_ref() {
        if !verify_liveness() {
            timings.reused_strip = true;
            return (
                Strip::clone(strip),
                report_from(objects, &atoms, strip),
                timings,
            );
        }
    }

    let step = std::time::Instant::now();
    let mut live_parts = [0.0f64; 2];
    let (live, revived) = liveness(&atoms, entry, &mut live_parts);
    timings.liveness_ms = step.elapsed().as_secs_f64() * 1000.0;
    timings.group_ms = live_parts[0];
    timings.traverse_ms = live_parts[1];

    let step = std::time::Instant::now();
    let strip = Strip::build(objects, &atoms, &live);
    timings.build_ms = step.elapsed().as_secs_f64() * 1000.0;

    if let Some(held) = held {
        assert_eq!(
            *held, strip,
            "the held strip differs from a freshly computed one: reachability \
             reused an answer that the graph no longer supports"
        );
    }
    session.store_strip(projections, std::sync::Arc::new(strip.clone()));

    (strip, report(objects, &atoms, &live, revived), timings)
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
    for index in 0..atoms.len() {
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
pub(crate) fn analyse(objects: &[LoadedObject], entry: &str) -> Report {
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
    for index in 0..atoms.len() {
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
    report
}
