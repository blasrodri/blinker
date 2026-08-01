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

use crate::hashing::{FastMap as HashMap, FastSet as HashSet};
use crate::LoadedObject;
use blinker_macho::{
    Arm64RelocationKind, InputRelocation, InputSection, ObjectId, RelocationTarget, SectionId,
    SectionKind, SymbolVisibility,
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

/// Every atom of a link, with the index needed to find one by address.
pub(crate) struct Atoms<'a> {
    atoms: Vec<Atom>,
    /// `(object, section)` -> that section's atoms, as a range of indices into
    /// `atoms`, ascending by offset.
    by_section: crate::hashing::FastMap<(u32, u32), std::ops::Range<usize>>,
    /// Sections kept whole because a reference into their middle would not
    /// survive their atoms being moved.
    opaque: HashSet<(u32, u32)>,
    /// Atoms defining each externally visible name.
    ///
    /// A weak symbol may have several definitions, and all of them are kept:
    /// which one wins is decided elsewhere, and guessing here would strip the
    /// one that does.
    /// Borrowed, not cloned. Every non-local defined symbol in the link went
    /// through `name.clone()` here — around seven thousand `String`
    /// allocations to build a map that is discarded at the end of the same
    /// function, and it was 0.72 ms of a 1.23 ms stage. The names live in the
    /// parsed objects, which outlive this.
    owners: HashMap<&'a str, Vec<usize>>,
}

