//! The export trie: how a dylib says what it defines.
//!
//! An executable can get away without this. Nothing looks up a symbol in a
//! program, so blinker has emitted `export_off = 0, export_size = 0` since the
//! first image and nothing has ever noticed. A dylib is the opposite case: the
//! trie *is* the library's interface, and a dylib without one exports nothing,
//! links against nothing, and fails at the first crate that uses it.
//!
//! # The format
//!
//! Not a symbol table — a compressed prefix tree, walked by dyld one character
//! at a time. Rust mangles names with long shared prefixes (`_ZN4core3fmt…`),
//! which is exactly the shape a trie collapses: the prefix is stored once and
//! every symbol under it is an edge.
//!
//! Each node is:
//!
//! ```text
//!   uleb  terminal_size        0 for an interior node
//!   ...   terminal payload     uleb flags, then uleb address, when terminal
//!   u8    child_count
//!   ...   per child: NUL-terminated edge label, uleb offset of the child node
//! ```
//!
//! The child offset is absolute — measured from the start of the trie — which
//! is what makes encoding circular: an offset is ULEB128, so its *size*
//! depends on its *value*, and every node's value depends on the sizes of the
//! nodes before it. [`encode`] resolves that by laying the nodes out
//! repeatedly until nothing moves, which is what `ld64` does and for the same
//! reason.
//!
//! # What this deliberately does not do
//!
//! Re-exports (`EXPORT_SYMBOL_FLAGS_REEXPORT`) and resolver stubs
//! (`…_STUB_AND_RESOLVER`) have their own terminal payloads and are not
//! emitted: nothing in a Rust `cdylib` or proc-macro produces either. They are
//! named here so the omission is a decision rather than a gap — the encoder
//! rejects flags it does not model rather than writing a payload that would be
//! read as an address.

/// A symbol the image offers to whoever links against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    /// Offset from the mach header, which for a dylib based at zero is the
    /// symbol's address. Not the file offset: dyld adds this to the load
    /// address, and the two differ for anything past `__TEXT`.
    pub address: u64,
    pub flags: u32,
}

/// `EXPORT_SYMBOL_FLAGS_KIND_REGULAR` — an ordinary defined symbol.
pub const KIND_REGULAR: u32 = 0x00;
/// `EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL`.
pub const KIND_THREAD_LOCAL: u32 = 0x01;
/// `EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE`.
pub const KIND_ABSOLUTE: u32 = 0x02;
/// `EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION`.
pub const WEAK_DEFINITION: u32 = 0x04;
/// `EXPORT_SYMBOL_FLAGS_REEXPORT` — modelled only well enough to refuse.
pub const REEXPORT: u32 = 0x08;
/// `EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER` — likewise.
pub const STUB_AND_RESOLVER: u32 = 0x10;

/// Flags whose terminal payload is `flags` followed by one address.
const MODELLED: u32 = KIND_REGULAR | KIND_THREAD_LOCAL | KIND_ABSOLUTE | WEAK_DEFINITION;

#[derive(Debug, PartialEq, Eq)]
pub enum ExportError {
    /// A flag combination whose payload is not `flags` + address.
    UnmodelledFlags(u32),
    /// Two exports with one name. Silently keeping either would make the
    /// library's interface depend on input order.
    Duplicate(String),
    /// An edge label with an interior NUL, which the format cannot express.
    NulInName(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::UnmodelledFlags(flags) => {
                write!(f, "export flags {flags:#x} are not modelled")
            }
            ExportError::Duplicate(name) => write!(f, "{name} is exported twice"),
            ExportError::NulInName(name) => write!(f, "{name} contains a NUL"),
        }
    }
}

impl std::error::Error for ExportError {}

/// One node of the trie, in an arena indexed by `usize`.
#[derive(Default)]
struct Node {
    /// The terminal payload, when a symbol ends exactly here.
    terminal: Option<Vec<u8>>,
    /// Edge label and child index. Every label at one node starts with a
    /// distinct byte, which is the invariant `insert` maintains and what makes
    /// "does any child share a prefix" a question with at most one answer.
    children: Vec<(String, usize)>,
    offset: usize,
}

