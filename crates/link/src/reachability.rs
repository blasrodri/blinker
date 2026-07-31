//! Which code a program can actually reach.
//!
//! This is the analysis half of dead-stripping (finding 70), built and
//! measured before anything depends on it. It changes no output: it reports
//! what *would* be removed, so the model can be checked against the linker
//! that already does this correctly before the layout is rebuilt around it.
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
//! the next symbol's, or to the end of its section.
//!
//! # What this deliberately does not model
//!
//! Only `__text`, because that is where two thirds of the size gap is and the
//! rest follows from it: unwind tables, exception tables and literals exist to
//! serve code, and are dropped alongside the functions they describe. Counting
//! them here would mean modelling that relationship for a number that adds
//! nothing to the decision.
//!
//! Anything it cannot prove dead, it reports live. An over-estimate of live
//! code makes the prediction conservative; an under-estimate would suggest a
//! stripper can remove something it must not.

use crate::LoadedObject;
use blinker_macho::{ObjectId, RelocationTarget, SectionId};
use std::collections::{HashMap, HashSet};

/// One symbol's worth of code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Atom {
    pub object: ObjectId,
    pub section: SectionId,
    /// Offset of this atom within its input section.
    pub offset: u64,
    pub size: u64,
}

/// What the analysis found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub live_atoms: usize,
    pub total_atoms: usize,
    pub live_bytes: u64,
    pub total_bytes: u64,
    /// Bytes in input sections where *no* atom is live.
    ///
    /// A strict subset of `dead_bytes`, and the part reachable without
    /// changing the unit of layout: a section nothing reaches can be dropped
    /// from placement whole, exactly like a linker-internal one. Measured
    /// separately because it decides whether there is a safe increment worth
    /// landing before atoms replace sections everywhere.
    pub fully_dead_section_bytes: u64,
}

impl Report {
    pub fn dead_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.live_bytes)
    }
}

/// Whether a section describes code rather than using it.
///
/// Unwind and exception tables are emitted per function and reference it by
/// name; keeping a function alive because its own unwind entry mentions it
/// would make every function live.
fn is_metadata(name: &str) -> bool {
    matches!(
        name,
        "__eh_frame" | "__compact_unwind" | "__gcc_except_tab" | "__unwind_info"
    ) || name.starts_with("__debug")
        || name.starts_with("__zdebug")
}

/// An atom, plus the names it defines and refers to.
struct Node {
    atom: Atom,
    /// Names this atom defines. Usually one; aliases give more.
    defines: Vec<String>,
    /// Names its relocations reach.
    refers: Vec<String>,
}

/// Split every `__text` section into atoms and record what each one reaches.
fn build(objects: &[LoadedObject]) -> Vec<Node> {
    let mut nodes = Vec::new();
    for object in objects {
        for section in &object.parsed.sections {
            if section.name != "__text" {
                continue;
            }
            // Defined symbols in this section, by offset. Several names can
            // share an offset — an alias defines the same bytes — so they are
            // grouped rather than assumed unique.
            let mut by_offset: HashMap<u64, Vec<String>> = HashMap::new();
            for symbol in &object.parsed.symbols {
                if symbol.section != Some(section.id) || !symbol.strength.is_definition() {
                    continue;
                }
                by_offset
                    .entry(symbol.value.saturating_sub(section.vm_address))
                    .or_default()
                    .push(symbol.name.clone());
            }
            let mut offsets: Vec<u64> = by_offset.keys().copied().collect();
            offsets.sort_unstable();

            // A section whose first symbol is not at zero has bytes before it
            // that no symbol names. Nothing can refer to them individually, so
            // they are attributed to the first atom rather than dropped.
            if let Some(first) = offsets.first().copied() {
                if first > 0 {
                    if let Some(names) = by_offset.remove(&first) {
                        by_offset.insert(0, names);
                        offsets[0] = 0;
                    }
                }
            }

            for (index, offset) in offsets.iter().copied().enumerate() {
                let end = offsets.get(index + 1).copied().unwrap_or(section.size);
                let atom = Atom {
                    object: object.parsed.id,
                    section: section.id,
                    offset,
                    size: end.saturating_sub(offset),
                };
                let refers = object
                    .parsed
                    .relocations
                    .iter()
                    .filter(|r| r.section == section.id && r.offset >= offset && r.offset < end)
                    .filter_map(|r| match r.target {
                        RelocationTarget::Symbol(id) => {
                            object.parsed.symbol(id).map(|s| s.name.clone())
                        }
                        // A section-relative reference names no symbol, and
                        // resolving which atom it lands in needs the inline
                        // addend. Left unmodelled, which is why sections
                        // reached this way are kept whole below.
                        RelocationTarget::Section(_) => None,
                    })
                    .collect();
                nodes.push(Node {
                    atom,
                    defines: by_offset.get(&offset).cloned().unwrap_or_default(),
                    refers,
                });
            }
        }
    }
    nodes
}

