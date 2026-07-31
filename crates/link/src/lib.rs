//! The link itself: object files in, an executable out.
//!
//! Every other crate here does one stage well and is tested in isolation.
//! This one is the seam between them, and seams are where the errors that
//! isolated tests cannot see live — a symbol whose address is computed in one
//! coordinate system and consumed in another, a section copied to the right
//! offset in the wrong buffer. So the test for this crate is not "did the
//! pieces get called" but "does the program run and print what it should".
//!
//! ```text
//! parse each object      blinker-macho
//!   → resolve symbols    blinker-symbols
//!   → place sections     blinker-layout
//!   → copy content       here
//!   → patch relocations  blinker-relocations
//!   → emit and sign      blinker-output
//! ```
//!
//! # Why the layout is computed twice
//!
//! Relocations need the addresses that layout assigns, and layout runs inside
//! `ImageBuilder::build`. Rather than duplicate the layout computation — where
//! a divergence would put relocations at addresses the emitted image does not
//! use — the image is built once to *learn* the layout, the relocations are
//! applied against it, and it is built again with the patched bytes. Layout
//! depends only on section sizes and alignments, which the patching does not
//! change, so the two passes agree by construction.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use blinker_layout::InputPlacement;
use blinker_macho::{
    parse_object, Arm64RelocationKind, InputRelocation, InputSection, ObjectId, ParsedObject,
    RelocationLength, RelocationTarget, SectionId, SectionKind, SymbolId, SymbolVisibility,
};
use blinker_output::image::Dylib;
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Bind, Image, ImageBuilder, Rebase, UnwindEntry};
use blinker_relocations::{apply, Context};
use blinker_symbols::{SymbolProvider, SymbolTable};

pub mod error;
pub use error::LinkError;

/// How long each stage of a link took.
///
/// Recorded because M4's cache has to know *what* to cache. Caching the wrong
/// stage buys nothing: if emitting dominates, storing parse results saves
/// almost none of the 44.6 ms a cold link costs. This is measured rather than
/// assumed for the same reason every other number in this project is.
#[derive(Debug, Clone, Default)]
pub struct LinkTimings {
    pub read_and_parse_ms: f64,
    pub resolve_ms: f64,
    pub layout_probe_ms: f64,
    pub relocate_ms: f64,
    pub emit_ms: f64,
    pub total_ms: f64,
    /// Objects whose patched bytes came from the cache rather than from
    /// relocating them again. Zero on a cold link, and on every link that did
    /// not ask for a cache.
    pub reused_objects: u64,
}

impl std::fmt::Display for LinkTimings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pct = |v: f64| {
            if self.total_ms > 0.0 {
                v / self.total_ms * 100.0
            } else {
                0.0
            }
        };
        if self.reused_objects > 0 {
            writeln!(f, "  reused      {:7} objects", self.reused_objects)?;
        }
        writeln!(
            f,
            "  read+parse  {:7.1} ms  {:5.1}%",
            self.read_and_parse_ms,
            pct(self.read_and_parse_ms)
        )?;
        writeln!(
            f,
            "  resolve     {:7.1} ms  {:5.1}%",
            self.resolve_ms,
            pct(self.resolve_ms)
        )?;
        writeln!(
            f,
            "  layout      {:7.1} ms  {:5.1}%",
            self.layout_probe_ms,
            pct(self.layout_probe_ms)
        )?;
        writeln!(
            f,
            "  relocate    {:7.1} ms  {:5.1}%",
            self.relocate_ms,
            pct(self.relocate_ms)
        )?;
        writeln!(
            f,
            "  emit+sign   {:7.1} ms  {:5.1}%",
            self.emit_ms,
            pct(self.emit_ms)
        )?;
        write!(f, "  total       {:7.1} ms", self.total_ms)
    }
}

/// Link, reporting how long each stage took.
pub fn link_timed(request: &LinkRequest) -> Result<(Image, LinkTimings), LinkError> {
    let mut timings = LinkTimings::default();
    let image = link_inner(request, &mut timings)?;
    Ok((image, timings))
}

/// What to link.
#[derive(Debug, Clone)]
pub struct LinkRequest {
    pub objects: Vec<PathBuf>,
    /// Symbol the image enters at.
    pub entry_symbol: String,
    /// Identifier embedded in the ad-hoc signature; conventionally the output
    /// file's base name.
    pub identifier: String,
    pub dylibs: Vec<Dylib>,
    /// `.tbd` stubs describing what the dylibs export.
    pub stub_libraries: Vec<PathBuf>,
    /// Where the incremental cache lives, when one is wanted.
    ///
    /// `None` disables the cache entirely, which is what every test that is
    /// not *about* the cache should use: a link that silently reads state from
    /// a previous run is a link whose result depends on history, and that is
    /// the one thing a correctness test must not tolerate.
    pub cache_path: Option<PathBuf>,
}

impl LinkRequest {
    pub fn new(objects: Vec<PathBuf>) -> Self {
        LinkRequest {
            objects,
            entry_symbol: "_main".to_string(),
            identifier: "a.out".to_string(),
            dylibs: vec![Dylib::lib_system()],
            stub_libraries: default_stub_library().into_iter().collect(),
            cache_path: None,
        }
    }

    /// Cache this link's relocated output under `path`, and reuse what a
    /// previous link left there.
    pub fn cached_at(mut self, path: PathBuf) -> Self {
        self.cache_path = Some(path);
        self
    }

    pub fn identifier(mut self, identifier: &str) -> Self {
        self.identifier = identifier.to_string();
        self
    }

    /// Use a `.tbd` stub library as the source of importable symbols.
    pub fn stub_library(mut self, path: PathBuf) -> Self {
        self.stub_libraries.push(path);
        self
    }

    /// Every symbol the stub libraries export for this target.
    ///
    /// `None` when no stub library was supplied, which is different from an
    /// empty set: with no library, nothing can be imported and every
    /// undefined reference is an error.
    fn dynamic_symbols(&self) -> Option<BTreeSet<String>> {
        if self.stub_libraries.is_empty() {
            return None;
        }
        let mut all = BTreeSet::new();
        for path in &self.stub_libraries {
            let Ok(file) = blinker_tbd::parse_tbd_file(path) else {
                continue;
            };
            all.extend(file.exported_symbols(blinker_tbd::Target::aarch64_macos()));
        }
        Some(all)
    }
}

/// Where the SDK keeps `libSystem`'s stub, if it can be found.
///
/// # Why this is cached
///
/// Asking `xcrun` costs **14 ms** — a third of a 40 ms link, spent spawning a
/// process to learn a path that cannot change while blinker is running. It was
/// called from `LinkRequest::new`, so every link paid it, and because it
/// happened before the link's own timers started it appeared as unexplained
/// overhead rather than as a phase.
///
/// `SDKROOT` is honoured first: the compiler driver sets it, so in a real
/// build the answer is already in the environment and no process need be
/// spawned at all. `xcrun` remains the fallback, because the SDK genuinely
/// does move between Xcode versions and the Command Line Tools.
pub fn default_stub_library() -> Option<PathBuf> {
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED.get_or_init(discover_stub_library).clone()
}

fn discover_stub_library() -> Option<PathBuf> {
    let stub_in = |sdk: &str| {
        let path = PathBuf::from(sdk).join("usr/lib/libSystem.tbd");
        path.exists().then_some(path)
    };

    if let Ok(sdk) = std::env::var("SDKROOT") {
        if let Some(path) = stub_in(&sdk) {
            return Some(path);
        }
    }

    let output = std::process::Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    stub_in(String::from_utf8_lossy(&output.stdout).trim())
}

/// An object file and the bytes it was parsed from.
///
/// The bytes are kept because section *content* is read from them later;
/// `ParsedObject` describes where the content is, not what it is.
struct LoadedObject {
    parsed: ParsedObject,
    data: Vec<u8>,
}

/// Sections that exist for the linker's benefit and must not reach the output.
///
/// `__LD,__compact_unwind` is the clearest case: it is *input* to unwind-table
/// generation, and ld64 consumes it to synthesise `__TEXT,__unwind_info`.
/// Copying it through would put a linker-internal segment in the image.
fn is_linker_internal(section: &InputSection) -> bool {
    section.segment == "__LD"
        || section.kind == SectionKind::Debug
        || section.name == "__compact_unwind"
}

/// Section id of the synthesised `__unwind_info`.
const UNWIND_SECTION: SectionId = SectionId(3);

/// Bytes in one `__LD,__compact_unwind` record.
const COMPACT_UNWIND_RECORD: u64 = 32;

/// Field offsets within a compact unwind record.
const CU_FUNCTION: u64 = 0;
const CU_LENGTH: u64 = 8;
const CU_ENCODING: u64 = 12;
const CU_PERSONALITY: u64 = 16;
const CU_LSDA: u64 = 24;

/// `UNWIND_ARM64_MODE_MASK` and the DWARF mode value.
const UNWIND_MODE_MASK: u32 = 0x0f00_0000;
const UNWIND_MODE_DWARF: u32 = 0x0300_0000;
/// Low 24 bits of a DWARF-mode encoding hold the FDE offset.
const UNWIND_DWARF_OFFSET_MASK: u32 = 0x00ff_ffff;

/// Read a ULEB128 value, returning it and the position after it.
fn uleb128(data: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let (mut value, mut shift) = (0u64, 0u32);
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, pos));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Skip a SLEB128 value, returning the position after it.
fn skip_sleb128(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        if byte & 0x80 == 0 {
            return Some(pos);
        }
    }
}

/// `DW_EH_PE_indirect` — the encoded value addresses a *slot* holding the
/// real pointer, rather than being the pointer.
const DW_EH_PE_INDIRECT: u8 = 0x80;

/// Where each CIE stores its personality reference.
///
/// # Why this has to parse the augmentation
///
/// A CIE names its personality routine only in its augmentation data, and
/// nothing else in the object says which relocation that is. blinker's first
/// attempt at this keyed on personality symbols collected from
/// `__compact_unwind` — which, in DWARF mode, contains none (finding 31), so
/// the code was inert (finding 49).
///
/// The layout being walked, per the DWARF CFI format:
///
/// ```text
/// length, CIE id (0), version, augmentation string,
/// code alignment (ULEB), data alignment (SLEB), return register,
/// if augmentation begins with 'z': augmentation length (ULEB), then one
///   entry per remaining character — 'P' is an encoding byte followed by the
///   personality pointer, 'L' and 'R' are a single byte each.
/// ```
///
/// Returns the offsets, *within each input section*, of personality fields
/// that use an indirect encoding — the only ones that must name a GOT slot.
fn eh_frame_personality_fields(
    object: &LoadedObject,
    section: &InputSection,
) -> std::collections::HashSet<u64> {
    let mut fields = std::collections::HashSet::new();
    let Some(file_offset) = section.file_offset else {
        return fields;
    };
    let data = &object.data;
    let base = file_offset as usize;

    let mut position = 0u64;
    while position + 8 <= section.size {
        let at = base + position as usize;
        let Some(bytes) = data.get(at..at + 8) else {
            break;
        };
        let length = u32::from_le_bytes(bytes[0..4].try_into().expect("4")) as u64;
        if length == 0 {
            break;
        }
        let id = u32::from_le_bytes(bytes[4..8].try_into().expect("4"));

        // Only CIEs carry an augmentation; an FDE's second word is the
        // distance back to its CIE.
        if id == 0 {
            if let Some(offset) = personality_field_in_cie(data, at + 8, position + 8) {
                fields.insert(offset);
            }
        }
        position += 4 + length;
    }
    fields
}