fn uleb(value: u64, out: &mut Vec<u8>) {
    let mut value = value;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn uleb_size(value: u64) -> usize {
    let mut size = 1;
    let mut value = value >> 7;
    while value != 0 {
        size += 1;
        value >>= 7;
    }
    size
}

fn common_prefix(a: &str, b: &str) -> usize {
    // Bytes, not characters: edge labels are split at byte boundaries and
    // symbol names are ASCII in practice, but a mangled name carrying UTF-8
    // must not be split mid-character.
    let mut common = 0;
    for (left, right) in a.as_bytes().iter().zip(b.as_bytes()) {
        if left != right {
            break;
        }
        common += 1;
    }
    while common > 0 && !a.is_char_boundary(common) {
        common -= 1;
    }
    common
}

/// Build the trie and serialize it.
///
/// Returns an empty vector for no exports — not a one-byte empty root. dyld
/// reads `export_size = 0` as "nothing exported", and a trie containing only a
/// root says the same thing in more bytes; `ld64` emits nothing, and matching
/// it costs nothing.
pub fn encode(exports: &[Export]) -> Result<Vec<u8>, ExportError> {
    if exports.is_empty() {
        return Ok(Vec::new());
    }

    // Sorted so the trie's shape, and therefore its bytes, depend on the set
    // of exports and not on the order they were discovered in. The daemon
    // makes that a correctness property rather than a nicety: a warm link and
    // a cold link must produce the same image.
    let mut sorted: Vec<&Export> = exports.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for pair in sorted.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(ExportError::Duplicate(pair[0].name.clone()));
        }
    }

    let mut arena: Vec<Node> = vec![Node::default()];
    for export in &sorted {
        if export.name.contains('\0') {
            return Err(ExportError::NulInName(export.name.clone()));
        }
        if export.flags & !MODELLED != 0 {
            return Err(ExportError::UnmodelledFlags(export.flags));
        }
        let mut payload = Vec::new();
        uleb(export.flags as u64, &mut payload);
        uleb(export.address, &mut payload);
        insert(&mut arena, &export.name, payload);
    }

    Ok(serialize(&mut arena))
}

/// Add one symbol, splitting an edge when it diverges partway along.
fn insert(arena: &mut Vec<Node>, name: &str, payload: Vec<u8>) {
    let mut node = 0usize;
    let mut rest = name;
    loop {
        if rest.is_empty() {
            arena[node].terminal = Some(payload);
            return;
        }
        // At most one child can match: labels at a node start with distinct
        // bytes.
        let matched = arena[node]
            .children
            .iter()
            .position(|(label, _)| common_prefix(label, rest) > 0);
        let Some(at) = matched else {
            let leaf = arena.len();
            arena.push(Node {
                terminal: Some(payload),
                ..Node::default()
            });
            arena[node].children.push((rest.to_string(), leaf));
            return;
        };
        let (label, child) = arena[node].children[at].clone();
        let common = common_prefix(&label, rest);
        if common == label.len() {
            node = child;
            rest = &rest[common..];
            continue;
        }
        // The edge and the new name agree for a while and then diverge, so
        // the edge becomes two: a shared head, and the old tail hanging off a
        // new interior node.
        let split = arena.len();
        arena.push(Node {
            children: vec![(label[common..].to_string(), child)],
            ..Node::default()
        });
        arena[node].children[at] = (label[..common].to_string(), split);
        node = split;
        rest = &rest[common..];
    }
}

