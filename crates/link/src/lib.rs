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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use blinker_layout::InputPlacement;
use blinker_macho::{
    parse_object, Arm64RelocationKind, InputSection, ObjectId, ParsedObject, RelocationTarget,
    SectionId, SectionKind, SymbolVisibility,
};
use blinker_output::image::Dylib;
use blinker_output::symtab::OutputSymbol;
use blinker_output::{Image, ImageBuilder, Rebase};
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
}

impl LinkRequest {
    pub fn new(objects: Vec<PathBuf>) -> Self {
        LinkRequest {
            objects,
            entry_symbol: "_main".to_string(),
            identifier: "a.out".to_string(),
            dylibs: vec![Dylib::lib_system()],
        }
    }

    pub fn identifier(mut self, identifier: &str) -> Self {
        self.identifier = identifier.to_string();
        self
    }
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
    // Resolution runs for its diagnostics: it is what turns a missing
    // definition into a named error rather than a relocation against zero.
    resolve_symbols(&objects)?;
    let mut placements = placements_for(&objects);

    if placements.is_empty() {
        return Err(LinkError::NothingToLink);
    }

    // Synthesise `__got` before layout, so it is placed and addressed like any
    // other section rather than appended afterwards.
    let got = got_symbols(&objects);
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
    let probe = assemble(request, &objects, &placements, &[], &HashMap::new(), &[], 0)?;

    // With addresses known, copy content and patch it.
    let addresses = address_map(&objects, &probe);
    let got_slots = got_slot_addresses(&got, &probe);
    let mut contents = build_contents(&objects, &probe, &placements)?;
    fill_got(&mut contents, &probe, &got, &addresses)?;
    let contents = apply_relocations(&objects, &probe, &addresses, &got_slots, contents)?;
    let entry_offset = entry_offset(request, &objects, &probe)?;

    // Pass two: the same layout, with real bytes.
    //
    // The symbol table grows between passes, which changes `LC_SYMTAB`'s
    // contents but not the load commands' *sizes* — so the section addresses
    // the relocations were computed against still hold.
    let symbols = output_symbols(&objects, &probe)?;

    // Each GOT slot holds an absolute address, and the image is position
    // independent, so dyld must relocate every one of them at load time.
    let rebases = got_rebases(&probe, got.len());

    assemble(
        request,
        &objects,
        &placements,
        &symbols,
        &contents,
        &rebases,
        entry_offset,
    )
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
fn got_rebases(image: &Image, count: usize) -> Vec<Rebase> {
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
    (0..count as u64)
        .map(|slot| Rebase {
            segment: segment_index as u8,
            offset: base + slot * GOT_ENTRY_SIZE,
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
fn resolve_symbols(objects: &[LoadedObject]) -> Result<SymbolTable, LinkError> {
    let mut table = SymbolTable::new();

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
fn assemble(
    request: &LinkRequest,
    _objects: &[LoadedObject],
    placements: &[InputPlacement],
    output_symbols: &[OutputSymbol],
    contents: &HashMap<usize, Vec<u8>>,
    rebases: &[Rebase],
    entry_offset: u64,
) -> Result<Image, LinkError> {
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
    let section_count = placements.len();
    for index in 0..section_count {
        if let Some(bytes) = contents.get(&index) {
            builder.content(index, bytes.clone());
        }
    }

    for symbol in output_symbols {
        builder.symbols().add(symbol.clone());
    }
    for rebase in rebases {
        builder.rebase(*rebase);
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

            let target = target_address(object, image, addresses, relocation.target)?;

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