/// The section-relative offset of this CIE's indirect personality field.
fn personality_field_in_cie(data: &[u8], start: usize, start_offset: u64) -> Option<u64> {
    // `at` indexes the file; `offset` is the same position expressed relative
    // to the section, which is what a relocation records. They advance
    // together, so one delta keeps both correct.
    let mut at = start;
    let section_delta = start_offset.wrapping_sub(start as u64);

    let _version = *data.get(at)?;
    at += 1;

    let mut augmentation = Vec::new();
    while *data.get(at)? != 0 {
        augmentation.push(*data.get(at)?);
        at += 1;
    }
    at += 1; // the NUL

    if augmentation.first() != Some(&b'z') {
        return None;
    }

    (_, at) = uleb128(data, at)?; // code alignment factor
    at = skip_sleb128(data, at)?; // data alignment factor
    (_, at) = uleb128(data, at)?; // return address register
    (_, at) = uleb128(data, at)?; // augmentation data length

    for entry in &augmentation[1..] {
        match entry {
            b'P' => {
                let encoding = *data.get(at)?;
                at += 1;
                // The field starts here. Only an indirect encoding needs a GOT
                // slot; a direct one genuinely wants the symbol's address.
                return (encoding & DW_EH_PE_INDIRECT != 0)
                    .then_some((at as u64).wrapping_add(section_delta));
            }
            b'L' | b'R' => at += 1,
            _ => return None,
        }
    }
    None
}

/// Map each function to the offset of its FDE within the output `__eh_frame`.
///
/// # Why the records are walked but not decoded
///
/// A DWARF-mode compact unwind encoding is a *pointer*: its low 24 bits are
/// the offset of the function's FDE in `__eh_frame`, and the unwinder follows
/// it to the real description. Producing those offsets needs to know where
/// each FDE begins and which function it covers.
///
/// Finding the boundaries needs only the length field every record starts
/// with. Finding the *function* would normally mean decoding the CIE's
/// augmentation string to learn how the FDE's `PC begin` field is encoded —
/// but in a relocatable object that field carries a **relocation**, so the
/// answer is already available from the relocation list, in the same form the
/// rest of the linker uses. Decoding DWARF pointer encodings here would be
/// re-deriving something the object already states.
fn eh_frame_fde_offsets(
    objects: &[LoadedObject],
    image: &Image,
    addresses: &AddressMap,
) -> HashMap<u64, u32> {
    let mut offsets = HashMap::new();

    let Some(output) = image
        .layout
        .sections
        .iter()
        .find(|s| s.name == "__eh_frame")
    else {
        return offsets;
    };

    for object in objects {
        for section in &object.parsed.sections {
            if section.name != "__eh_frame" {
                continue;
            }
            let Some(file_offset) = section.file_offset else {
                continue;
            };
            // Where this object's records begin within the output section.
            let Some(chunk) = output.address_of(object.parsed.id, section.id) else {
                continue;
            };
            let chunk_offset = chunk - output.vm_address;

            // Relocations in this section, by offset.
            let mut targets: HashMap<u64, u64> = HashMap::new();
            for relocation in object
                .parsed
                .relocations
                .iter()
                .filter(|r| r.section == section.id)
            {
                if let Ok(address) = target_address(object, image, addresses, relocation.target) {
                    targets.insert(relocation.offset, address);
                }
            }

            let mut position = 0u64;
            while position + 8 <= section.size {
                let at = (file_offset + position) as usize;
                let Some(length_bytes) = object.data.get(at..at + 4) else {
                    break;
                };
                let length = u32::from_le_bytes(length_bytes.try_into().expect("4 bytes")) as u64;
                // A zero length terminates the section.
                if length == 0 {
                    break;
                }
                let record_size = 4 + length;

                let id = object
                    .data
                    .get(at + 4..at + 8)
                    .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                    .unwrap_or(0);

                // Zero identifies a CIE; anything else is an FDE whose value is
                // the distance back to its CIE.
                if id != 0 {
                    // `PC begin` immediately follows the CIE pointer.
                    if let Some(function) = targets.get(&(position + 8)) {
                        offsets.insert(*function, (chunk_offset + position) as u32);
                    }
                }

                position += record_size;
            }
        }
    }
    offsets
}

/// Everything a single pass over the relocations can decide.
///
/// These four questions used to be four separate walks over every relocation
/// of every object — plus two more for the output symbol table and the
/// undefined set. Profiling put that repeated traversal at 31% of the link,
/// larger than any named stage, so they are answered together.
///
/// The order still matters: stubs are only needed for symbols that turned out
/// to be imports, so the caller supplies that set.
#[derive(Default)]
struct RelocationSurvey {
    got: Vec<TableEntry>,
    tlv: Vec<TableEntry>,
    stubs: Vec<String>,
    personalities: Vec<TableEntry>,
}

fn survey_relocations(objects: &[LoadedObject], imports: &[String]) -> RelocationSurvey {
    let imported: std::collections::HashSet<&str> = imports.iter().map(String::as_str).collect();
    let mut survey = RelocationSurvey::default();
    let (mut got_seen, mut tlv_seen, mut stub_seen, mut personality_seen) = (
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    );

    for object in objects {
        // Which sections are `__compact_unwind`, so personality relocations can
        // be recognised without a second scan.
        let unwind_sections: std::collections::HashSet<SectionId> = object
            .parsed
            .sections
            .iter()
            .filter(|s| s.name == "__compact_unwind")
            .map(|s| s.id)
            .collect();

        for relocation in &object.parsed.relocations {
            let RelocationTarget::Symbol(id) = relocation.target else {
                continue;
            };
            let Some(symbol) = object.parsed.symbol(id) else {
                continue;
            };
            let entry = || TableEntry {
                object: object.parsed.id,
                name: symbol.name.clone(),
            };

            if needs_got(relocation.kind) && got_seen.insert(symbol.name.clone()) {
                survey.got.push(entry());
            }
            if needs_tlv(relocation.kind) && tlv_seen.insert(symbol.name.clone()) {
                survey.tlv.push(entry());
            }
            if relocation.kind == Arm64RelocationKind::Branch26
                && imported.contains(symbol.name.as_str())
                && stub_seen.insert(symbol.name.clone())
            {
                survey.stubs.push(symbol.name.clone());
            }
            if unwind_sections.contains(&relocation.section)
                && relocation.offset % COMPACT_UNWIND_RECORD == CU_PERSONALITY
                && personality_seen.insert(symbol.name.clone())
            {
                survey.personalities.push(entry());
            }
        }
    }

    survey.stubs.sort();
    survey
}

/// Read the compact unwind records the compiler emitted.
///
/// Each record names a function, how long it is, how to restore its frame, and
/// optionally a personality routine and an LSDA. The pointers are *relocations*
/// rather than values — the object is not laid out yet — so the targets come
/// from the relocation list and the scalars from the section bytes.
fn compact_unwind_entries(
    objects: &[LoadedObject],
    image: &Image,
    addresses: &AddressMap,
    got_slots: &HashMap<String, u64>,
    fde_offsets: &HashMap<u64, u32>,
) -> Vec<UnwindEntry> {
    let Some(text) = image.layout.segment("__TEXT") else {
        return Vec::new();
    };
    let image_base = text.vm_address;
    let mut entries = Vec::new();

    for object in objects {
        for section in &object.parsed.sections {
            if section.name != "__compact_unwind" {
                continue;
            }
            let Some(file_offset) = section.file_offset else {
                continue;
            };

            // Relocation targets, indexed by (record, field).
            //
            // The addend is stored **inline**, in the eight bytes being
            // patched, not in the relocation entry. Ignoring it made every
            // record that points into `__text` resolve to the same section
            // base: 469 functions collapsed to 17 distinct offsets, and the
            // unwinder was handed a table describing almost nothing.
            //
            // For a section target the inline value is an address in the
            // object's own coordinate space, so the offset within that section
            // has to be recovered before rebasing onto the output address.
            let mut targets: HashMap<(u64, u64), u64> = HashMap::new();
            // Personalities are recorded by GOT slot rather than by address,
            // so they are collected by name and resolved separately.
            let mut personality_names: HashMap<u64, String> = HashMap::new();
            for relocation in object
                .parsed
                .relocations
                .iter()
                .filter(|r| r.section == section.id)
            {
                let Ok(base) = target_address(object, image, addresses, relocation.target) else {
                    continue;
                };

                let field_at = (file_offset + relocation.offset) as usize;
                let inline = object
                    .data
                    .get(field_at..field_at + 8)
                    .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
                    .unwrap_or(0);

                let address = match relocation.target {
                    RelocationTarget::Section(id) => {
                        let origin = object.parsed.section(id).map(|s| s.vm_address).unwrap_or(0);
                        base + inline.saturating_sub(origin)
                    }
                    // For a symbol the inline value is a plain addend.
                    RelocationTarget::Symbol(_) => base + inline,
                };

                let record = relocation.offset / COMPACT_UNWIND_RECORD;
                let field = relocation.offset % COMPACT_UNWIND_RECORD;

                if field == CU_PERSONALITY {
                    if let RelocationTarget::Symbol(id) = relocation.target {
                        if let Some(symbol) = object.parsed.symbol(id) {
                            personality_names.insert(record, symbol.name.clone());
                        }
                    }
                }
                targets.insert((record, field), address);
            }

            let count = section.size / COMPACT_UNWIND_RECORD;
            for record in 0..count {
                let base = file_offset + record * COMPACT_UNWIND_RECORD;
                let read_u32 = |at: u64| -> Option<u32> {
                    let start = (base + at) as usize;
                    object
                        .data
                        .get(start..start + 4)
                        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                };

                // A record whose function pointer has no relocation refers to
                // a function that was dead-stripped or never placed.
                let Some(function) = targets.get(&(record, CU_FUNCTION)) else {
                    continue;
                };
                let Some(encoding) = read_u32(CU_ENCODING) else {
                    continue;
                };
                let _length = read_u32(CU_LENGTH);

                // A DWARF-mode encoding must carry the offset of this
                // function's FDE. Without it the unwinder follows a zero and
                // reads the start of `__eh_frame` for every function.
                let encoding = if encoding & UNWIND_MODE_MASK == UNWIND_MODE_DWARF {
                    let fde = fde_offsets.get(function).copied().unwrap_or(0);
                    (encoding & !UNWIND_DWARF_OFFSET_MASK) | (fde & UNWIND_DWARF_OFFSET_MASK)
                } else {
                    encoding
                };

                entries.push(UnwindEntry {
                    function_offset: (function - image_base) as u32,
                    encoding,
                    personality: personality_names
                        .get(&record)
                        .and_then(|name| got_slots.get(name))
                        .map(|slot| (slot - image_base) as u32),
                    lsda: targets
                        .get(&(record, CU_LSDA))
                        .map(|a| (a - image_base) as u32),
                });
            }
        }
    }
    entries
}