/// Lay the nodes out until no offset moves, then write them.
fn serialize(arena: &mut [Node]) -> Vec<u8> {
    // Children sorted by label, for the same determinism reason as the input
    // sort. dyld does not care about the order; two runs of blinker do.
    for node in arena.iter_mut() {
        node.children.sort_by(|a, b| a.0.cmp(&b.0));
    }

    // The circular part. A node's size includes ULEB offsets of its children,
    // whose values depend on the sizes of everything before them — so lay out,
    // measure, and repeat until a pass changes nothing. It converges because
    // each pass can only be driven by a ULEB growing, and a ULEB for a trie
    // that fits in memory grows a bounded number of times; the cap is a
    // guard against a bug here becoming a hang in a linker.
    for _ in 0..64 {
        let mut offset = 0usize;
        let mut moved = false;
        for index in 0..arena.len() {
            if arena[index].offset != offset {
                arena[index].offset = offset;
                moved = true;
            }
            offset += size_of_node(arena, index);
        }
        if !moved {
            break;
        }
    }

    let mut out = Vec::new();
    for index in 0..arena.len() {
        debug_assert_eq!(
            arena[index].offset,
            out.len(),
            "node {index} was laid out somewhere other than where it was written"
        );
        write_node(arena, index, &mut out);
    }
    out
}

fn size_of_node(arena: &[Node], index: usize) -> usize {
    let node = &arena[index];
    let terminal = node.terminal.as_ref().map_or(0, Vec::len);
    let mut size = uleb_size(terminal as u64) + terminal + 1;
    for (label, child) in &node.children {
        size += label.len() + 1 + uleb_size(arena[*child].offset as u64);
    }
    size
}

