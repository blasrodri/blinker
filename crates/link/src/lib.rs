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

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use blinker_layout::InputPlacement;
use blinker_macho::{
    parse_object, Arm64RelocationKind, InputSection, ObjectId, ParsedObject, RelocationTarget,
    SectionId, SectionKind, SymbolVisibility,
};
use blinker_output::image::Dylib;
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Bind, Image, ImageBuilder, Rebase};
use blinker_relocations::{apply, Context};
use blinker_symbols::{SymbolProvider, SymbolTable};

pub mod error;
pub use error::LinkError;

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
}

impl LinkRequest {
    pub fn new(objects: Vec<PathBuf>) -> Self {
        LinkRequest {
            objects,
            entry_symbol: "_main".to_string(),
            identifier: "a.out".to_string(),
            dylibs: vec![Dylib::lib_system()],
            stub_libraries: default_stub_library().into_iter().collect(),
        }
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
/// `xcrun` is asked rather than a path assumed: the SDK moves between Xcode
/// versions and between Xcode and the Command Line Tools.
pub fn default_stub_library() -> Option<PathBuf> {
    let output = std::process::Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sdk = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(sdk).join("usr/lib/libSystem.tbd");
    path.exists().then_some(path)
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

/// Symbols an imported-function *call* needs a stub for.
///
/// Only branches need one: a `BRANCH26` cannot reach an address dyld will fill
/// in later, so it targets a stub that loads the bound pointer and jumps. Data
/// references already go through the GOT and need nothing extra.
fn stub_symbols(objects: &[LoadedObject], imports: &[String]) -> Vec<String> {
    let imported: std::collections::HashSet<&str> = imports.iter().map(String::as_str).collect();
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for object in objects {
        for relocation in &object.parsed.relocations {
            if relocation.kind != Arm64RelocationKind::Branch26 {
                continue;
            }
            let RelocationTarget::Symbol(id) = relocation.target else {
                continue;
            };
            let Some(symbol) = object.parsed.symbol(id) else {
                continue;
            };
            if imported.contains(symbol.name.as_str()) && seen.insert(symbol.name.clone()) {
                names.push(symbol.name.clone());
            }
        }
    }
    names.sort();
    names
}

/// Symbols that need a GOT entry, in a stable order.
///
/// A reference to data defined in *another* object goes through the GOT even
/// within a single executable: the compiler cannot know at compile time
/// whether the definition will end up in this image or in a dylib, so it emits
/// the indirect form and leaves the choice to the linker.
///
/// This is why the first end-to-end tests passed without a GOT at all — a
/// global read from the same object is a direct ADRP/ADD, and a call to
/// another object is a direct branch. Only cross-object *data* takes this path.
fn got_symbols(objects: &[LoadedObject]) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for object in objects {
        for relocation in &object.parsed.relocations {
            if !needs_got(relocation.kind) {
                continue;
            }
            let RelocationTarget::Symbol(id) = relocation.target else {
                continue;
            };
            let Some(symbol) = object.parsed.symbol(id) else {
                continue;
            };
            if seen.insert(symbol.name.clone()) {
                names.push(symbol.name.clone());
            }
        }
    }
    names
}

/// Link the request into an image.
pub fn link(request: &LinkRequest) -> Result<Image, LinkError> {
    let objects = load_objects(&request.objects)?;
    let mut placements = placements_for(&objects);

    if placements.is_empty() {
        return Err(LinkError::NothingToLink);
    }

    // Imports are resolved before the symbol table is checked, not after: an
    // undefined reference is only an error once the dylibs have had their
    // chance at it. Checking first reported `_printf` as undefined in a
    // program that links against libSystem.
    let imports = resolve_imports(&objects, request)?;

    // Resolution runs for its diagnostics: it is what turns a genuinely
    // missing definition into a named error rather than a relocation against
    // zero.
    resolve_symbols(&objects, &imports)?;

    let stubs = stub_symbols(&objects, &imports);
    // Synthesise `__got` before layout, so it is placed and addressed like any
    // other section rather than appended afterwards. Internal targets and
    // imports share one table: the difference is only how each slot gets its
    // value — a rebase for an address we know, a bind for one dyld supplies.
    let mut got = got_symbols(&objects);
    for name in &imports {
        if !got.contains(name) {
            got.push(name.clone());
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
    let probe = assemble(
        request,
        &Assembly {
            placements: &placements,
            ..Assembly::default()
        },
    )?;

    // With addresses known, copy content and patch it.
    let addresses = address_map(&objects, &probe);
    let got_slots = got_slot_addresses(&got, &probe);
    let stub_slots = stub_addresses(&stubs, &probe);
    let mut contents = build_contents(&objects, &probe, &placements)?;
    fill_got(&mut contents, &probe, &got, &addresses, &imports)?;
    fill_stubs(&mut contents, &probe, &stubs, &got_slots)?;
    let contents = apply_relocations(
        &objects,
        &probe,
        &addresses,
        &got_slots,
        &stub_slots,
        contents,
    )?;
    let entry_offset = entry_offset(request, &objects, &probe)?;

    // Pass two: the same layout, with real bytes.
    //
    // The symbol table grows between passes, which changes `LC_SYMTAB`'s
    // contents but not the load commands' *sizes* — so the section addresses
    // the relocations were computed against still hold.
    let symbols = output_symbols(&objects, &probe)?;

    // Each GOT slot holds an absolute address, and the image is position
    // independent, so dyld must relocate every one of them at load time.
    // A slot whose value we know is rebased; a slot dyld fills is bound.
    let rebases = got_rebases(&probe, &got, &imports);
    let binds = got_binds(&probe, &got, &imports);

    assemble(
        request,
        &Assembly {
            placements: &placements,
            symbols: &symbols,
            contents: Some(&contents),
            rebases: &rebases,
            binds: &binds,
            entry_offset,
        },
    )
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
fn got_binds(image: &Image, got: &[String], imports: &[String]) -> Vec<Bind> {
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
        .filter(|(_, name)| imports.contains(name))
        .map(|(slot, name)| Bind {
            segment: segment_index as u8,
            offset: base + slot as u64 * GOT_ENTRY_SIZE,
            symbol: name.clone(),
            // One-based; the only library in the list is libSystem.
            library_ordinal: 1,
            addend: 0,
        })
        .collect()
}

/// Address of each GOT slot, in the order the symbols were collected.
fn got_slot_addresses(got: &[String], image: &Image) -> HashMap<String, u64> {
    let mut slots = HashMap::new();
    let Some(section) = image.layout.sections.iter().find(|s| s.name == "__got") else {
        return slots;
    };
    for (index, name) in got.iter().enumerate() {
        slots.insert(
            name.clone(),
            section.vm_address + index as u64 * GOT_ENTRY_SIZE,
        );
    }
    slots
}

/// Write each GOT slot's initial value: the address of the symbol it points at.
fn fill_got(
    contents: &mut HashMap<usize, Vec<u8>>,
    image: &Image,
    got: &[String],
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

    for (slot, name) in got.iter().enumerate() {
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

/// One rebase entry per GOT slot.
fn got_rebases(image: &Image, got: &[String], imports: &[String]) -> Vec<Rebase> {
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
        .filter(|(_, name)| !imports.contains(name))
        .map(|(slot, _)| Rebase {
            segment: segment_index as u8,
            offset: base + slot as u64 * GOT_ENTRY_SIZE,
        })
        .collect()
}

fn load_objects(paths: &[PathBuf]) -> Result<Vec<LoadedObject>, LinkError> {
    let mut objects = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let data = std::fs::read(path).map_err(|source| LinkError::Read {
            path: path.clone(),
            source,
        })?;
        let parsed = parse_object(&data, path, None, ObjectId(index as u32))
            .map_err(|source| LinkError::Parse(Box::new(source)))?;
        objects.push(LoadedObject { parsed, data });
    }
    Ok(objects)
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
fn apply_relocations(
    objects: &[LoadedObject],
    image: &Image,
    addresses: &AddressMap,
    got_slots: &HashMap<String, u64>,
    stub_slots: &HashMap<String, u64>,
    mut contents: HashMap<usize, Vec<u8>>,
) -> Result<HashMap<usize, Vec<u8>>, LinkError> {
    for object in objects {
        for relocation in &object.parsed.relocations {
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

            let chunk_address = output_section
                .address_of(object.parsed.id, relocation.section)
                .expect("just matched");
            let chunk_offset = chunk_address - output_section.vm_address;
            let place = chunk_address + relocation.offset;

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

            let target = match (stub, got) {
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
                    tlv: None,
                },
                buffer,
            )
            .map_err(|source| LinkError::Relocation {
                object: object.parsed.id,
                kind: relocation.kind,
                source: Box::new(source),
            })?;
        }
    }
    Ok(contents)
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
    let image = link(request)?;
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
    Ok(image)
}