/// Section id of the synthesised `__thread_ptrs`.
const TLV_SECTION: SectionId = SectionId(2);

/// Relocation kinds that reach their target through a thread-local pointer.
fn needs_tlv(kind: Arm64RelocationKind) -> bool {
    matches!(
        kind,
        Arm64RelocationKind::TlvpLoadPage21 | Arm64RelocationKind::TlvpLoadPageOff12
    )
}

/// One slot of a synthesised pointer table.
///
/// The owning object is carried, not just the name. A thread-local or a
/// GOT target may be a **local** symbol, and locals are keyed per object
/// because two objects may legitimately define the same local name. Looking
/// one up under the linker's own synthetic object id finds nothing, and the
/// slot was then left zero — which is a null descriptor pointer, and a crash
/// on first use rather than a link error.
#[derive(Debug, Clone)]
struct TableEntry {
    object: ObjectId,
    name: String,
}

/// Section id of the synthesised `__stubs`.
const STUBS_SECTION: SectionId = SectionId(1);

/// Bytes per stub: three instructions.
const STUB_SIZE: u64 = 12;

/// `BR x16` — the last instruction of every stub.
const BR_X16: u32 = 0xD61F_0200;

/// Build one stub: load the GOT slot's contents and jump to it.
///
/// ```text
/// adrp x16, <got page>
/// ldr  x16, [x16, <got page offset>]
/// br   x16
/// ```
///
/// This is the *non-lazy* form. ld64's default stubs jump through
/// `__la_symbol_ptr` into `__stub_helper`, which calls `dyld_stub_binder` on
/// first use — three more synthesised sections and a second opcode stream, all
/// to defer work that a short-lived process never saves. Binding eagerly is
/// simpler and correct; the lazy path is an optimisation to add once there is
/// something to measure it against.
fn stub_code(stub_address: u64, got_slot: u64) -> [u8; 12] {
    let adrp = blinker_relocations::encode::encode_adrp(
        0x9000_0000 | 16, // ADRP with Rd = x16
        page_distance(stub_address, got_slot),
    )
    .expect("a GOT slot is within ADRP range of its stub");

    // LDR (unsigned offset, 64-bit): the immediate is scaled by 8.
    let offset_in_page = got_slot & 0xfff;
    let ldr = 0xF940_0000 | ((offset_in_page / 8) as u32) << 10 | (16 << 5) | 16;

    let mut code = [0u8; 12];
    code[0..4].copy_from_slice(&adrp.to_le_bytes());
    code[4..8].copy_from_slice(&ldr.to_le_bytes());
    code[8..12].copy_from_slice(&BR_X16.to_le_bytes());
    code
}

/// Distance in 4 KiB pages from one address to another.
fn page_distance(from: u64, to: u64) -> i64 {
    let from_page = (from & !0xfff) as i64;
    let to_page = (to & !0xfff) as i64;
    (to_page - from_page) >> 12
}

/// Object id used for sections the linker synthesises rather than reads.
///
/// Layout keys contributions by `(object, section)`, so synthesised content
/// needs an id that cannot collide with a real input's.
const SYNTHETIC_OBJECT: ObjectId = ObjectId(u32::MAX);

/// Section id of the synthesised `__got`.
const GOT_SECTION: SectionId = SectionId(0);

/// Bytes per GOT entry: one 64-bit pointer.
const GOT_ENTRY_SIZE: u64 = 8;

/// Relocation kinds that reach their target through the GOT.
fn needs_got(kind: Arm64RelocationKind) -> bool {
    matches!(
        kind,
        Arm64RelocationKind::GotLoadPage21
            | Arm64RelocationKind::GotLoadPageOff12
            | Arm64RelocationKind::PointerToGot
    )
}

/// Symbols referenced but not defined by any object.
///
/// These become dynamic imports. They are checked against the SDK's stub for
/// `libSystem` rather than assumed: an unresolved symbol that libSystem does
/// not export is a typo or a missing input, and silently binding it would turn
/// a link error into a crash at first call.
fn undefined_references(objects: &[LoadedObject]) -> Vec<String> {
    let mut defined = std::collections::HashSet::new();
    for object in objects {
        for symbol in &object.parsed.symbols {
            if symbol.strength.is_definition() {
                defined.insert(symbol.name.clone());
            }
        }
    }

    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for object in objects {
        for symbol in &object.parsed.symbols {
            if symbol.strength.is_definition() || defined.contains(&symbol.name) {
                continue;
            }
            if seen.insert(symbol.name.clone()) {
                names.push(symbol.name.clone());
            }
        }
    }
    names.sort();
    names
}

/// Link the request into an image.
pub fn link(request: &LinkRequest) -> Result<Image, LinkError> {
    link_inner(request, &mut LinkTimings::default())
}

fn link_inner(request: &LinkRequest, timings: &mut LinkTimings) -> Result<Image, LinkError> {
    let overall = std::time::Instant::now();

    let step = std::time::Instant::now();
    let objects = load_objects(&request.objects)?;
    timings.read_and_parse_ms = elapsed_ms(step);
    let mut placements = placements_for(&objects);

    if placements.is_empty() {
        return Err(LinkError::NothingToLink);
    }

    // Imports are resolved before the symbol table is checked, not after: an
    // undefined reference is only an error once the dylibs have had their
    // chance at it. Checking first reported `_printf` as undefined in a
    // program that links against libSystem.
    let step = std::time::Instant::now();
    let imports = resolve_imports(&objects, request)?;

    // Resolution runs for its diagnostics: it is what turns a genuinely
    // missing definition into a named error rather than a relocation against
    // zero.
    resolve_symbols(&objects, &imports)?;
    timings.resolve_ms = elapsed_ms(step);

    let survey = survey_relocations(&objects, &imports);
    let stubs = survey.stubs;
    // Synthesise `__got` before layout, so it is placed and addressed like any
    // other section rather than appended afterwards. Internal targets and
    // imports share one table: the difference is only how each slot gets its
    // value — a rebase for an address we know, a bind for one dyld supplies.
    let mut got = survey.got;
    for entry in survey.personalities {
        if !got.iter().any(|e| e.name == entry.name) {
            got.push(entry);
        }
    }
    for name in &imports {
        if !got.iter().any(|e| &e.name == name) {
            got.push(TableEntry {
                object: SYNTHETIC_OBJECT,
                name: name.clone(),
            });
        }
    }
    // Personality routines named by CIE augmentation data — the only place they
    // appear in DWARF mode (finding 31), which is why collecting them from
    // `__compact_unwind` found none (finding 49).
    let mut eh_personality_fields: HashMap<(u32, u32), std::collections::HashSet<u64>> =
        HashMap::new();
    for object in &objects {
        for section in &object.parsed.sections {
            if section.name != "__eh_frame" {
                continue;
            }
            let fields = eh_frame_personality_fields(object, section);
            for relocation in &object.parsed.relocations {
                if relocation.section != section.id || !fields.contains(&relocation.offset) {
                    continue;
                }
                if let RelocationTarget::Symbol(id) = relocation.target {
                    if let Some(symbol) = object.parsed.symbol(id) {
                        if !got.iter().any(|e| e.name == symbol.name) {
                            got.push(TableEntry {
                                object: object.parsed.id,
                                name: symbol.name.clone(),
                            });
                        }
                    }
                }
            }
            if !fields.is_empty() {
                eh_personality_fields.insert((object.parsed.id.0, section.id.0), fields);
            }
        }
    }

    if !stubs.is_empty() {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: STUBS_SECTION,
            segment: "__TEXT".into(),
            name: "__stubs".into(),
            kind: SectionKind::Code,
            size: stubs.len() as u64 * STUB_SIZE,
            alignment: 4,
        });
    }
    // `__unwind_info` needs addresses to be built, but its *size* must be known
    // before layout runs. Sized from the record count, which is known now: one
    // entry per record, and the encoder's own size formula.
    let unwind_size = unwind_table_size(&objects);
    if unwind_size > 0 {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: UNWIND_SECTION,
            segment: "__TEXT".into(),
            name: "__unwind_info".into(),
            kind: SectionKind::Unwind,
            size: unwind_size,
            alignment: 4,
        });
    }

    let tlv = survey.tlv;
    if !tlv.is_empty() {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: TLV_SECTION,
            segment: "__DATA".into(),
            name: "__thread_ptrs".into(),
            kind: SectionKind::ThreadLocal,
            size: tlv.len() as u64 * GOT_ENTRY_SIZE,
            alignment: 8,
        });
    }
    if !got.is_empty() {
        placements.push(InputPlacement {
            object: SYNTHETIC_OBJECT,
            section: GOT_SECTION,
            segment: "__DATA_CONST".into(),
            name: "__got".into(),
            kind: SectionKind::Data,
            size: got.len() as u64 * GOT_ENTRY_SIZE,
            alignment: 8,
        });
    }

    // Pass one: learn where everything lands.
    let step = std::time::Instant::now();
    let probe = assemble(
        request,
        &Assembly {
            placements: &placements,
            ..Assembly::default()
        },
    )?;

    timings.layout_probe_ms = elapsed_ms(step);

    // With addresses known, copy content and patch it.
    let step = std::time::Instant::now();
    let addresses = address_map(&objects, &probe);
    let got_slots = got_slot_addresses(&got, &probe);
    let stub_slots = stub_addresses(&stubs, &probe);
    let tlv_slots = pointer_slot_addresses(&tlv, &probe, "__thread_ptrs");
    let mut contents = build_contents(&objects, &probe, &placements)?;
    fill_got(&mut contents, &probe, &got, &addresses, &imports)?;
    fill_stubs(&mut contents, &probe, &stubs, &got_slots)?;
    fill_pointer_table(&mut contents, &probe, &tlv, &addresses, "__thread_ptrs")?;
    fill_unwind_info(&mut contents, &probe, &objects, &addresses, &got_slots)?;
    // The addresses this link produced, in the form the cache compares. Built
    // before relocation because it is what decides which objects can skip it.
    let current_addresses = request
        .cache_path
        .as_ref()
        .map(|_| address_table(&addresses, &got_slots, &stub_slots, &tlv_slots));

    let previous = request.cache_path.as_deref().and_then(blinker_cache::load);
    let plan = match (&previous, &current_addresses) {
        (Some(previous), Some(current)) => Some(plan_reuse(&objects, &probe, previous, current)),
        _ => None,
    };
    timings.reused_objects = plan.as_ref().map_or(0, |p| p.entries.len() as u64);

    let patched = apply_relocations(
        &objects,
        &probe,
        &addresses,
        &IndirectTables {
            got: &got_slots,
            stubs: &stub_slots,
            tlv: &tlv_slots,
            imports: &imports,
            personalities: &eh_personality_fields,
        },
        contents,
        request.cache_path.is_some(),
        plan.as_ref(),
    )?;
    if let (Some(path), Some(addresses)) = (&request.cache_path, current_addresses) {
        let cache = build_cache(&objects, &probe, addresses, &patched.contents, &patched);
        // A cache that cannot be written is not an error: the link succeeded,
        // and the only consequence is that the next one is cold.
        let _ = blinker_cache::store(path, &cache);
    }

    let contents = patched.contents;
    let entry_offset = entry_offset(request, &objects, &probe)?;
    timings.relocate_ms = elapsed_ms(step);

    // Pass two: the same layout, with real bytes.
    //
    // The symbol table grows between passes, which changes `LC_SYMTAB`'s
    // contents but not the load commands' *sizes* — so the section addresses
    // the relocations were computed against still hold.
    let symbols = output_symbols(&objects, &probe)?;

    // Each GOT slot holds an absolute address, and the image is position
    // independent, so dyld must relocate every one of them at load time.
    // A slot whose value we know is rebased; a slot dyld fills is bound.
    let mut rebases = got_rebases(&probe, &got, &imports);
    let mut binds = got_binds(&probe, &got, &imports);
    binds.extend(patched.binds);
    rebases.extend(pointer_table_rebases(&probe, "__thread_ptrs", tlv.len()));
    rebases.extend(patched.rebases);

    let step = std::time::Instant::now();
    let image = assemble(
        request,
        &Assembly {
            placements: &placements,
            symbols: &symbols,
            contents: Some(&contents),
            rebases: &rebases,
            binds: &binds,
            entry_offset,
        },
    );
    timings.emit_ms = elapsed_ms(step);
    timings.total_ms = elapsed_ms(overall);
    image
}