fn write_node(arena: &[Node], index: usize, out: &mut Vec<u8>) {
    let node = &arena[index];
    match &node.terminal {
        Some(payload) => {
            uleb(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }
        None => uleb(0, out),
    }
    out.push(node.children.len() as u8);
    for (label, child) in &node.children {
        out.extend_from_slice(label.as_bytes());
        out.push(0);
        uleb(arena[*child].offset as u64, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Walk a trie the way dyld does, sharing no code with the encoder.
    ///
    /// Deliberately a second implementation. A decoder built from the
    /// encoder's own helpers agrees with it about everything including its
    /// mistakes, which is how a symbol table that named the wrong things once
    /// passed a byte comparison in this project.
    fn walk(trie: &[u8]) -> BTreeMap<String, (u32, u64)> {
        fn read_uleb(bytes: &[u8], at: &mut usize) -> u64 {
            let mut value = 0u64;
            let mut shift = 0;
            loop {
                let byte = bytes[*at];
                *at += 1;
                value |= ((byte & 0x7f) as u64) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        }
        fn visit(trie: &[u8], node: usize, prefix: &str, found: &mut BTreeMap<String, (u32, u64)>) {
            let mut at = node;
            let terminal = read_uleb(trie, &mut at) as usize;
            if terminal > 0 {
                let end = at + terminal;
                let flags = read_uleb(trie, &mut at) as u32;
                let address = read_uleb(trie, &mut at);
                assert_eq!(at, end, "the terminal payload was not its declared size");
                found.insert(prefix.to_string(), (flags, address));
                at = end;
            }
            let children = trie[at];
            at += 1;
            for _ in 0..children {
                let start = at;
                while trie[at] != 0 {
                    at += 1;
                }
                let label = std::str::from_utf8(&trie[start..at]).expect("utf8 label");
                at += 1;
                let child = read_uleb(trie, &mut at) as usize;
                visit(trie, child, &format!("{prefix}{label}"), found);
            }
        }
        let mut found = BTreeMap::new();
        if !trie.is_empty() {
            visit(trie, 0, "", &mut found);
        }
        found
    }

    fn export(name: &str, address: u64) -> Export {
        Export {
            name: name.to_string(),
            address,
            flags: KIND_REGULAR,
        }
    }

    #[test]
    fn nothing_exported_is_no_trie_at_all() {
        assert!(encode(&[]).expect("encodes").is_empty());
    }

    #[test]
    fn every_symbol_comes_back_out() {
        let exports = vec![
            export("_main", 0x4000),
            export("_helper", 0x4100),
            export("_help", 0x4200),
            export("_h", 0x4300),
            export("_zzz", 0x4400),
        ];
        let trie = encode(&exports).expect("encodes");
        let found = walk(&trie);
        assert_eq!(found.len(), exports.len());
        for export in &exports {
            assert_eq!(
                found.get(&export.name),
                Some(&(export.flags, export.address)),
                "{} did not survive the round trip",
                export.name
            );
        }
    }

    /// The case the format exists for, and the one that breaks a naive
    /// encoder: a symbol that is a strict prefix of another is *both* an
    /// interior node and a terminal one.
    #[test]
    fn a_symbol_that_is_a_prefix_of_another_is_still_exported() {
        let trie =
            encode(&[export("_ab", 1), export("_abc", 2), export("_abcd", 3)]).expect("encodes");
        let found = walk(&trie);
        assert_eq!(found["_ab"], (KIND_REGULAR, 1));
        assert_eq!(found["_abc"], (KIND_REGULAR, 2));
        assert_eq!(found["_abcd"], (KIND_REGULAR, 3));
    }

    /// Mangled Rust names, which is what this will actually hold.
    #[test]
    fn long_shared_prefixes_survive_being_split_repeatedly() {
        let names: Vec<String> = (0..200)
            .map(|i| format!("__ZN4core3fmt9Formatter{i:03}17h0123456789abcdefE"))
            .collect();
        let exports: Vec<Export> = names
            .iter()
            .enumerate()
            .map(|(i, name)| export(name, 0x4000 + i as u64 * 16))
            .collect();
        let trie = encode(&exports).expect("encodes");
        let found = walk(&trie);
        assert_eq!(found.len(), 200);
        for (i, name) in names.iter().enumerate() {
            assert_eq!(found[name].1, 0x4000 + i as u64 * 16);
        }
    }

    /// Enough symbols that child offsets need more than one ULEB byte, which
    /// is the case the fixed-point layout exists for. An encoder that sized
    /// offsets once and wrote them later produces a trie that decodes into
    /// nonsense from the first node whose offset grew.
    #[test]
    fn offsets_that_grow_are_laid_out_until_they_stop_moving() {
        let exports: Vec<Export> = (0..5000)
            .map(|i| {
                export(
                    &format!("_symbol_{i:05}_padded_out_to_make_the_trie_large"),
                    i,
                )
            })
            .collect();
        let trie = encode(&exports).expect("encodes");
        assert!(trie.len() > 128, "the trie is too small to test offsets");
        let found = walk(&trie);
        assert_eq!(found.len(), 5000);
        for i in 0..5000u64 {
            let name = format!("_symbol_{i:05}_padded_out_to_make_the_trie_large");
            assert_eq!(found[&name].1, i, "{name} decoded to the wrong address");
        }
    }

    #[test]
    fn the_same_exports_in_a_different_order_are_the_same_bytes() {
        let mut forward = vec![export("_b", 2), export("_a", 1), export("_ab", 3)];
        let mut backward = forward.clone();
        backward.reverse();
        forward.rotate_left(1);
        assert_eq!(
            encode(&forward).expect("encodes"),
            encode(&backward).expect("encodes"),
            "the trie depends on discovery order, so a warm link would differ from a cold one"
        );
    }

    #[test]
    fn flags_reach_the_terminal_payload() {
        let weak = Export {
            name: "_w".to_string(),
            address: 0x100,
            flags: KIND_REGULAR | WEAK_DEFINITION,
        };
        let thread_local = Export {
            name: "_t".to_string(),
            address: 0x200,
            flags: KIND_THREAD_LOCAL,
        };
        let found = walk(&encode(&[weak.clone(), thread_local.clone()]).expect("encodes"));
        assert_eq!(found["_w"], (weak.flags, weak.address));
        assert_eq!(found["_t"], (thread_local.flags, thread_local.address));
    }

    /// Refused rather than mis-encoded: a re-export's payload is an ordinal
    /// and a name, so writing an address there produces a trie that decodes
    /// cleanly and means something else entirely.
    #[test]
    fn unmodelled_flags_are_refused_rather_than_written_as_an_address() {
        for flags in [REEXPORT, STUB_AND_RESOLVER] {
            let bad = Export {
                name: "_x".to_string(),
                address: 0,
                flags,
            };
            assert_eq!(encode(&[bad]), Err(ExportError::UnmodelledFlags(flags)));
        }
    }

    #[test]
    fn one_name_exported_twice_is_an_error_rather_than_a_choice() {
        let twice = [export("_dup", 1), export("_dup", 2)];
        assert_eq!(
            encode(&twice),
            Err(ExportError::Duplicate("_dup".to_string()))
        );
    }

    /// Read the export trie out of a dylib `ld64` produced, and walk it.
    ///
    /// Everything above tests the encoder against the decoder written next to
    /// it, and two implementations of one misunderstanding agree perfectly.
    /// This is the check that the misunderstanding is not shared: a real
    /// linker's bytes, walked by our decoder, have to yield the symbols the
    /// source actually defines. If the format here were wrong, this is where
    /// it shows.
    #[test]
    fn a_real_dylib_from_the_system_linker_decodes() {
        let directory = std::env::temp_dir().join(format!("blinker-trie-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch");
        let source = directory.join("lib.c");
        let dylib = directory.join("lib.dylib");
        std::fs::write(
            &source,
            "int exported_alpha(void) { return 1; }\n\
             int exported_beta(void) { return 2; }\n\
             __attribute__((visibility(\"hidden\"))) int hidden_one(void) { return 3; }\n",
        )
        .expect("source");
        let built = std::process::Command::new("cc")
            .arg("-dynamiclib")
            .arg("-o")
            .arg(&dylib)
            .arg(&source)
            .status();
        // A machine without a C compiler is not a failing linker.
        let Ok(status) = built else { return };
        assert!(status.success(), "cc failed to build a dylib");

        let bytes = std::fs::read(&dylib).expect("dylib");
        let trie = export_trie_of(&bytes).expect("the dylib has an export trie");
        let found = walk(trie);
        let _ = std::fs::remove_dir_all(&directory);

        assert!(
            found.contains_key("_exported_alpha") && found.contains_key("_exported_beta"),
            "a real dylib's exports did not decode: {:?}",
            found.keys().collect::<Vec<_>>()
        );
        assert!(
            !found.contains_key("_hidden_one"),
            "a hidden symbol appeared in the export trie, so the walk is reading something else"
        );
    }

    /// Locate the export trie in a thin 64-bit Mach-O.
    ///
    /// Two commands can carry it: `LC_DYLD_INFO_ONLY` has the offset in its
    /// eleventh and twelfth words, and `LC_DYLD_EXPORTS_TRIE` — what a recent
    /// `ld64` emits — is a `linkedit_data_command` holding only that. Which
    /// one appears depends on the deployment target, so both are read.
    fn export_trie_of(image: &[u8]) -> Option<&[u8]> {
        const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
        const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;
        let word = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?))
        };
        if word(0)? != 0xfeed_facf {
            return None;
        }
        let count = word(16)?;
        let mut at = 32;
        for _ in 0..count {
            let kind = word(at)?;
            let size = word(at + 4)? as usize;
            let (offset, length) = match kind {
                LC_DYLD_INFO_ONLY => (word(at + 40)? as usize, word(at + 44)? as usize),
                LC_DYLD_EXPORTS_TRIE => (word(at + 8)? as usize, word(at + 12)? as usize),
                _ => {
                    at += size;
                    continue;
                }
            };
            if length == 0 {
                at += size;
                continue;
            }
            return image.get(offset..offset + length);
        }
        None
    }

    #[test]
    fn uleb_matches_its_own_size_function() {
        for value in [0u64, 1, 0x7f, 0x80, 0x3fff, 0x4000, u64::MAX] {
            let mut bytes = Vec::new();
            uleb(value, &mut bytes);
            assert_eq!(
                bytes.len(),
                uleb_size(value),
                "size disagreed for {value:#x}"
            );
        }
    }
}