/// Report how much `__text` a program can reach from `entry`.
///
/// Roots are the entry symbol and every `__text` symbol named from outside
/// `__text` — data holding a function pointer, a vtable, an unwind table. Any
/// atom reached from those, transitively through its relocations, is live.
pub(crate) fn analyse(objects: &[LoadedObject], entry: &str) -> Report {
    let nodes = build(objects);

    // Name -> the atoms defining it. A name normally has one definition; weak
    // symbols may have several, and all of them must be kept live because
    // which one wins is decided elsewhere.
    let mut owners: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        for name in &node.defines {
            owners.entry(name.as_str()).or_default().push(index);
        }
    }

    let mut worklist: Vec<usize> = Vec::new();
    let mut live: HashSet<usize> = HashSet::new();
    let root = |name: &str, live: &mut HashSet<usize>, worklist: &mut Vec<usize>| {
        for index in owners.get(name).into_iter().flatten() {
            if live.insert(*index) {
                worklist.push(*index);
            }
        }
    };
    root(entry, &mut live, &mut worklist);

    // Anything named from outside `__text` is a root — data holding a function
    // pointer, a vtable — because this analysis cannot see how those sections
    // are reached and must not claim their targets dead.
    //
    // Except metadata *about* code. `__eh_frame`, `__compact_unwind` and
    // `__gcc_except_tab` name every function in the object, so rooting from
    // them makes everything live: a first version reported 2274 of 2274 atoms
    // reachable, which is the analysis saying nothing at all. They describe a
    // function rather than use it, and are dropped along with it.
    for object in objects {
        for relocation in &object.parsed.relocations {
            let outside = object
                .parsed
                .section(relocation.section)
                .is_some_and(|s| !is_metadata(&s.name) && s.name != "__text");
            if !outside {
                continue;
            }
            if let RelocationTarget::Symbol(id) = relocation.target {
                if let Some(symbol) = object.parsed.symbol(id) {
                    root(&symbol.name, &mut live, &mut worklist);
                }
            }
        }
    }

    // A section-relative reference names no symbol, so the atom it lands in
    // cannot be identified. Every atom of such a section stays live rather
    // than risk removing the one that was meant.
    let opaque: HashSet<(u32, u32)> = objects
        .iter()
        .flat_map(|o| o.parsed.relocations.iter().map(move |r| (o, r)))
        .filter(|(o, r)| {
            // A reference *from* metadata cannot keep an atom alive either.
            o.parsed
                .section(r.section)
                .is_some_and(|s| !is_metadata(&s.name))
        })
        .filter_map(|(o, r)| match r.target {
            RelocationTarget::Section(section) => Some((o.parsed.id.0, section.0)),
            RelocationTarget::Symbol(_) => None,
        })
        .collect();
    for (index, node) in nodes.iter().enumerate() {
        if opaque.contains(&(node.atom.object.0, node.atom.section.0)) && live.insert(index) {
            worklist.push(index);
        }
    }

    while let Some(index) = worklist.pop() {
        for name in nodes[index].refers.clone() {
            root(&name, &mut live, &mut worklist);
        }
    }

    // Sections with no live atom at all.
    let mut by_section: HashMap<(u32, u32), (u64, bool)> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        let entry = by_section
            .entry((node.atom.object.0, node.atom.section.0))
            .or_insert((0, false));
        entry.0 += node.atom.size;
        entry.1 |= live.contains(&index);
    }

    Report {
        live_atoms: live.len(),
        total_atoms: nodes.len(),
        live_bytes: live.iter().map(|i| nodes[*i].atom.size).sum(),
        total_bytes: nodes.iter().map(|n| n.atom.size).sum(),
        fully_dead_section_bytes: by_section
            .values()
            .filter(|(_, any_live)| !any_live)
            .map(|(size, _)| *size)
            .sum(),
    }
}