/// Copy one object's cached bytes into the sections being assembled.
///
/// Returns whether every range landed. A range that does not fit is a cache
/// describing a layout this link did not produce, and the caller relocates the
/// object instead — bounds are checked rather than trusted because the file
/// came off disk and may have been written by anything.
fn copy_cached_bytes(
    entry: &blinker_cache::Entry,
    plan: &ReusePlan<'_>,
    contents: &mut SectionContents,
) -> bool {
    for range in &entry.ranges {
        let source = plan.sections.get(&range.section);
        // A zero-filled section — `__bss` and the thread-local block — has a
        // contribution with a real length and no bytes anywhere, in this link
        // or the cached one. There is nothing to copy and nothing wrong. This
        // is why a first version reused nothing at all on a Rust link: every
        // object that touched `__bss` failed the copy and fell back, which
        // looked exactly like a cache that never matched.
        if !contents.contains_key(&(range.section as usize)) {
            if source.is_none() {
                continue;
            }
            return false;
        }
        let (Some(source), Some(destination)) =
            (source, contents.get_mut(&(range.section as usize)))
        else {
            return false;
        };
        let (start, end) = (range.start as usize, (range.start + range.len) as usize);
        let (Some(from), Some(to)) = (source.get(start..end), destination.get_mut(start..end))
        else {
            return false;
        };
        to.copy_from_slice(from);
    }
    true
}

/// What a previous link left that this one can reuse.
///
/// Built once, before relocation, so the relocation loop's decision is a map
/// lookup rather than a re-derivation per object.
struct ReusePlan<'a> {
    /// Entries whose three conditions all hold, by object.
    entries: HashMap<u32, &'a blinker_cache::Entry>,
    /// The previous link's patched section bytes.
    sections: HashMap<u32, &'a [u8]>,
}

impl ReusePlan<'_> {
    fn entry(&self, object: ObjectId) -> Option<&blinker_cache::Entry> {
        self.entries.get(&object.0).copied()
    }
}

/// Decide, for every object, whether its bytes survive from the previous link.
///
/// Entries are matched to objects by **where their bytes went**, not by
/// position in the cache: adding or removing one input shifts every later
/// object's id, and an entry matched by index would then be checked against a
/// different object's content hash and pass. The first range is unique to an
/// object — two contributions cannot begin at the same offset of the same
/// section — so it identifies the entry without needing a name.
fn plan_reuse<'a>(
    objects: &[LoadedObject],
    image: &Image,
    previous: &'a blinker_cache::LinkCache,
    current_addresses: &[(blinker_cache::NameHash, u64)],
) -> ReusePlan<'a> {
    let changed: std::collections::HashSet<blinker_cache::NameHash> = blinker_cache::LinkCache {
        addresses: current_addresses.to_vec(),
        ..blinker_cache::LinkCache::default()
    }
    .changed_addresses(previous)
    .into_iter()
    .collect();

    let by_placement: HashMap<(u32, u64), &blinker_cache::Entry> = previous
        .entries
        .iter()
        .filter_map(|entry| entry.ranges.first().map(|r| ((r.section, r.start), entry)))
        .collect();

    // One probe per distinct file: an archive is proven unchanged once, not
    // once per member pulled out of it.
    let mut keys: HashMap<&Path, Option<blinker_cache::InputKey>> = HashMap::new();
    let mut entries = HashMap::new();
    for object in objects {
        let ranges = object_ranges(image, object.parsed.id);
        let Some(first) = ranges.first() else {
            continue;
        };
        let Some(entry) = by_placement.get(&(first.section, first.start)) else {
            continue;
        };
        let path = object.parsed.metadata.path.as_path();
        let Some(key) = keys
            .entry(path)
            .or_insert_with(|| blinker_cache::InputKey::probe(path))
            .clone()
        else {
            continue;
        };
        if entry.is_reusable(&key, &ranges, &changed) {
            entries.insert(object.parsed.id.0, *entry);
        }
    }

    ReusePlan {
        entries,
        sections: previous
            .sections
            .iter()
            .map(|(index, bytes)| (*index, bytes.as_slice()))
            .collect(),
    }
}

/// Assemble the record a later link can reuse.
///
/// Built after relocation, from what that pass already produced: the patched
/// bytes, each object's fixups, and the addresses every object read. Nothing
/// here is computed for the cache's sake alone except the input keys and the
/// hashing of names, which is why writing a cache costs a fraction of using
/// one.
fn build_cache(
    objects: &[LoadedObject],
    image: &Image,
    addresses: Vec<(blinker_cache::NameHash, u64)>,
    contents: &SectionContents,
    patched: &Patched,
) -> blinker_cache::LinkCache {
    // Input keys, one probe per distinct file. Archive members share their
    // archive's path and therefore its key: an rlib is proven unchanged once,
    // not once per member pulled out of it.
    let mut keys: HashMap<&Path, Option<blinker_cache::InputKey>> = HashMap::new();

    let entries = patched
        .records
        .iter()
        .filter_map(|record| {
            let object = objects.iter().find(|o| o.parsed.id == record.object)?;
            let path = object.parsed.metadata.path.as_path();
            let key = keys
                .entry(path)
                .or_insert_with(|| blinker_cache::InputKey::probe(path))
                .clone()?;
            Some(blinker_cache::Entry {
                key,
                ranges: object_ranges(image, record.object),
                deps: record.deps.clone(),
                binds: patched.binds[record.binds.clone()]
                    .iter()
                    .map(|bind| blinker_cache::CachedBind {
                        segment: bind.segment,
                        offset: bind.offset,
                        symbol: bind.symbol.clone(),
                        library_ordinal: bind.library_ordinal,
                        addend: bind.addend,
                    })
                    .collect(),
                rebases: patched.rebases[record.rebases.clone()]
                    .iter()
                    .map(|rebase| blinker_cache::CachedRebase {
                        segment: rebase.segment,
                        offset: rebase.offset,
                    })
                    .collect(),
            })
        })
        .collect();

    let mut sections: Vec<_> = contents
        .iter()
        .map(|(index, bytes)| (*index as u32, bytes.clone()))
        .collect();
    sections.sort_unstable_by_key(|(index, _)| *index);

    blinker_cache::LinkCache {
        entries,
        addresses,
        sections,
    }
}

/// Where one object's bytes sit in the output, in cache terms.
fn object_ranges(image: &Image, object: ObjectId) -> Vec<blinker_cache::Range> {
    let mut ranges: Vec<_> = image
        .layout
        .sections
        .iter()
        .enumerate()
        .flat_map(|(index, section)| {
            section
                .contributions
                .iter()
                .filter(move |c| c.object == object)
                .map(move |c| blinker_cache::Range {
                    section: index as u32,
                    start: c.offset,
                    len: c.size,
                })
        })
        .collect();
    // Sorted so that comparing two links compares placement, not the order
    // the layout happened to visit sections in.
    ranges.sort_unstable_by_key(|r| (r.section, r.start));
    ranges
}

/// Every address a relocation could have read, in one sorted table.
///
/// The indirect tables are included alongside the symbols because they move
/// independently of them: inserting a GOT entry shifts every slot after it
/// while leaving every symbol address untouched, and an entry whose bytes
/// reference the shifted slot must not look unchanged.
fn address_table(
    addresses: &AddressMap,
    got_slots: &HashMap<String, u64>,
    stub_slots: &HashMap<String, u64>,
    tlv_slots: &HashMap<String, u64>,
) -> Vec<(blinker_cache::NameHash, u64)> {
    use blinker_cache::{dep_hash, Table, GLOBAL};
    let mut table: Vec<_> =
        addresses
            .global
            .iter()
            .map(|(name, address)| (dep_hash(GLOBAL, Table::Symbol, name), *address))
            .chain(addresses.local.iter().map(|((object, name), address)| {
                (dep_hash(*object, Table::Symbol, name), *address)
            }))
            .chain(indirect_entries(got_slots, Table::Got))
            .chain(indirect_entries(stub_slots, Table::Stub))
            .chain(indirect_entries(tlv_slots, Table::ThreadLocal))
            .collect();
    table.sort_unstable();
    table.dedup();
    table
}