impl<'a> Atoms<'a> {
    /// Split three ways because the three are incremental to different degrees:
    /// boundaries are a pure function of one object, owners need the global
    /// atom numbering, and opacity is a scan of every relocation in the link.
    pub(crate) fn build(objects: &'a [LoadedObject], parts: &mut [f64; 3]) -> Atoms<'a> {
        let step = std::time::Instant::now();
        let mut atoms: Vec<Atom> = Vec::new();
        let mut by_section = crate::hashing::FastMap::default();

        for object in objects {
            for section in &object.parsed.sections {
                let Some(offsets) = boundaries(object, section) else {
                    continue;
                };
                let start = atoms.len();
                for (index, offset) in offsets.iter().copied().enumerate() {
                    let end = offsets.get(index + 1).copied().unwrap_or(section.size);
                    atoms.push(Atom {
                        object: object.parsed.id,
                        section: section.id,
                        offset,
                        size: end.saturating_sub(offset),
                    });
                }
                by_section.insert((object.parsed.id.0, section.id.0), start..atoms.len());
            }
        }

        parts[0] = step.elapsed().as_secs_f64() * 1000.0;
        let step = std::time::Instant::now();

        let mut result = Atoms {
            atoms,
            by_section,
            opaque: HashSet::default(),
            owners: HashMap::default(),
        };

        for object in objects {
            for symbol in &object.parsed.symbols {
                if !symbol.strength.is_definition() || symbol.visibility == SymbolVisibility::Local
                {
                    continue;
                }
                let Some((section, offset)) = offset_of(object, symbol) else {
                    continue;
                };
                if let Some(index) = result.containing(object.parsed.id, section, offset) {
                    result
                        .owners
                        .entry(symbol.name.as_str())
                        .or_default()
                        .push(index);
                }
            }
        }

        parts[1] = step.elapsed().as_secs_f64() * 1000.0;
        let step = std::time::Instant::now();
        result.opaque = result.find_opaque(objects);
        parts[2] = step.elapsed().as_secs_f64() * 1000.0;
        result
    }

    /// Sections that must be kept whole.
    ///
    /// A reference that does not land on a symbol keeps its whole target
    /// section, because moving the atoms would move the bytes out from under
    /// it. Two shapes qualify: a section-relative target, and a symbol-relative
    /// one with a non-zero addend. References *from* metadata do not count —
    /// those are resolved by the code that understands the format, and are
    /// remapped rather than copied.
    fn find_opaque(&self, objects: &[LoadedObject]) -> HashSet<(u32, u32)> {
        let mut opaque = HashSet::default();
        for object in objects {
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
                        opaque.insert((object.parsed.id.0, section.0));
                    }
                    RelocationTarget::Symbol(id) => {
                        if !stores_addend(relocation)
                            || crate::inline_addend(object, relocation) == 0
                        {
                            continue;
                        }
                        // The addend is measured from the symbol, so the
                        // section holding that symbol is the one at risk.
                        let Some(symbol) = object.parsed.symbol(id) else {
                            continue;
                        };
                        if let Some((section, _)) = offset_of(object, symbol) {
                            opaque.insert((object.parsed.id.0, section.0));
                        }
                        for target in self.owners.get(symbol.name.as_str()).into_iter().flatten() {
                            opaque.insert(self.atoms[*target].key());
                        }
                    }
                }
            }
        }
        opaque
    }

    /// The atom holding `offset` bytes into `(object, section)`.
    fn containing(&self, object: ObjectId, section: SectionId, offset: u64) -> Option<usize> {
        let range = self.by_section.get(&(object.0, section.0))?.clone();
        let slice = &self.atoms[range.clone()];
        // The first atom starting after `offset`; the one before it is the
        // only candidate, because atoms tile their section in order.
        let index = slice
            .partition_point(|a| a.offset <= offset)
            .checked_sub(1)?;
        (offset < slice[index].end()).then_some(range.start + index)
    }

    /// The atom a relocation points at, if the target is one this link splits.
    fn target_atom(&self, object: &LoadedObject, relocation: &InputRelocation) -> Option<usize> {
        let RelocationTarget::Symbol(id) = relocation.target else {
            // A section-relative target keeps its section whole, so there is no
            // single atom to name and nothing to propagate to.
            return None;
        };
        let symbol = object.parsed.symbol(id)?;
        if symbol.strength.is_definition() {
            if let Some((section, offset)) = offset_of(object, symbol) {
                return self.containing(object.parsed.id, section, offset);
            }
        }
        // A reference resolves to whatever object defines the name.
        self.owners.get(symbol.name.as_str())?.first().copied()
    }

    /// Every atom defining `name`, for use as a root.
    fn defining(&self, name: &str) -> &[usize] {
        self.owners.get(name).map(Vec::as_slice).unwrap_or(&[])
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

/// Which atoms a program can reach from `entry`.
fn liveness(
    objects: &[LoadedObject],
    atoms: &Atoms<'_>,
    entry: &str,
    parts: &mut [f64; 2],
) -> (LiveSet, usize) {
    // Grouping is a pure function of one object — its relocations, by section,
    // sorted by offset — and it is rebuilt for all of them on every link. The
    // traversal that follows is genuinely global. Timed apart because only one
    // of the two can be held in a session.
    let step = std::time::Instant::now();
    let grouped: HashMap<u32, ByOffset<'_>> = objects
        .iter()
        .map(|o| (o.parsed.id.0, group_relocations(o)))
        .collect();
    let by_id: HashMap<u32, &LoadedObject> = objects.iter().map(|o| (o.parsed.id.0, o)).collect();
    parts[0] = step.elapsed().as_secs_f64() * 1000.0;
    let step = std::time::Instant::now();

    let mut live = LiveSet::with_capacity(atoms.atoms.len());
    let mut worklist: Vec<usize> = Vec::new();
    let mark = |index: usize, live: &mut LiveSet, worklist: &mut Vec<usize>| {
        if live.insert(index) {
            worklist.push(index);
        }
    };

    // Roots. The entry point, anything the object marked as never strippable,
    // and every atom of a section that has to be kept whole.
    for index in atoms.defining(entry) {
        mark(*index, &mut live, &mut worklist);
    }
    for (index, atom) in atoms.atoms.iter().enumerate() {
        if atoms.opaque.contains(&atom.key()) {
            mark(index, &mut live, &mut worklist);
        }
    }
    for object in objects {
        for symbol in &object.parsed.symbols {
            if !symbol.no_dead_strip || !symbol.strength.is_definition() {
                continue;
            }
            let Some((section, offset)) = offset_of(object, symbol) else {
                continue;
            };
            if let Some(index) = atoms.containing(object.parsed.id, section, offset) {
                mark(index, &mut live, &mut worklist);
            }
        }
    }
    // A section this link does not split still holds references, and they
    // still reach. `__data`, the thread-local block and any section in an
    // object without `MH_SUBSECTIONS_VIA_SYMBOLS` arrive here.
    for object in objects {
        for section in &object.parsed.sections {
            if atoms
                .by_section
                .contains_key(&(object.parsed.id.0, section.id.0))
                || is_metadata(&section.name)
                || section.kind == SectionKind::Debug
            {
                continue;
            }
            for relocation in object.parsed.relocations_for(section.id) {
                if let Some(index) = atoms.target_atom(object, relocation) {
                    mark(index, &mut live, &mut worklist);
                }
            }
        }
    }

    // Atoms whose own edges were deliberately not followed. Every other live
    // atom passes through the worklist exactly once and has its targets marked,
    // so these are the only places the invariant can be violated — and the only
    // places the verification below has to look.
    let mut suppressed: Vec<usize> = Vec::new();
    let propagate = |live: &mut LiveSet, worklist: &mut Vec<usize>, suppressed: &mut Vec<usize>| {
        while let Some(index) = worklist.pop() {
            let atom = &atoms.atoms[index];
            let Some(object) = by_id.get(&atom.object.0) else {
                continue;
            };
            let Some(grouped) = grouped.get(&atom.object.0) else {
                continue;
            };
            // Metadata's own references do not keep anything alive; it is kept
            // alive *by* what it describes, below.
            if object
                .parsed
                .section(atom.section)
                .is_some_and(|s| is_metadata(&s.name))
            {
                // Except forward: a live unwind record's exception table is
                // part of it, so an `__eh_frame` FDE that survives brings its
                // LSDA with it.
                if !object
                    .parsed
                    .section(atom.section)
                    .is_some_and(|s| s.name == "__eh_frame")
                {
                    suppressed.push(index);
                    continue;
                }
            }
            for relocation in within(grouped, atom) {
                if let Some(target) = atoms.target_atom(object, relocation) {
                    if live.insert(target) {
                        worklist.push(target);
                    }
                }
            }
        }
    };
    propagate(&mut live, &mut worklist, &mut suppressed);

    // Metadata comes alive with its subject, not before it.
    for object in objects {
        let Some(grouped) = grouped.get(&object.parsed.id.0) else {
            continue;
        };
        for section in &object.parsed.sections {
            match section.name.as_str() {
                "__eh_frame" => {
                    revive_eh_frame(object, atoms, grouped, section, &mut live, &mut worklist)
                }
                "__compact_unwind" => {
                    revive_lsdas(object, atoms, grouped, section, &mut live, &mut worklist)
                }
                _ => {}
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
            let atom = &atoms.atoms[index];
            let (Some(object), Some(grouped)) =
                (by_id.get(&atom.object.0), grouped.get(&atom.object.0))
            else {
                continue;
            };
            for relocation in within(grouped, atom) {
                if let Some(target) = atoms.target_atom(object, relocation) {
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
            if live.insert(index) {
                worklist.push(index);
            }
        }
        propagate(&mut live, &mut worklist, &mut suppressed);
    }

    parts[1] = step.elapsed().as_secs_f64() * 1000.0;
    (live, revived)
}

/// An `__eh_frame` record is live when the function it describes is.
///
/// CIEs are kept unconditionally: there are a handful per link, every live FDE
/// needs one, and deciding which would cost more than the bytes are worth.
fn revive_eh_frame(
    object: &LoadedObject,
    atoms: &Atoms<'_>,
    grouped: &ByOffset<'_>,
    section: &InputSection,
    live: &mut LiveSet,
    worklist: &mut Vec<usize>,
) {
    let Some(range) = atoms
        .by_section
        .get(&(object.parsed.id.0, section.id.0))
        .cloned()
    else {
        return;
    };
    let Some(file_offset) = section.file_offset else {
        return;
    };
    for index in range {
        let atom = &atoms.atoms[index];
        let at = (file_offset + atom.offset) as usize;
        let id = object
            .data
            .get(at + 4..at + 8)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
            .unwrap_or(0);
        if id == 0 {
            // A CIE.
            if live.insert(index) {
                worklist.push(index);
            }
            continue;
        }
        // `PC begin` follows the CIE pointer, and carries the relocation
        // naming the function. Both halves of the pair share that offset; the
        // one that is not the `SUBTRACTOR` names the function.
        let function = within(grouped, atom)
            .iter()
            .find(|r| r.offset == atom.offset + 8 && r.kind != Arm64RelocationKind::Subtractor)
            .and_then(|r| atoms.target_atom(object, r));
        if function.is_some_and(|f| live.contains(f)) && live.insert(index) {
            worklist.push(index);
        }
    }
}

/// A compact unwind record's exception table is live when its function is.
///
/// The record itself never reaches the output — it is input to
/// `__unwind_info` — but the `__gcc_except_tab` atom it points at does, and
/// nothing else refers to that atom when the encoding is not DWARF mode.
fn revive_lsdas(
    object: &LoadedObject,
    atoms: &Atoms<'_>,
    grouped: &ByOffset<'_>,
    section: &InputSection,
    live: &mut LiveSet,
    worklist: &mut Vec<usize>,
) {
    let Some(list) = grouped.get(&section.id.0) else {
        return;
    };
    let mut functions: HashMap<u64, Option<usize>> = HashMap::default();
    let mut lsdas: HashMap<u64, usize> = HashMap::default();
    for relocation in list {
        let record = relocation.offset / COMPACT_UNWIND_RECORD;
        match relocation.offset % COMPACT_UNWIND_RECORD {
            CU_FUNCTION => {
                functions.insert(record, function_atom(object, atoms, relocation));
            }
            CU_LSDA => {
                if let Some(index) = atoms.target_atom(object, relocation) {
                    lsdas.insert(record, index);
                }
            }
            _ => {}
        }
    }
    for (record, lsda) in lsdas {
        let alive = functions
            .get(&record)
            .and_then(|f| *f)
            .is_some_and(|f| live.contains(f));
        if alive && live.insert(lsda) {
            worklist.push(lsda);
        }
    }
}

/// The atom a `__compact_unwind` function pointer names.
///
/// Unlike everything else in the link, this is a *section* relocation with the
/// function's address stored inline, so the offset has to be recovered before
/// the atom can be found.
fn function_atom(
    object: &LoadedObject,
    atoms: &Atoms<'_>,
    relocation: &InputRelocation,
) -> Option<usize> {
    match relocation.target {
        RelocationTarget::Section(id) => {
            let origin = object.parsed.section(id)?.vm_address;
            let inline = crate::inline_addend(object, relocation) as u64;
            atoms.containing(object.parsed.id, id, inline.saturating_sub(origin))
        }
        RelocationTarget::Symbol(_) => atoms.target_atom(object, relocation),
    }
}

/// One run of surviving bytes, and where it moved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Piece {
    pub from: u64,
    pub size: u64,
    pub to: u64,
}

/// A section that lost bytes, and the map from old offsets to new ones.
#[derive(Debug, Clone, Default)]
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
#[derive(Debug, Clone, Default)]
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
                let Some(range) = atoms.by_section.get(&key).cloned() else {
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
                    let atom = &atoms.atoms[index];
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
    /// Inside `liveness_ms`: grouping relocations per object, then traversing.
    pub group_ms: f64,
    pub traverse_ms: f64,
}

/// Decide what a link keeps.
pub(crate) fn plan(objects: &[LoadedObject], entry: &str) -> (Strip, Report, StripTimings) {
    let mut timings = StripTimings::default();
    let step = std::time::Instant::now();
    let mut parts = [0.0f64; 3];
    let atoms = Atoms::build(objects, &mut parts);
    timings.atoms_ms = step.elapsed().as_secs_f64() * 1000.0;

    let step = std::time::Instant::now();
    let mut live_parts = [0.0f64; 2];
    let (live, revived) = liveness(objects, &atoms, entry, &mut live_parts);
    timings.liveness_ms = step.elapsed().as_secs_f64() * 1000.0;
    timings.group_ms = live_parts[0];
    timings.traverse_ms = live_parts[1];

    let step = std::time::Instant::now();
    let strip = Strip::build(objects, &atoms, &live);
    timings.build_ms = step.elapsed().as_secs_f64() * 1000.0;

    (strip, report(objects, &atoms, &live, revived), timings)
}

/// Report what analysis alone can say, without changing any output.
pub(crate) fn analyse(objects: &[LoadedObject], entry: &str) -> Report {
    plan(objects, entry).1
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
    for (index, atom) in atoms.atoms.iter().enumerate() {
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