fn indirect_entries(
    slots: &HashMap<String, u64>,
    table: blinker_cache::Table,
) -> impl Iterator<Item = (blinker_cache::NameHash, u64)> + '_ {
    slots.iter().map(move |(name, address)| {
        (
            blinker_cache::dep_hash(blinker_cache::GLOBAL, table, name),
            *address,
        )
    })
}

fn elapsed_ms(start: std::time::Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Undefined references, checked against what `libSystem` actually exports.
fn resolve_imports(
    objects: &[LoadedObject],
    request: &LinkRequest,
) -> Result<Vec<String>, LinkError> {
    let undefined = undefined_references(objects);
    if undefined.is_empty() {
        return Ok(Vec::new());
    }

    let Some(available) = request.dynamic_symbols() else {
        // No stub library was supplied, so nothing can be imported and every
        // undefined reference is an error.
        return Err(LinkError::UndefinedSymbols { names: undefined });
    };

    let mut imports = Vec::new();
    let mut missing = Vec::new();
    for name in undefined {
        if available.contains(&name) {
            imports.push(name);
        } else {
            missing.push(name);
        }
    }
    if !missing.is_empty() {
        return Err(LinkError::UndefinedSymbols { names: missing });
    }
    Ok(imports)
}

/// Size the `__unwind_info` table will come to.
///
/// Computed from the record count rather than by building the table, because
/// layout needs the size before any address exists. Building it twice would
/// need addresses that do not exist yet.
fn unwind_table_size(objects: &[LoadedObject]) -> u64 {
    let records: usize = objects
        .iter()
        .flat_map(|o| o.parsed.sections.iter())
        .filter(|s| s.name == "__compact_unwind")
        .map(|s| (s.size / COMPACT_UNWIND_RECORD) as usize)
        .sum();
    if records == 0 {
        return 0;
    }
    // Deliberately generous: the real table is smaller once duplicate function
    // offsets collapse and only some entries carry an LSDA. Over-reserving
    // wastes a few kilobytes; under-reserving is a link failure.
    blinker_output::unwind::upper_bound_size(records) as u64
}

/// Build the unwind table and write it into its section.
fn fill_unwind_info(
    contents: &mut SectionContents,
    image: &Image,
    objects: &[LoadedObject],
    addresses: &AddressMap,
    got_slots: &HashMap<String, u64>,
) -> Result<(), LinkError> {
    let Some((index, section)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == "__unwind_info")
    else {
        return Ok(());
    };

    let fde_offsets = eh_frame_fde_offsets(objects, image, addresses);
    let entries = compact_unwind_entries(objects, image, addresses, got_slots, &fde_offsets);
    let mut table = blinker_output::unwind::build(entries);

    if table.len() as u64 > section.size {
        return Err(LinkError::UnwindTableTooLarge {
            reserved: section.size,
            needed: table.len(),
        });
    }
    // The reservation is an upper bound, so the tail is padding.
    table.resize(section.size as usize, 0);
    contents.insert(index, table);
    Ok(())
}

/// Address of each stub.
fn stub_addresses(stubs: &[String], image: &Image) -> HashMap<String, u64> {
    let mut slots = HashMap::new();
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__stubs") else {
        return slots;
    };
    for (index, name) in stubs.iter().enumerate() {
        slots.insert(name.clone(), section.vm_address + index as u64 * STUB_SIZE);
    }
    slots
}

/// Write each stub's three instructions.
fn fill_stubs(
    contents: &mut HashMap<usize, Vec<u8>>,
    image: &Image,
    stubs: &[String],
    got_slots: &HashMap<String, u64>,
) -> Result<(), LinkError> {
    if stubs.is_empty() {
        return Ok(());
    }
    let Some((index, section)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == "__stubs")
    else {
        return Ok(());
    };
    let buffer = contents
        .entry(index)
        .or_insert_with(|| vec![0u8; stubs.len() * STUB_SIZE as usize]);

    for (slot, name) in stubs.iter().enumerate() {
        let stub_address = section.vm_address + slot as u64 * STUB_SIZE;
        let got = *got_slots
            .get(name)
            .ok_or_else(|| LinkError::UndefinedSymbols {
                names: vec![name.clone()],
            })?;
        let start = slot * STUB_SIZE as usize;
        buffer[start..start + STUB_SIZE as usize].copy_from_slice(&stub_code(stub_address, got));
    }
    Ok(())
}

/// Bind entries: one per GOT slot dyld has to fill.
fn got_binds(image: &Image, got: &[TableEntry], imports: &[String]) -> Vec<Bind> {
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__got") else {
        return Vec::new();
    };
    let Some((segment_index, segment)) = image
        .layout
        .segments
        .iter()
        .enumerate()
        .find(|(_, seg)| seg.name == section.segment)
    else {
        return Vec::new();
    };
    let base = section.vm_address - segment.vm_address;
    got.iter()
        .enumerate()
        .filter(|(_, entry)| imports.contains(&entry.name))
        .map(|(slot, entry)| Bind {
            segment: segment_index as u8,
            offset: base + slot as u64 * GOT_ENTRY_SIZE,
            symbol: entry.name.clone(),
            // One-based; the only library in the list is libSystem.
            library_ordinal: 1,
            addend: 0,
        })
        .collect()
}

/// Address of each GOT slot, in the order the symbols were collected.
fn got_slot_addresses(got: &[TableEntry], image: &Image) -> HashMap<String, u64> {
    pointer_slot_addresses(got, image, "__got")
}

/// Address of each slot in a synthesised pointer table.
fn pointer_slot_addresses(
    names: &[TableEntry],
    image: &Image,
    section_name: &str,
) -> HashMap<String, u64> {
    let mut slots = HashMap::new();
    let Some(section) = image
        .layout
        .sections
        .iter()
        .find(|s| s.name == section_name)
    else {
        return slots;
    };
    for (index, entry) in names.iter().enumerate() {
        slots.insert(
            entry.name.clone(),
            section.vm_address + index as u64 * GOT_ENTRY_SIZE,
        );
    }
    slots
}

/// Fill a synthesised pointer table with the addresses its slots point at.
///
/// A slot whose target is not defined in this image is left zero: dyld fills
/// it, and writing a wrong value would be worse than writing none.
fn fill_pointer_table(
    contents: &mut HashMap<usize, Vec<u8>>,
    image: &Image,
    names: &[TableEntry],
    addresses: &AddressMap,
    section_name: &str,
) -> Result<(), LinkError> {
    if names.is_empty() {
        return Ok(());
    }
    let Some((index, _)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == section_name)
    else {
        return Ok(());
    };
    let buffer = contents
        .entry(index)
        .or_insert_with(|| vec![0u8; names.len() * GOT_ENTRY_SIZE as usize]);

    for (slot, entry) in names.iter().enumerate() {
        // Looked up against the object that *referenced* it, so a local
        // definition is visible.
        let Some(address) = addresses.lookup(entry.object, &entry.name) else {
            continue;
        };
        let start = slot * GOT_ENTRY_SIZE as usize;
        buffer[start..start + 8].copy_from_slice(&address.to_le_bytes());
    }
    Ok(())
}

/// Write each GOT slot's initial value: the address of the symbol it points at.
fn fill_got(
    contents: &mut HashMap<usize, Vec<u8>>,
    image: &Image,
    got: &[TableEntry],
    addresses: &AddressMap,
    imports: &[String],
) -> Result<(), LinkError> {
    if got.is_empty() {
        return Ok(());
    }
    let Some((index, _)) = image
        .layout
        .sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.name == "__got")
    else {
        return Ok(());
    };
    let buffer = contents
        .entry(index)
        .or_insert_with(|| vec![0u8; got.len() * 8]);

    for (slot, entry) in got.iter().enumerate() {
        let name = &entry.name;
        if imports.contains(name) {
            // dyld writes this slot at load time; it starts as zero.
            continue;
        }
        let address = addresses.lookup(SYNTHETIC_OBJECT, name).ok_or_else(|| {
            LinkError::UndefinedSymbols {
                names: vec![name.clone()],
            }
        })?;
        let start = slot * GOT_ENTRY_SIZE as usize;
        buffer[start..start + 8].copy_from_slice(&address.to_le_bytes());
    }
    Ok(())
}

/// One rebase entry per slot of a synthesised pointer table.
///
/// The GOT is not the only such table: `__thread_ptrs` holds absolute
/// addresses of thread-local descriptors and needs sliding just the same.
/// Missing these produced a `SIGSEGV` in `lang_start_internal`, the first code
/// to walk a thread-local pointer, with a fault address in the *unslid*
/// address space — the signature of a pointer dyld was never told about.
fn pointer_table_rebases(image: &Image, section_name: &str, count: usize) -> Vec<Rebase> {
    let Some(section) = image
        .layout
        .sections
        .iter()
        .find(|s| s.name == section_name)
    else {
        return Vec::new();
    };
    let Some((segment_index, segment)) = image
        .layout
        .segments
        .iter()
        .enumerate()
        .find(|(_, seg)| seg.name == section.segment)
    else {
        return Vec::new();
    };
    let base = section.vm_address - segment.vm_address;
    (0..count as u64)
        .map(|slot| Rebase {
            segment: segment_index as u8,
            offset: base + slot * GOT_ENTRY_SIZE,
        })
        .collect()
}

/// One rebase entry per GOT slot.
fn got_rebases(image: &Image, got: &[TableEntry], imports: &[String]) -> Vec<Rebase> {
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__got") else {
        return Vec::new();
    };
    let Some((segment_index, segment)) = image
        .layout
        .segments
        .iter()
        .enumerate()
        .find(|(_, seg)| seg.name == section.segment)
    else {
        return Vec::new();
    };
    let base = section.vm_address - segment.vm_address;
    got.iter()
        .enumerate()
        // An imported slot is bound, not rebased: rebasing it would add the
        // load bias to a value dyld is about to overwrite.
        .filter(|(_, entry)| !imports.contains(&entry.name))
        .map(|(slot, _)| Rebase {
            segment: segment_index as u8,
            offset: base + slot as u64 * GOT_ENTRY_SIZE,
        })
        .collect()
}

/// Whether a path is an archive rather than a single object.
///
/// By extension, not by content: `lib.rmeta` inside an `.rlib` is a *genuine*
/// Mach-O object holding crate metadata (finding 9), so sniffing magic numbers
/// misclassifies in the other direction too. The toolchain names these files
/// consistently, and the name is the reliable signal.
fn is_archive(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("a") | Some("rlib")
    )
}

/// Load the inputs, extracting from archives only what the link needs.
///
/// # Why archives are not simply expanded
///
/// `libstd.rlib` holds hundreds of objects. Linking all of them would work and
/// produce a binary tens of megabytes larger than it should be, full of code
/// nothing calls. The rule every linker follows instead: a member is pulled in
/// only when it defines a symbol something already in the link needs — and
/// pulling it in can create new undefined symbols, so the process repeats to a
/// fixed point.
///
/// Order matters within a pass and between passes, which is why this is a loop
/// rather than a single sweep.
fn load_objects(paths: &[PathBuf]) -> Result<Vec<LoadedObject>, LinkError> {
    let mut objects: Vec<LoadedObject> = Vec::new();
    let mut next_id = 0u32;

    // Archives are indexed but not extracted; objects go in directly.
    let mut archives: Vec<(PathBuf, blinker_archive::ArchiveIndex, Vec<u8>)> = Vec::new();

    for path in paths {
        let data = std::fs::read(path).map_err(|source| LinkError::Read {
            path: path.clone(),
            source,
        })?;

        if is_archive(path) {
            let index = blinker_archive::index_archive(&data, path)
                .map_err(|source| LinkError::Archive(Box::new(source)))?;
            archives.push((path.clone(), index, data));
            continue;
        }

        let parsed = parse_object(&data, path, None, ObjectId(next_id))
            .map_err(|source| LinkError::Parse(Box::new(source)))?;
        next_id += 1;
        objects.push(LoadedObject { parsed, data });
    }

    if archives.is_empty() {
        return Ok(objects);
    }

    // Pull members in until nothing new is needed.
    let mut extracted: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    loop {
        let wanted = undefined_references(&objects);
        let mut added = false;

        for name in &wanted {
            for (archive_index, (path, index, data)) in archives.iter().enumerate() {
                let Some(member_id) = index.member_defining(name) else {
                    continue;
                };
                if !extracted.insert((archive_index, member_id.0)) {
                    continue; // already in the link
                }
                let Some(member) = index.member(member_id) else {
                    continue;
                };
                let bytes = blinker_archive::member_data(data, member, path)
                    .map_err(|source| LinkError::Archive(Box::new(source)))?;
                let parsed = parse_object(bytes, path, Some(&member.name), ObjectId(next_id))
                    .map_err(|source| LinkError::Parse(Box::new(source)))?;
                next_id += 1;
                objects.push(LoadedObject {
                    parsed,
                    data: bytes.to_vec(),
                });
                added = true;
                break;
            }
        }

        if !added {
            return Ok(objects);
        }
    }
}

/// Build the global symbol table and check it is complete.
fn resolve_symbols(objects: &[LoadedObject], imports: &[String]) -> Result<SymbolTable, LinkError> {
    let mut table = SymbolTable::new();

    // Dylib exports are definitions as far as resolution is concerned; dyld
    // supplies the address at load time.
    for name in imports {
        table.define_dynamic(name, 0);
    }

    for object in objects {
        for symbol in &object.parsed.symbols {
            if symbol.strength.is_definition() {
                table.define(
                    &symbol.name,
                    SymbolProvider::Object {
                        object: object.parsed.id,
                        symbol: symbol.id,
                    },
                    symbol.strength,
                    symbol.visibility,
                );
            } else {
                table.reference(&symbol.name, object.parsed.id, symbol.strength);
            }
        }
    }

    let undefined = table.undefined_symbols();
    if !undefined.is_empty() {
        return Err(LinkError::UndefinedSymbols {
            names: undefined
                .into_iter()
                .filter_map(|u| table.name_of(u.name).map(str::to_string))
                .collect(),
        });
    }
    Ok(table)
}

/// Every input section that belongs in the output, in object order.
fn placements_for(objects: &[LoadedObject]) -> Vec<InputPlacement> {
    let mut placements = Vec::new();
    for object in objects {
        for section in &object.parsed.sections {
            if is_linker_internal(section) {
                continue;
            }
            placements.push(InputPlacement {
                object: object.parsed.id,
                section: section.id,
                segment: section.segment.clone(),
                name: section.name.clone(),
                kind: section.kind,
                size: section.size,
                alignment: section.alignment,
            });
        }
    }
    placements
}

/// Assemble an image from the current knowledge.
///
/// `contents` is keyed by output-section index; an empty map produces an image
/// whose sections are zero-filled, which is what the first pass wants.
/// Everything the emitter needs, gathered so the parameter list stays a list
/// of *decisions* rather than of accumulated arguments.
#[derive(Default)]
struct Assembly<'a> {
    placements: &'a [InputPlacement],
    symbols: &'a [OutputSymbol],
    contents: Option<&'a HashMap<usize, Vec<u8>>>,
    rebases: &'a [Rebase],
    binds: &'a [Bind],
    entry_offset: u64,
}

fn assemble(request: &LinkRequest, assembly: &Assembly<'_>) -> Result<Image, LinkError> {
    let Assembly {
        placements,
        symbols: output_symbols,
        contents,
        rebases,
        binds,
        entry_offset,
    } = *assembly;
    let mut builder = ImageBuilder::new();
    for placement in placements {
        builder.input(placement.clone());
    }
    for dylib in &request.dylibs {
        builder.dylib(dylib.clone());
    }
    builder.identifier(&request.identifier);
    builder.entry_offset(entry_offset);

    // Sections with no supplied content are emitted as zeroes of the right
    // size, so the first pass produces a valid image to read the layout from.
    if let Some(contents) = contents {
        for index in 0..placements.len() {
            if let Some(bytes) = contents.get(&index) {
                builder.content(index, bytes.clone());
            }
        }
    }

    for symbol in output_symbols {
        builder.symbols().add(symbol.clone());
    }
    for rebase in rebases {
        builder.rebase(*rebase);
    }
    for bind in binds {
        builder.bind(bind.clone());
    }
    builder.build().map_err(LinkError::Emit)
}

/// The output symbol table: every non-local definition, at the address layout
/// gave it.
///
/// Locals are dropped rather than emitted with a wrong address. They are
/// invisible outside their object by definition, and the only consumer that
/// would want them is a debugger, which needs the `N_OSO`/stabs path that is
/// not implemented.
fn output_symbols(objects: &[LoadedObject], image: &Image) -> Result<Vec<OutputSymbol>, LinkError> {
    let mut out = Vec::new();
    for object in objects {
        for symbol in &object.parsed.symbols {
            if !symbol.strength.is_definition() || symbol.visibility == SymbolVisibility::Local {
                continue;
            }
            let Some(section_id) = symbol.section else {
                continue;
            };
            let Some(input) = object.parsed.section(section_id) else {
                continue;
            };
            let offset_in_section = symbol.value.saturating_sub(input.vm_address);
            let Some(chunk) = image
                .layout
                .sections
                .iter()
                .find_map(|s| s.address_of(object.parsed.id, section_id))
            else {
                continue;
            };
            out.push(OutputSymbol::exported(
                &symbol.name,
                1,
                chunk + offset_in_section,
            ));
        }
    }
    // Deterministic order regardless of how the objects were traversed.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Copy each input section's bytes into its output section's buffer.
fn build_contents(
    objects: &[LoadedObject],
    image: &Image,
    _placements: &[InputPlacement],
) -> Result<HashMap<usize, Vec<u8>>, LinkError> {
    let mut contents: HashMap<usize, Vec<u8>> = HashMap::new();

    for (index, section) in image.layout.sections.iter().enumerate() {
        if section.is_zero_filled() {
            continue;
        }
        let mut buffer = vec![0u8; section.size as usize];

        for contribution in &section.contributions {
            // Synthesised content (the GOT) has no input object to copy from;
            // it is filled in separately once addresses are known.
            if contribution.object == SYNTHETIC_OBJECT {
                continue;
            }
            let object = objects
                .iter()
                .find(|o| o.parsed.id == contribution.object)
                .ok_or(LinkError::MissingObject {
                    object: contribution.object,
                })?;
            let input =
                object
                    .parsed
                    .section(contribution.section)
                    .ok_or(LinkError::MissingSection {
                        object: contribution.object,
                        section: contribution.section,
                    })?;

            // A section with no file bytes (zero-filled in the input) leaves
            // its span in the buffer as the zeroes it already holds.
            let Some(file_offset) = input.file_offset else {
                continue;
            };
            let start = file_offset as usize;
            let end = start + input.size as usize;
            let bytes = object
                .data
                .get(start..end)
                .ok_or(LinkError::SectionOutOfBounds {
                    object: contribution.object,
                    section: contribution.section,
                })?;

            let target = contribution.offset as usize;
            buffer[target..target + bytes.len()].copy_from_slice(bytes);
        }

        contents.insert(index, buffer);
    }

    Ok(contents)
}

/// Patch every relocation against the addresses layout assigned.
/// Start of the per-thread block: the address `__thread_data` was placed at.
///
/// A TLV descriptor's third word is the variable's **offset within this
/// block**, not its address. dyld copies the block per thread, so an absolute
/// address there is meaningless — it rejects the image with "malformed
/// thread-local, offset=… is larger than total size".
fn thread_local_base(image: &Image) -> Option<u64> {
    image
        .layout
        .sections
        .iter()
        .filter(|s| s.name == "__thread_data" || s.name == "__thread_bss")
        .map(|s| s.vm_address)
        .min()
}

/// Whether an address falls inside the per-thread block.
fn in_thread_local_block(image: &Image, address: u64) -> bool {
    image
        .layout
        .sections
        .iter()
        .filter(|s| s.name == "__thread_data" || s.name == "__thread_bss")
        .any(|s| address >= s.vm_address && address < s.vm_address + s.size)
}

/// Section content keyed by output-section index.
type SectionContents = HashMap<usize, Vec<u8>>;

/// The indirection tables a relocation may need to reach its target.
///
/// Grouped because they travel together and are consulted by the same rules:
/// which one applies is decided by the relocation's kind, not by the caller.
struct IndirectTables<'a> {
    got: &'a HashMap<String, u64>,
    stubs: &'a HashMap<String, u64>,
    tlv: &'a HashMap<String, u64>,
    imports: &'a [String],
    /// Offsets, per `(object, section)`, of CIE personality fields that use an
    /// indirect encoding.
    ///
    /// A CIE's augmentation encodes its personality with `DW_EH_PE_indirect`:
    /// the stored value addresses a *slot* holding the routine's address, not
    /// the routine. Resolving it like any other symbol reference wrote a
    /// function address where libunwind expected a pointer slot, and it
    /// segfaulted dereferencing it (finding 48).
    personalities: &'a HashMap<(u32, u32), std::collections::HashSet<u64>>,
}

/// Patched content, plus the fixups dyld must apply at load time.
struct Patched {
    contents: SectionContents,
    binds: Vec<Bind>,
    /// Absolute pointers written into data, which dyld must slide.
    ///
    /// **Every** such pointer needs an entry, not just the GOT's. A
    /// position-independent image is loaded at a random offset, so an absolute
    /// address baked in at link time is stale the moment it loads. C programs
    /// hid this for a long time: their globals are reached PC-relatively, so
    /// they contain almost no absolute pointers. Rust's vtables, statics and
    /// panic metadata are full of them, and the result was a `SIGSEGV` inside
    /// `std::rt::lang_start_internal` — the first code to walk one.
    rebases: Vec<Rebase>,
    /// What each object read and produced, for the cache.
    records: Vec<ObjectRecord>,
}

/// One object's trace through the relocation pass.
///
/// The fixups are ranges into `Patched`'s flat vectors rather than copies: the
/// link needs them flat to encode, and the cache needs them attributed, and
/// slicing serves both without duplicating either.
struct ObjectRecord {
    object: ObjectId,
    deps: Vec<blinker_cache::NameHash>,
    binds: std::ops::Range<usize>,
    rebases: std::ops::Range<usize>,
}

/// Note that a relocation reads an address, without resolving it.
///
/// Both the symbol and *which table it is read from* are recorded: a symbol,
/// its GOT slot, its stub and its thread-local slot are four addresses that
/// move independently, and a GOT entry inserted ahead of this one shifts the
/// slot while leaving the symbol exactly where it was.
fn note_reference(
    referenced: &mut HashSet<(u32, u8)>,
    relocation: &blinker_macho::InputRelocation,
) {
    let RelocationTarget::Symbol(id) = relocation.target else {
        // A section target resolves within this object, so its address is
        // already covered by the entry's own ranges.
        return;
    };
    // The symbol's own address is read whenever the indirect one is not, and
    // which of the two applies depends on whether the symbol turned out to be
    // imported — known here only for some kinds. Recording both is correct and
    // costs one extra hash.
    referenced.insert((id.0, blinker_cache::Table::Symbol as u8));
    let indirect = if needs_got(relocation.kind) {
        blinker_cache::Table::Got
    } else if needs_tlv(relocation.kind) {
        blinker_cache::Table::ThreadLocal
    } else if relocation.kind == Arm64RelocationKind::Branch26 {
        blinker_cache::Table::Stub
    } else {
        return;
    };
    referenced.insert((id.0, indirect as u8));
}

/// Turn noted references into the hashes the cache compares.
///
/// The scope must mirror `AddressMap::lookup` exactly: a name this object
/// defines locally is a different address from a global of the same name, and
/// resolving one against the other is the bug finding 57 traced through the
/// `__eh_frame` LSDAs.
fn dependency_hashes(
    object: &LoadedObject,
    addresses: &AddressMap,
    referenced: &HashSet<(u32, u8)>,
) -> Vec<blinker_cache::NameHash> {
    let mut hashes: Vec<_> = referenced
        .iter()
        .filter_map(|(symbol, table)| {
            let name = &object.parsed.symbol(SymbolId(*symbol))?.name;
            let table = match table {
                1 => blinker_cache::Table::Got,
                2 => blinker_cache::Table::Stub,
                3 => blinker_cache::Table::ThreadLocal,
                _ => blinker_cache::Table::Symbol,
            };
            Some(blinker_cache::dep_hash(
                addresses.scope_of(object.parsed.id, name),
                table,
                name,
            ))
        })
        .collect();
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

fn apply_relocations(
    objects: &[LoadedObject],
    image: &Image,
    addresses: &AddressMap,
    tables: &IndirectTables<'_>,
    mut contents: SectionContents,
    // Whether to trace what each object read and produced.
    //
    // Off by default because it is not free: noting references and hashing
    // their names costs 1.9 ms on a 27.6 ms link — worth paying to *write* a
    // cache, and pure waste on a link that will not.
    record: bool,
    // Objects whose patched bytes a previous link already produced.
    reuse: Option<&ReusePlan<'_>>,
) -> Result<Patched, LinkError> {
    let mut extra_rebases = Vec::new();
    let IndirectTables {
        got: got_slots,
        stubs: stub_slots,
        tlv: tlv_slots,
        imports,
        personalities,
    } = *tables;

    // A reference from `__eh_frame` to a personality routine must name that
    // routine's GOT slot.
    // A relocation whose field is a CIE's indirect personality reference must
    // resolve to that symbol's GOT slot. Identified by *offset* — the CIE's
    // augmentation is the only thing that says which field this is, and it was
    // parsed before layout.
    let indirect_personality =
        |object: &LoadedObject, relocation: &blinker_macho::InputRelocation| {
            let fields = personalities.get(&(object.parsed.id.0, relocation.section.0))?;
            if !fields.contains(&relocation.offset) {
                return None;
            }
            let RelocationTarget::Symbol(id) = relocation.target else {
                return None;
            };
            let symbol = object.parsed.symbol(id)?;
            got_slots.get(&symbol.name).copied()
        };
    let mut extra_binds = Vec::new();
    let mut records: Vec<ObjectRecord> = Vec::new();
    for object in objects {
        // Where this object's fixups start. Binds and rebases are produced as
        // a side effect of relocating, so an object whose bytes are later
        // reused from the cache must carry its own away with it — and the
        // cheapest way to attribute them is to remember where its run began
        // rather than to thread a second collection through every push site.
        let bind_start = extra_binds.len();
        let rebase_start = extra_rebases.len();
        // Addresses this object read, deduplicated by (symbol, table) so the
        // hashing below is proportional to distinct references rather than to
        // relocations — an object typically has several times more of the
        // latter.
        let mut referenced: HashSet<(u32, u8)> = HashSet::new();

        // The whole point of the cache: this object's bytes were relocated by
        // a previous link, nothing it reads has moved, and it has not moved
        // itself — so copy them and skip every relocation it holds.
        if let Some(entry) = reuse.and_then(|plan| plan.entry(object.parsed.id)) {
            let plan = reuse.expect("just matched");
            if copy_cached_bytes(entry, plan, &mut contents) {
                extra_binds.extend(entry.binds.iter().map(|bind| Bind {
                    segment: bind.segment,
                    offset: bind.offset,
                    symbol: bind.symbol.clone(),
                    library_ordinal: bind.library_ordinal,
                    addend: bind.addend,
                }));
                extra_rebases.extend(entry.rebases.iter().map(|rebase| Rebase {
                    segment: rebase.segment,
                    offset: rebase.offset,
                }));
                if record {
                    records.push(ObjectRecord {
                        object: object.parsed.id,
                        deps: entry.deps.clone(),
                        binds: bind_start..extra_binds.len(),
                        rebases: rebase_start..extra_rebases.len(),
                    });
                }
                continue;
            }
            // The cached bytes did not fit where they claimed to. Nothing is
            // wrong with the link, only with the cache, so fall through and
            // relocate this object as though there had been no entry at all.
        }

        // Indexed rather than iterated: `SUBTRACTOR` is one half of a pair and
        // needs the relocation that follows it, so the loop has to be able to
        // consume two entries at once.
        let relocations = &object.parsed.relocations;
        let mut index = 0;
        while index < relocations.len() {
            let relocation = &relocations[index];
            index += 1;

            // Where the patched field lives in the output.
            let Some((section_index, output_section)) = image
                .layout
                .sections
                .iter()
                .enumerate()
                .find(|(_, s)| s.address_of(object.parsed.id, relocation.section).is_some())
            else {
                // The relocation patches a section that was dropped as
                // linker-internal; nothing in the output refers to it.
                continue;
            };

            // Recorded before any branch, so no relocation kind can be added
            // later that reads an address without declaring it. Over-recording
            // only costs an unnecessary rebuild; under-recording reuses bytes
            // that are wrong.
            if record {
                note_reference(&mut referenced, relocation);
            }

            let chunk_address = output_section
                .address_of(object.parsed.id, relocation.section)
                .expect("just matched");
            let chunk_offset = chunk_address - output_section.vm_address;
            let place = chunk_address + relocation.offset;

            // `SUBTRACTOR` computes a *difference* between two addresses, so
            // it is meaningless alone: the pair is emitted as SUBTRACTOR (the
            // value being subtracted) immediately followed by UNSIGNED (the
            // value being subtracted from). Relative pointers in unwind and
            // exception tables are built this way, which is why Rust hits it
            // and simple C does not.
            if relocation.kind == Arm64RelocationKind::Subtractor {
                let Some(pair) = relocations.get(index) else {
                    return Err(LinkError::UnpairedSubtractor {
                        object: object.parsed.id,
                        offset: relocation.offset,
                    });
                };
                index += 1;

                if record {
                    note_reference(&mut referenced, pair);
                }
                let subtrahend = target_address(object, image, addresses, relocation.target)?;
                let minuend = match indirect_personality(object, pair) {
                    Some(slot) => slot,
                    None => target_address(object, image, addresses, pair.target)?,
                };

                // Mach-O relocations carry their addend **in the bytes being
                // patched**, not in the relocation entry — `addend` is zero on
                // every one of them. For a pair that difference is not a small
                // correction: the subtrahend is the section's own anchor label
                // (`ltmpN`), so `minuend - subtrahend` is measured from the
                // start of the contribution, while the field wants it measured
                // from the field. The inline value is exactly that gap.
                let addend = pair.addend + inline_addend(object, pair);

                let Some(buffer) = contents.get_mut(&section_index) else {
                    continue;
                };
                blinker_relocations::apply_pair(
                    pair.length,
                    chunk_offset + pair.offset,
                    subtrahend,
                    minuend,
                    addend,
                    place,
                    buffer,
                )
                .map_err(|source| LinkError::Relocation {
                    object: object.parsed.id,
                    kind: relocation.kind,
                    source: Box::new(source),
                })?;
                continue;
            }

            // GOT-based kinds are patched with the address of the *slot*, not
            // of the symbol; the symbol's address is what the slot contains.
            let got = if needs_got(relocation.kind) {
                match relocation.target {
                    RelocationTarget::Symbol(id) => object
                        .parsed
                        .symbol(id)
                        .and_then(|s| got_slots.get(&s.name))
                        .copied(),
                    RelocationTarget::Section(_) => None,
                }
            } else {
                None
            };

            let tlv = if needs_tlv(relocation.kind) {
                match relocation.target {
                    RelocationTarget::Symbol(id) => object
                        .parsed
                        .symbol(id)
                        .and_then(|s| tlv_slots.get(&s.name))
                        .copied(),
                    RelocationTarget::Section(_) => None,
                }
            } else {
                None
            };

            // A branch to an imported function goes to its stub: dyld has not
            // filled anything in yet, and a BRANCH26 cannot reach an address
            // that does not exist until load time.
            let stub = if relocation.kind == Arm64RelocationKind::Branch26 {
                match relocation.target {
                    RelocationTarget::Symbol(id) => object
                        .parsed
                        .symbol(id)
                        .and_then(|s| stub_slots.get(&s.name))
                        .copied(),
                    RelocationTarget::Section(_) => None,
                }
            } else {
                None
            };

            // A pointer-sized data reference to an imported symbol cannot be
            // patched at all: the address does not exist until dyld supplies
            // it. The field stays zero and a bind entry tells dyld where to
            // write. TLV descriptors are built this way — their first word is
            // a pointer to `__tlv_bootstrap`, which lives in libdyld.
            if relocation.kind == Arm64RelocationKind::Unsigned {
                if let RelocationTarget::Symbol(id) = relocation.target {
                    if let Some(symbol) = object.parsed.symbol(id) {
                        if imports.contains(&symbol.name) {
                            if let Some((segment_index, segment)) = image
                                .layout
                                .segments
                                .iter()
                                .enumerate()
                                .find(|(_, seg)| seg.name == output_section.segment)
                            {
                                extra_binds.push(Bind {
                                    segment: segment_index as u8,
                                    offset: place - segment.vm_address,
                                    symbol: symbol.name.clone(),
                                    library_ordinal: 1,
                                    addend: relocation.addend,
                                });
                            }
                            continue;
                        }
                    }
                }
            }

            // A descriptor's pointer to its variable is stored as an offset
            // into the per-thread block rather than as an address.
            let thread_local_offset = if relocation.kind == Arm64RelocationKind::Unsigned
                && output_section.name == "__thread_vars"
            {
                thread_local_base(image)
            } else {
                None
            };

            let target = match (stub, got.or(tlv)) {
                (Some(address), _) => address,
                // A GOT-based reference to an *imported* symbol has no address
                // of its own — that is the point of importing it. The
                // instruction is patched from the slot's address, and `target`
                // is unused for these kinds, so a failed lookup here is
                // expected rather than an error.
                (None, Some(_)) => {
                    target_address(object, image, addresses, relocation.target).unwrap_or(0)
                }
                (None, None) => target_address(object, image, addresses, relocation.target)?,
            };

            let Some(buffer) = contents.get_mut(&section_index) else {
                continue; // zero-filled section: nothing to patch
            };

            // Rewrite an address into a block-relative offset where the
            // descriptor expects one. Only targets that actually land in the
            // block are converted: a descriptor also holds a bound thunk
            // pointer and a key, and those are not offsets.
            let target = match thread_local_offset {
                Some(base) if in_thread_local_block(image, target) => target - base,
                _ => target,
            };

            // An absolute pointer stored in data has to be slid by dyld.
            // Excluded: fields in read-only segments (nothing writes them),
            // and thread-local descriptor offsets, which are offsets rather
            // than addresses and must not move.
            if relocation.kind == Arm64RelocationKind::Unsigned
                && relocation.length == blinker_macho::RelocationLength::Long
                && thread_local_offset.is_none()
                && output_section.segment != "__TEXT"
            {
                if let Some((segment_index, segment)) = image
                    .layout
                    .segments
                    .iter()
                    .enumerate()
                    .find(|(_, seg)| seg.name == output_section.segment)
                {
                    extra_rebases.push(Rebase {
                        segment: segment_index as u8,
                        offset: place - segment.vm_address,
                    });
                }
            }

            let field_offset = chunk_offset + relocation.offset;
            apply(
                relocation.kind,
                relocation.length,
                field_offset,
                Context {
                    place,
                    target,
                    addend: relocation.addend,
                    got,
                    tlv,
                    pc_relative: relocation.pc_relative,
                },
                buffer,
            )
            .map_err(|source| LinkError::Relocation {
                object: object.parsed.id,
                kind: relocation.kind,
                source: Box::new(source),
            })?;
        }

        if !record {
            continue;
        }
        records.push(ObjectRecord {
            object: object.parsed.id,
            deps: dependency_hashes(object, addresses, &referenced),
            binds: bind_start..extra_binds.len(),
            rebases: rebase_start..extra_rebases.len(),
        });
    }
    Ok(Patched {
        contents,
        binds: extra_binds,
        rebases: extra_rebases,
        records,
    })
}

/// The addend a Mach-O relocation stores in the bytes it patches.
///
/// Mach-O has no addend field: `InputRelocation::addend` is zero on every
/// relocation an object file actually contains, and the value is written into
/// the patch site instead. Read from the *input* bytes rather than from the
/// output buffer being assembled, so applying a relocation twice cannot
/// accumulate.
///
/// Sign-extended from the relocation's own width — these are signed
/// displacements, and a 4-byte field holding `-125` is `0xffffff83`, not
/// four billion.
fn inline_addend(object: &LoadedObject, relocation: &InputRelocation) -> i64 {
    let Some(file_offset) = object
        .parsed
        .section(relocation.section)
        .and_then(|s| s.file_offset)
    else {
        return 0;
    };
    let at = (file_offset + relocation.offset) as usize;
    match relocation.length {
        RelocationLength::Long => object
            .data
            .get(at..at + 8)
            .map(|b| i64::from_le_bytes(b.try_into().expect("8 bytes"))),
        RelocationLength::Word => object
            .data
            .get(at..at + 4)
            .map(|b| i32::from_le_bytes(b.try_into().expect("4 bytes")) as i64),
        RelocationLength::Half => object
            .data
            .get(at..at + 2)
            .map(|b| i16::from_le_bytes(b.try_into().expect("2 bytes")) as i64),
        RelocationLength::Byte => object.data.get(at).map(|b| *b as i8 as i64),
    }
    .unwrap_or(0)
}

/// The output address a relocation refers to.
fn target_address(
    object: &LoadedObject,
    image: &Image,
    addresses: &AddressMap,
    target: RelocationTarget,
) -> Result<u64, LinkError> {
    match target {
        RelocationTarget::Section(section) => image
            .layout
            .sections
            .iter()
            .find_map(|s| s.address_of(object.parsed.id, section))
            .ok_or(LinkError::MissingSection {
                object: object.parsed.id,
                section,
            }),
        RelocationTarget::Symbol(symbol_id) => {
            let symbol = object
                .parsed
                .symbol(symbol_id)
                .ok_or(LinkError::MissingSymbol { symbol: symbol_id })?;
            addresses
                .lookup(object.parsed.id, &symbol.name)
                .ok_or(LinkError::UndefinedSymbols {
                    names: vec![symbol.name.clone()],
                })
        }
    }
}

/// Where every defined symbol ended up in the output image.
///
/// Built once, across *all* objects. The first version of this searched only
/// the object holding the relocation, which works for locals and for
/// self-contained files, and fails the moment one object calls into another —
/// the definition is elsewhere, so the lookup reported the symbol undefined
/// even though resolution had already found it. Two coordinate systems that
/// happen to coincide in the single-object case is exactly the seam an
/// isolated test cannot see.
///
/// Locals are keyed per object because two objects may legitimately define the
/// same local name; globals are keyed by name alone.
#[derive(Default)]
struct AddressMap {
    global: HashMap<String, u64>,
    local: HashMap<(u32, String), u64>,
}

impl AddressMap {
    /// The scope in which `lookup` would find `name` from `object`.
    ///
    /// Paired with `lookup` so the cache hashes the address the linker would
    /// actually have read, rather than one that merely shares its name.
    fn scope_of(&self, object: ObjectId, name: &str) -> u32 {
        if self.local.contains_key(&(object.0, name.to_string())) {
            object.0
        } else {
            blinker_cache::GLOBAL
        }
    }

    fn lookup(&self, object: ObjectId, name: &str) -> Option<u64> {
        // A local definition in this object shadows a global of the same name,
        // which is what "local" means.
        self.local
            .get(&(object.0, name.to_string()))
            .or_else(|| self.global.get(name))
            .copied()
    }
}

/// Compute the output address of every definition.
fn address_map(objects: &[LoadedObject], image: &Image) -> AddressMap {
    let mut map = AddressMap::default();

    for object in objects {
        for symbol in &object.parsed.symbols {
            if !symbol.strength.is_definition() {
                continue;
            }
            let Some(section_id) = symbol.section else {
                continue;
            };
            let Some(input) = object.parsed.section(section_id) else {
                continue;
            };
            let Some(chunk) = image
                .layout
                .sections
                .iter()
                .find_map(|s| s.address_of(object.parsed.id, section_id))
            else {
                // Its section was dropped as linker-internal.
                continue;
            };
            // The symbol's value is an address in the object's own coordinate
            // space, so the offset within its section has to be recovered
            // first. Using the value directly would be right only when the
            // section begins at zero.
            let address = chunk + symbol.value.saturating_sub(input.vm_address);

            if symbol.visibility == SymbolVisibility::Local {
                map.local
                    .insert((object.parsed.id.0, symbol.name.clone()), address);
            } else {
                map.global.insert(symbol.name.clone(), address);
            }
        }
    }
    map
}

/// `LC_MAIN`'s entry offset: a **file** offset, not an address.
fn entry_offset(
    request: &LinkRequest,
    objects: &[LoadedObject],
    image: &Image,
) -> Result<u64, LinkError> {
    for object in objects {
        let Some(symbol) = object
            .parsed
            .symbols
            .iter()
            .find(|s| s.name == request.entry_symbol && s.strength.is_definition())
        else {
            continue;
        };
        let Some(section_id) = symbol.section else {
            continue;
        };
        let input = object
            .parsed
            .section(section_id)
            .ok_or(LinkError::MissingSection {
                object: object.parsed.id,
                section: section_id,
            })?;
        let offset_in_section = symbol.value.saturating_sub(input.vm_address);

        for section in &image.layout.sections {
            if let Some(address) = section.address_of(object.parsed.id, section_id) {
                let Some(file_offset) = section.file_offset else {
                    continue;
                };
                let chunk_offset = address - section.vm_address;
                return Ok(file_offset + chunk_offset + offset_in_section);
            }
        }
    }
    Err(LinkError::NoEntryPoint {
        symbol: request.entry_symbol.clone(),
    })
}

/// Convenience: link and write the result.
pub fn link_to_file(request: &LinkRequest, output: &Path) -> Result<Image, LinkError> {
    link_to_file_timed(request, output).map(|(image, _)| image)
}

/// As [`link_to_file`], reporting what the link did.
pub fn link_to_file_timed(
    request: &LinkRequest,
    output: &Path,
) -> Result<(Image, LinkTimings), LinkError> {
    let (image, timings) = link_timed(request)?;
    std::fs::write(output, &image.bytes).map_err(|source| LinkError::Write {
        path: output.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755)).map_err(
            |source| LinkError::Write {
                path: output.to_path_buf(),
                source,
            },
        )?;
    }
    Ok((image, timings))
}
