//! The incremental link cache: relocated output bytes, keyed by content.
//!
//! # What is cached, and why it is not the parse
//!
//! M4 was originally specified as a cache of *parse results*. That design was
//! measured and killed (finding 41): blinker's parser borrows section bytes
//! rather than copying them, so parsing a 9.4 MB object costs 0.43 ms, while
//! deserialising the same `ParsedObject` costs 0.75–1.50 ms — it must allocate
//! every `String` and `Vec` the parser merely pointed at. A parse cache is
//! slower than parsing.
//!
//! What this caches instead is the **relocated section bytes**, and the same
//! test says the opposite about them (finding 59):
//!
//! ```text
//!   recompute (the relocate stage)  7.3 ms
//!   load from a warm page cache     0.065 ms   (1.03 MB)
//! ```
//!
//! 112× rather than 0.3×, and the reason is the same one that killed the parse
//! cache, inverted: patched bytes deserialise into a `Vec<u8>` — one
//! allocation and a copy — because there is no structure in them to rebuild.
//!
//! # When a cached contribution is still valid
//!
//! An object's patched bytes are reusable only if all three hold:
//!
//! 1. **its input is unchanged** — by [`InputKey`], which hashes rustc's own
//!    objects and trusts the path of a content-addressed toolchain rlib.
//!    Hashing everything costs exactly what the cache saves (findings 60, 61);
//! 2. **its contribution has not moved** — same output section, same offset,
//!    same length. Layout slop (finding 43) is what usually makes this true
//!    after an edit, and when the slop is outgrown this is the condition that
//!    notices;
//! 3. **nothing it references has moved.** A body edit to object X can shift
//!    the addresses of X's own symbols, and every object that referenced them
//!    holds bytes that are now wrong — even though those objects did not
//!    change at all.
//!
//! Condition 3 is the one that makes this a *graph* rather than a set of
//! independent files, and it is why each entry records the symbols it resolved
//! against. Validating it does not cost a lookup per relocation: the addresses
//! that changed since the previous link are diffed once, into a small set, and
//! each entry's dependency list is tested against that set.
//!
//! # Failure is always a cold link
//!
//! Every fallible path here returns `None` rather than an error. A cache that
//! cannot be read, or was written by a different schema, or disagrees with
//! itself, must produce a slower link and never a wrong one — so there is no
//! way to express "use it anyway" in this API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod codec;
use codec::{Decoder, Encoder};

/// Bumped whenever any structure below changes shape.
///
/// A stale layout read as a current one is the one failure mode of a cache
/// that produces a *wrong* binary rather than a slow one, so the version is
/// checked before any other byte is trusted.
pub const SCHEMA: u32 = 4;

const MAGIC: &[u8; 8] = b"BLNKCAC\x01";

/// One address's identity, reduced to a hash. See [`dep_hash`].
pub type NameHash = u64;

/// Which table an address was read from.
///
/// A symbol can have up to four distinct addresses in one link — itself, its
/// GOT slot, its stub, and its thread-local pointer slot — and they move
/// independently. Adding an entry to the GOT shifts every slot after it while
/// leaving every symbol address untouched, so a dependency recorded without
/// this distinction would be checked against the wrong number and report "no
/// change" for bytes that are now wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Table {
    /// The symbol's own address.
    Symbol = 0,
    Got = 1,
    Stub = 2,
    ThreadLocal = 3,
}

/// The scope of a name that is defined per object rather than link-wide.
///
/// Local symbols are keyed by `(object, name)` because two objects may
/// legitimately define the same local name — every Rust object defines its own
/// `GCC_except_table1` and its own `ltmpN` (finding 57). Hashing such a name
/// alone would merge them into one dependency, and a change to either would
/// then look like a change to both.
pub const GLOBAL: u32 = u32::MAX;

/// Hash a symbol name into the form the cache stores.
///
/// Names are the bulk of a link's memory and most of them are Rust mangled
/// symbols hundreds of bytes long. The cache never needs to *read* a name
/// back, only to tell whether the set of addresses changed, so it stores 8
/// bytes instead of the string.
pub fn name_hash(name: &str) -> NameHash {
    dep_hash(GLOBAL, Table::Symbol, name)
}

/// Hash one address's identity: which name, in which scope, from which table.
///
/// All three matter, and the hash must mirror the linker's own lookup exactly.
/// Where the linker would find a local definition, this must scope to that
/// object; where it would fall through to the global map, this must not.
pub fn dep_hash(scope: u32, table: Table, name: &str) -> NameHash {
    combine(name_digest(name), scope, table)
}

/// The expensive half: BLAKE3 of the name, and of nothing else.
///
/// Split out from [`dep_hash`] so it can be computed once per *distinct name*
/// rather than once per address. A debug rust-analyzer link asks for 506,405
/// address hashes, and hashing the name text for each was 140 ms of a 1,265 ms
/// link — for names that had not changed, in a linker whose whole premise is
/// that they usually have not. The linker holds these beside its interning
/// table, so a held input's names are never hashed twice.
pub fn name_digest(name: &str) -> NameHash {
    u64::from_le_bytes(
        blake3::hash(name.as_bytes()).as_bytes()[..8]
            .try_into()
            .expect("8 bytes"),
    )
}

/// The cheap half: fold the scope and table into a name's digest.
///
/// Mixed in *after* the name rather than hashed before it, which is what makes
/// the digest reusable across every scope and table the same name appears in.
///
/// The finaliser is a bijection for any fixed `(scope, table)`, so two triples
/// collide exactly when their names' 64-bit digests collide under the xor —
/// the same birthday bound the concatenated hash had. Nothing here is a
/// cheaper hash of the name: BLAKE3 still runs over the text, once. See
/// finding 153 for why that is not negotiable.
pub fn combine(digest: NameHash, scope: u32, table: Table) -> NameHash {
    let mut x = digest ^ (((scope as u64) << 3) | table as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

/// Where one object's patched bytes live in an output section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub section: u32,
    pub start: u64,
    pub len: u64,
}

/// A dyld bind the relocation pass produced for this object.
///
/// Stored because binds and rebases are generated *during* relocation: an
/// object whose bytes are reused must contribute its fixups too, and
/// recomputing them would mean walking its relocations, which is the work
/// being skipped. Mirrors `blinker_output::Bind` field for field rather than
/// compressing it — the symbol here is an imported name, of which there are a
/// few hundred, not the hundreds of thousands that made hashing worthwhile for
/// `deps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBind {
    pub segment: u8,
    pub offset: u64,
    pub symbol: String,
    pub library_ordinal: u8,
    pub addend: i64,
}

/// An absolute pointer dyld must slide. Mirrors `blinker_output::Rebase`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedRebase {
    pub segment: u8,
    pub offset: u64,
}

/// How an input is proven unchanged.
///
/// Two classes of input reach a Rust link, and using one key for both is what
/// made a first version of this cache exactly break-even (findings 60, 61):
///
/// ```text
///   37 .o     rustc per-build codegen units    0.31 MB    1.8% of bytes
///   19 .rlib  toolchain libraries             16.87 MB   98.2% of bytes
/// ```
///
/// Hashing all of it costs 7.28 ms — within noise of the 7.3 ms the cache
/// saves. Hashing only rustc's output costs **0.16 ms**, and the rest is
/// covered soundly by metadata, because toolchain rlibs live at paths that are
/// already content-addressed: rustup writes `libstd-4f24f0876fd27385.rlib`,
/// hash included in the name. That file cannot change without changing path.
///
/// rustc's own objects get the opposite treatment for the opposite reason
/// (finding 15): their names carry a per-build session component, so the path
/// changes every build even when the bytes do not, and only content can
/// identify them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputKey {
    /// BLAKE3 of the file's bytes. For inputs whose path is not evidence.
    Content([u8; 32]),
    /// Path, modification time and size. For inputs whose path is evidence.
    Metadata {
        path: PathBuf,
        modified_nanos: u128,
        size: u64,
    },
}

impl InputKey {
    /// Choose a key for `path`, reading the file only when the path is not
    /// itself evidence of its content.
    ///
    /// Returns `None` when the file cannot be examined at all — a missing
    /// input is a cold link, not an error to propagate from here.
    pub fn probe(path: &Path) -> Option<InputKey> {
        if path_is_content_addressed(path) {
            let metadata = std::fs::metadata(path).ok()?;
            return Some(InputKey::Metadata {
                path: path.to_path_buf(),
                modified_nanos: metadata
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_nanos(),
                size: metadata.len(),
            });
        }
        let bytes = std::fs::read(path).ok()?;
        Some(InputKey::Content(*blake3::hash(&bytes).as_bytes()))
    }
}

/// Whether a path is strong enough evidence of its own content.
///
/// True for the toolchain's `.rlib`s, which rustup names with a content hash.
/// Deliberately conservative: anything not recognised is hashed, because the
/// cost of hashing an input unnecessarily is microseconds and the cost of
/// trusting a path that lied is a wrong binary.
fn path_is_content_addressed(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("rlib") {
        return false;
    }
    // `lib<crate>-<16 hex digits>.rlib`. The hash is what makes the path
    // evidence; an `.rlib` without one is just a file that happens to be an
    // archive, and gets hashed like anything else.
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    match stem.rsplit_once('-') {
        Some((_, hash)) => hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

/// One object's cached contribution to the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// How this input is proven unchanged. Condition 1.
    pub key: InputKey,
    /// Where this object's bytes sit in the output. Condition 2.
    pub ranges: Vec<Range>,
    /// Name hashes this object's relocations resolved against, sorted and
    /// deduplicated. Condition 3.
    /// Shared, not owned. Every link clones an entry's deps twice — once when
    /// the reuse path carries a reused object's record forward, and once when
    /// the next cache is built from those records — and on a link that reuses
    /// 211 objects that is 422 copies of a list this process already has. A
    /// refcount does the same job.
    pub deps: std::sync::Arc<[NameHash]>,
    pub binds: Vec<CachedBind>,
    pub rebases: Vec<CachedRebase>,
}

/// The complete record of one link, as far as a later link can reuse it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LinkCache {
    /// Entries by object index, in the order the link loaded them.
    pub entries: Vec<Entry>,
    /// Every defined symbol's output address, sorted by name hash.
    ///
    /// Sorted so that diffing two links is a merge rather than a hash join,
    /// and so the encoding is deterministic — two links over identical inputs
    /// must produce identical cache files, or a byte-comparison test cannot
    /// tell a real difference from map iteration order.
    pub addresses: Vec<(NameHash, u64)>,
    /// The patched output sections, by output section index.
    pub sections: Vec<(u32, Vec<u8>)>,
    /// The link's input files, in order, with the key that proves each one.
    ///
    /// Kept separately from `entries` because it is checked *before* anything
    /// is read: proving all 56 inputs of a Rust link unchanged costs 0.18 ms
    /// (0.16 to hash rustc's output, 0.024 to stat the rlibs), against 22.6 ms
    /// to link. That ratio is what makes the whole-image path below worth
    /// having.
    pub inputs: Vec<(PathBuf, InputKey)>,
    /// Everything about the request that is not an input file.
    ///
    /// The entry symbol, the dylibs, the stub libraries, the signing
    /// identifier. Identical inputs linked with a different entry point are a
    /// different binary, and nothing in `inputs` would say so.
    pub request: [u8; 32],
    /// Where this link put everything, so the next one can put it back.
    ///
    /// The one part of this cache that is *not* a copy of something the output
    /// already holds. Entries, addresses and sections are all recoverable from
    /// the image and the inputs; where a contribution sat is a decision, and a
    /// decision cannot be recomputed — recomputing it with padding and hoping
    /// it lands the same is what finding 94 measured at 9 reused relocations
    /// out of 84 116.
    pub layout: blinker_layout::PreviousLayout,
    /// SHA-256 of each 16 KiB page of `image`, as the code directory stores
    /// them.
    ///
    /// Kept beside the image rather than parsed back out of the signature it
    /// already contains: the reuse path then depends on a field, not on being
    /// able to re-read a structure another crate wrote. 32 bytes per 16 KiB is
    /// 3.5 KB on a 1.8 MB binary.
    pub page_hashes: Vec<[u8; 32]>,
    /// The finished, signed binary.
    ///
    /// Present so that a link whose inputs are *all* unchanged can skip the
    /// pipeline outright rather than rebuild a result it already has. This is
    /// the one case where the cache can be sure without doing any of the work:
    /// same inputs, same request, same output — no reasoning about which
    /// contributions moved, because none of them did.
    pub image: Vec<u8>,
}

impl LinkCache {
    /// Whether this cache describes exactly the link about to be performed.
    ///
    /// Order matters as much as content: the same objects in a different order
    /// lay out differently, so the comparison is positional rather than a set
    /// comparison.
    pub fn matches(&self, inputs: &[(PathBuf, InputKey)], request: &[u8; 32]) -> bool {
        !self.image.is_empty() && &self.request == request && self.inputs == inputs
    }
}

impl LinkCache {
    /// The addresses that differ between this link and a previous one.
    ///
    /// A symbol that appeared, disappeared, or moved all count: each makes any
    /// entry that referenced it unreusable. Both sides are sorted by name hash,
    /// so this is a linear merge — the cost is proportional to the symbol
    /// table, once, rather than to the relocations, per object.
    pub fn changed_addresses(&self, previous: &LinkCache) -> Vec<NameHash> {
        let (mut a, mut b) = (0, 0);
        let (old, new) = (&previous.addresses, &self.addresses);
        let mut changed = Vec::new();
        while a < old.len() && b < new.len() {
            let (on, oa) = old[a];
            let (nn, na) = new[b];
            match on.cmp(&nn) {
                std::cmp::Ordering::Less => {
                    // Defined before, gone now: anything referencing it is stale.
                    changed.push(on);
                    a += 1;
                }
                std::cmp::Ordering::Greater => {
                    changed.push(nn);
                    b += 1;
                }
                std::cmp::Ordering::Equal => {
                    if oa != na {
                        changed.push(on);
                    }
                    a += 1;
                    b += 1;
                }
            }
        }
        changed.extend(old[a..].iter().map(|(n, _)| *n));
        changed.extend(new[b..].iter().map(|(n, _)| *n));
        changed.sort_unstable();
        changed.dedup();
        changed
    }
}

impl Entry {
    /// Whether this entry's bytes may be reused.
    ///
    /// All three conditions from the module documentation, in the order that
    /// rejects most cheaply first: the content hash is a 32-byte compare, the
    /// ranges a handful of integers, and only then the dependency scan.
    pub fn is_reusable(
        &self,
        key: &InputKey,
        ranges: &[Range],
        changed: &std::collections::HashSet<NameHash>,
    ) -> bool {
        if &self.key != key || self.ranges != ranges {
            return false;
        }
        // `changed` is small after an ordinary edit and `deps` is large, so the
        // scan is driven by whichever the caller made small — a set probe per
        // dependency, not a search per change.
        !self.deps.iter().any(|d| changed.contains(d))
    }
}

/// Where a cache lives for a given output binary.
///
/// Under `CARGO_TARGET_DIR` when the build system set one, so that
/// `cargo clean` removes the cache along with everything else it describes: a
/// cache that outlives its build tree is a cache describing objects that no
/// longer exist.
pub fn cache_path(output: &Path) -> PathBuf {
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let directory = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(target) => PathBuf::from(target).join("blinker-cache"),
        None => output
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".blinker-cache"),
    };
    directory.join(format!("{name}.blinkcache"))
}

/// Read a cache, or `None` for any reason at all.
pub fn load(path: &Path) -> Option<LinkCache> {
    decode(&std::fs::read(path).ok()?)
}

/// Write a cache, replacing any previous one atomically.
///
/// Atomically because a link that is interrupted mid-write would otherwise
/// leave a truncated file that the *next* link reads as authoritative. The
/// temporary sits in the same directory so the rename cannot cross a
/// filesystem boundary.
pub fn store(path: &Path, cache: &LinkCache) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temporary, encode(cache))?;
    std::fs::rename(&temporary, path)
}

fn encode(cache: &LinkCache) -> Vec<u8> {
    let mut out = Encoder::new();
    out.bytes_raw(MAGIC);
    out.u32(SCHEMA);

    out.u32(cache.entries.len() as u32);
    for entry in &cache.entries {
        encode_key(&mut out, &entry.key);
        out.u32(entry.ranges.len() as u32);
        for range in &entry.ranges {
            out.u32(range.section);
            out.u64(range.start);
            out.u64(range.len);
        }
        out.u32(entry.deps.len() as u32);
        for dep in entry.deps.iter() {
            out.u64(*dep);
        }
        out.u32(entry.binds.len() as u32);
        for bind in &entry.binds {
            out.u32(bind.segment as u32);
            out.u64(bind.offset);
            out.u32(bind.symbol.len() as u32);
            out.bytes_raw(bind.symbol.as_bytes());
            out.u32(bind.library_ordinal as u32);
            out.u64(bind.addend as u64);
        }
        out.u32(entry.rebases.len() as u32);
        for rebase in &entry.rebases {
            out.u32(rebase.segment as u32);
            out.u64(rebase.offset);
        }
    }

    out.u32(cache.addresses.len() as u32);
    for (name, address) in &cache.addresses {
        out.u64(*name);
        out.u64(*address);
    }

    out.u32(cache.sections.len() as u32);
    for (index, bytes) in &cache.sections {
        out.u32(*index);
        out.u32(bytes.len() as u32);
        out.bytes_raw(bytes);
    }

    out.u32(cache.inputs.len() as u32);
    for (path, key) in &cache.inputs {
        let path = path.to_string_lossy();
        out.u32(path.len() as u32);
        out.bytes_raw(path.as_bytes());
        encode_key(&mut out, key);
    }
    out.bytes_raw(&cache.request);

    out.u32(cache.layout.sections.len() as u32);
    for (name, section) in &cache.layout.sections {
        out.u32(name.len() as u32);
        out.bytes_raw(name.as_bytes());
        out.u64(section.vm_address);
        // `None` is a zero-filled section, which occupies no file bytes. Sent
        // as a discriminant rather than as a sentinel offset: offset zero is a
        // real place, and __TEXT starts there.
        match section.file_offset {
            Some(offset) => {
                out.u32(1);
                out.u64(offset);
            }
            None => out.u32(0),
        }
        out.u64(section.reserved);
    }

    let mut slots: Vec<_> = cache.layout.slots.iter().collect();
    // Sorted so that two equal caches encode identically, which is what lets a
    // test compare bytes rather than structures.
    slots.sort_unstable_by_key(|(key, _)| key.0);
    out.u32(slots.len() as u32);
    for (key, slot) in slots {
        out.u64(key.0);
        out.u32(slot.section.len() as u32);
        out.bytes_raw(slot.section.as_bytes());
        out.u64(slot.offset);
        out.u64(slot.capacity);
    }

    out.u32(cache.page_hashes.len() as u32);
    for hash in &cache.page_hashes {
        out.bytes_raw(hash);
    }

    out.u32(cache.image.len() as u32);
    out.bytes_raw(&cache.image);
    out.finish()
}

fn encode_key(out: &mut Encoder, key: &InputKey) {
    match key {
        InputKey::Content(hash) => {
            out.u32(0);
            out.bytes_raw(hash);
        }
        InputKey::Metadata {
            path,
            modified_nanos,
            size,
        } => {
            out.u32(1);
            let path = path.to_string_lossy();
            out.u32(path.len() as u32);
            out.bytes_raw(path.as_bytes());
            out.u64((*modified_nanos >> 64) as u64);
            out.u64(*modified_nanos as u64);
            out.u64(*size);
        }
    }
}

fn decode_key(input: &mut Decoder<'_>) -> Option<InputKey> {
    Some(match input.u32()? {
        0 => InputKey::Content(input.bytes_raw(32)?.try_into().ok()?),
        1 => {
            let length = input.u32()? as usize;
            let path = std::str::from_utf8(input.bytes_raw(length)?).ok()?;
            let high = input.u64()? as u128;
            let low = input.u64()? as u128;
            InputKey::Metadata {
                path: PathBuf::from(path),
                modified_nanos: (high << 64) | low,
                size: input.u64()?,
            }
        }
        _ => return None,
    })
}

fn decode(bytes: &[u8]) -> Option<LinkCache> {
    let mut input = Decoder::new(bytes);
    if input.bytes_raw(MAGIC.len())? != MAGIC || input.u32()? != SCHEMA {
        return None;
    }

    let mut entries = Vec::new();
    for _ in 0..input.u32()? {
        let key = decode_key(&mut input)?;
        let mut entry = Entry {
            key,
            ranges: Vec::new(),
            deps: Vec::new().into(),
            binds: Vec::new(),
            rebases: Vec::new(),
        };
        for _ in 0..input.u32()? {
            entry.ranges.push(Range {
                section: input.u32()?,
                start: input.u64()?,
                len: input.u64()?,
            });
        }
        let mut deps = Vec::new();
        for _ in 0..input.u32()? {
            deps.push(input.u64()?);
        }
        entry.deps = deps.into();
        for _ in 0..input.u32()? {
            let segment = u8::try_from(input.u32()?).ok()?;
            let offset = input.u64()?;
            let length = input.u32()? as usize;
            let symbol = std::str::from_utf8(input.bytes_raw(length)?)
                .ok()?
                .to_string();
            entry.binds.push(CachedBind {
                segment,
                offset,
                symbol,
                library_ordinal: u8::try_from(input.u32()?).ok()?,
                addend: input.u64()? as i64,
            });
        }
        for _ in 0..input.u32()? {
            entry.rebases.push(CachedRebase {
                segment: u8::try_from(input.u32()?).ok()?,
                offset: input.u64()?,
            });
        }
        entries.push(entry);
    }

    let mut addresses = Vec::new();
    for _ in 0..input.u32()? {
        addresses.push((input.u64()?, input.u64()?));
    }

    let mut sections = Vec::new();
    for _ in 0..input.u32()? {
        let index = input.u32()?;
        let length = input.u32()? as usize;
        sections.push((index, input.bytes_raw(length)?.to_vec()));
    }

    let mut inputs = Vec::new();
    for _ in 0..input.u32()? {
        let length = input.u32()? as usize;
        let path = PathBuf::from(std::str::from_utf8(input.bytes_raw(length)?).ok()?);
        inputs.push((path, decode_key(&mut input)?));
    }
    let request: [u8; 32] = input.bytes_raw(32)?.try_into().ok()?;

    let mut layout = blinker_layout::PreviousLayout::default();
    for _ in 0..input.u32()? {
        let length = input.u32()? as usize;
        let name = std::str::from_utf8(input.bytes_raw(length)?)
            .ok()?
            .to_string();
        let vm_address = input.u64()?;
        let file_offset = match input.u32()? {
            1 => Some(input.u64()?),
            0 => None,
            _ => return None,
        };
        layout.sections.insert(
            name,
            blinker_layout::PreviousSection {
                vm_address,
                file_offset,
                reserved: input.u64()?,
            },
        );
    }
    for _ in 0..input.u32()? {
        let key = blinker_layout::ContributionKey(input.u64()?);
        let length = input.u32()? as usize;
        let section = std::str::from_utf8(input.bytes_raw(length)?)
            .ok()?
            .to_string();
        layout.slots.insert(
            key,
            blinker_layout::PreviousSlot {
                section,
                offset: input.u64()?,
                capacity: input.u64()?,
            },
        );
    }

    let mut page_hashes = Vec::new();
    for _ in 0..input.u32()? {
        page_hashes.push(input.bytes_raw(32)?.try_into().ok()?);
    }

    let length = input.u32()? as usize;
    let image = input.bytes_raw(length)?.to_vec();

    // Trailing bytes mean the file is not what it claims to be.
    input.at_end().then_some(LinkCache {
        entries,
        addresses,
        sections,
        inputs,
        request,
        layout,
        page_hashes,
        image,
    })
}

/// Build the sorted address vector the cache stores from a name-keyed map.
pub fn addresses_from(map: impl IntoIterator<Item = (String, u64)>) -> Vec<(NameHash, u64)> {
    let mut deduplicated: HashMap<NameHash, u64> = HashMap::new();
    for (name, address) in map {
        deduplicated.insert(name_hash(&name), address);
    }
    let mut out: Vec<_> = deduplicated.into_iter().collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn sample() -> LinkCache {
        LinkCache {
            entries: vec![Entry {
                key: InputKey::Content([7u8; 32]),
                ranges: vec![Range {
                    section: 1,
                    start: 0x40,
                    len: 0x100,
                }],
                deps: vec![name_hash("_main"), name_hash("_puts")].into(),
                binds: vec![CachedBind {
                    segment: 2,
                    offset: 8,
                    symbol: "_puts".to_string(),
                    library_ordinal: 1,
                    addend: -16,
                }],
                rebases: vec![CachedRebase {
                    segment: 2,
                    offset: 16,
                }],
            }],
            addresses: addresses_from([("_main".to_string(), 0x1000)]),
            sections: vec![(1, vec![0xab; 64])],
            inputs: vec![(PathBuf::from("/tmp/a.o"), InputKey::Content([1u8; 32]))],
            request: [2u8; 32],
            page_hashes: vec![[3u8; 32], [4u8; 32]],
            layout: {
                let mut layout = blinker_layout::PreviousLayout::default();
                layout.sections.insert(
                    "__TEXT,__text".to_string(),
                    blinker_layout::PreviousSection {
                        vm_address: 0x1_0000_4000,
                        file_offset: Some(0x4000),
                        reserved: 0x8000,
                    },
                );
                // A zero-filled section, so the file-offset discriminant is
                // exercised rather than only the common case.
                layout.sections.insert(
                    "__DATA,__bss".to_string(),
                    blinker_layout::PreviousSection {
                        vm_address: 0x1_0001_0000,
                        file_offset: None,
                        reserved: 0x1000,
                    },
                );
                layout.slots.insert(
                    blinker_layout::ContributionKey(0x9e37_79b9_7f4a_7c15),
                    blinker_layout::PreviousSlot {
                        section: "__TEXT,__text".to_string(),
                        offset: 0x120,
                        capacity: 0x200,
                    },
                );
                layout
            },
            image: vec![0xcd; 128],
        }
    }

    #[test]
    fn a_cache_survives_a_round_trip() {
        let cache = sample();
        assert_eq!(decode(&encode(&cache)), Some(cache));
    }

    /// Two encodings of equal caches must be byte-identical, or no test can
    /// distinguish a real change from map iteration order.
    #[test]
    fn encoding_is_deterministic() {
        assert_eq!(encode(&sample()), encode(&sample()));
    }

    #[test]
    fn a_cache_from_another_schema_is_refused_rather_than_misread() {
        let mut bytes = encode(&sample());
        bytes[MAGIC.len()] = bytes[MAGIC.len()].wrapping_add(1);
        assert_eq!(decode(&bytes), None);
    }

    #[test]
    fn a_file_that_is_not_a_cache_is_refused() {
        assert_eq!(decode(b"not a cache at all"), None);
        assert_eq!(decode(b""), None);
    }

    /// The failure mode a cache must never have: reading a truncated file as
    /// though it were complete.
    #[test]
    fn every_truncation_is_refused() {
        let bytes = encode(&sample());
        for cut in 0..bytes.len() {
            assert_eq!(decode(&bytes[..cut]), None, "accepted a {cut}-byte prefix");
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = encode(&sample());
        bytes.push(0);
        assert_eq!(decode(&bytes), None);
    }

    #[test]
    fn an_unchanged_entry_is_reusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        assert!(entry.is_reusable(
            &InputKey::Content([7u8; 32]),
            &entry.ranges,
            &HashSet::new()
        ));
    }

    #[test]
    fn a_changed_input_makes_its_entry_unreusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        assert!(!entry.is_reusable(
            &InputKey::Content([9u8; 32]),
            &entry.ranges,
            &HashSet::new()
        ));
    }

    /// Condition 2: the same bytes at a different offset are the wrong bytes,
    /// because every relocation in them was computed against the old address.
    #[test]
    fn a_moved_contribution_makes_its_entry_unreusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        let moved = vec![Range {
            start: 0x80,
            ..entry.ranges[0].clone()
        }];
        assert!(!entry.is_reusable(&InputKey::Content([7u8; 32]), &moved, &HashSet::new()));
    }

    /// Condition 3, and the one an isolated per-file cache would get wrong:
    /// this object did not change and did not move, but something it points at
    /// did.
    #[test]
    fn an_unchanged_entry_whose_dependency_moved_is_unreusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        let changed = HashSet::from([name_hash("_puts")]);
        assert!(!entry.is_reusable(&InputKey::Content([7u8; 32]), &entry.ranges, &changed));
    }

    #[test]
    fn a_change_to_a_symbol_this_entry_ignores_leaves_it_reusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        let changed = HashSet::from([name_hash("_something_else")]);
        assert!(entry.is_reusable(&InputKey::Content([7u8; 32]), &entry.ranges, &changed));
    }

    #[test]
    fn identical_links_report_no_changed_addresses() {
        assert!(sample().changed_addresses(&sample()).is_empty());
    }

    #[test]
    fn a_moved_symbol_is_reported_as_changed() {
        let mut after = sample();
        after.addresses = addresses_from([("_main".to_string(), 0x2000)]);
        assert_eq!(after.changed_addresses(&sample()), vec![name_hash("_main")]);
    }

    /// A symbol that only exists on one side is a change on both counts: the
    /// references to it are new, or the references to it are now dangling.
    #[test]
    fn an_added_or_removed_symbol_is_reported_as_changed() {
        let before = sample();
        let mut after = sample();
        after.addresses = addresses_from([
            ("_main".to_string(), 0x1000),
            ("_extra".to_string(), 0x3000),
        ]);
        assert_eq!(after.changed_addresses(&before), vec![name_hash("_extra")]);
        assert_eq!(before.changed_addresses(&after), vec![name_hash("_extra")]);
    }

    #[test]
    fn the_merge_reports_every_difference_across_a_long_run() {
        let before = LinkCache {
            addresses: addresses_from((0..500).map(|n| (format!("_s{n}"), n * 16))),
            ..LinkCache::default()
        };
        let after = LinkCache {
            // One moved, one removed, one added.
            addresses: addresses_from(
                (0..500)
                    .filter(|n| *n != 100)
                    .map(|n| (format!("_s{n}"), if n == 7 { 0xdead } else { n * 16 }))
                    .chain([("_new".to_string(), 0xbeef)]),
            ),
            ..LinkCache::default()
        };
        let mut changed = after.changed_addresses(&before);
        changed.sort_unstable();
        let mut expected = vec![name_hash("_s7"), name_hash("_s100"), name_hash("_new")];
        expected.sort_unstable();
        assert_eq!(changed, expected);
    }

    #[test]
    fn a_cache_is_written_and_read_back_through_the_filesystem() {
        let scratch = blinker_test_support::Scratch::dir("cache").unwrap();
        let path = scratch.join("nested/link.blinkcache");
        store(&path, &sample()).unwrap();
        assert_eq!(load(&path), Some(sample()));
    }

    #[test]
    fn a_missing_cache_loads_as_none_rather_than_failing() {
        assert_eq!(load(Path::new("/nonexistent/link.blinkcache")), None);
    }

    /// Path-keyed caching has a 0% hit rate on rustc output (finding 15), so
    /// the location must not depend on anything that changes per build.
    #[test]
    fn the_cache_path_is_derived_from_the_output_name() {
        let path = cache_path(Path::new("/tmp/build/myprog"));
        assert!(path.to_string_lossy().ends_with("myprog.blinkcache"));
    }
}

#[cfg(test)]
mod input_key_tests {
    use super::*;

    /// The 98.2% of bytes that must never be hashed, or the cache breaks even
    /// with doing nothing (findings 60, 61).
    #[test]
    fn a_rustup_rlib_is_keyed_by_its_path() {
        let scratch = blinker_test_support::Scratch::dir("key-rlib").unwrap();
        scratch
            .write("libstd-4f24f0876fd27385.rlib", "archive")
            .unwrap();
        let key = InputKey::probe(&scratch.join("libstd-4f24f0876fd27385.rlib"));
        assert!(matches!(key, Some(InputKey::Metadata { .. })), "{key:?}");
    }

    /// The 1.8% that must always be hashed: rustc rewrites these every build
    /// under a new name (finding 15), so nothing about the path is evidence.
    #[test]
    fn a_rustc_codegen_object_is_keyed_by_content() {
        let scratch = blinker_test_support::Scratch::dir("key-obj").unwrap();
        scratch
            .write("uw-9277bce136af823e.0f4x6c3m.023ewd0.rcgu.o", "object")
            .unwrap();
        let key = InputKey::probe(&scratch.join("uw-9277bce136af823e.0f4x6c3m.023ewd0.rcgu.o"));
        assert!(matches!(key, Some(InputKey::Content(_))), "{key:?}");
    }

    /// An `.rlib` without a hash in its name is just an archive. Cargo writes
    /// these for local crates, and their paths are reused across builds.
    #[test]
    fn an_rlib_without_a_hash_in_its_name_is_keyed_by_content() {
        let scratch = blinker_test_support::Scratch::dir("key-plain").unwrap();
        scratch.write("libmine.rlib", "archive").unwrap();
        assert!(matches!(
            InputKey::probe(&scratch.join("libmine.rlib")),
            Some(InputKey::Content(_))
        ));
    }

    /// A near-miss must fall to the safe side: the cost of hashing when the
    /// path would have done is microseconds, the cost of the reverse is a
    /// wrong binary.
    #[test]
    fn a_name_that_only_looks_content_addressed_is_still_hashed() {
        let scratch = blinker_test_support::Scratch::dir("key-near").unwrap();
        for name in [
            "libstd-4f24f0876fd2738.rlib",   // 15 digits, not 16
            "libstd-4f24f0876fd273855.rlib", // 17
            "libstd-zzzzzzzzzzzzzzzz.rlib",  // not hex
            "libstd.rlib",                   // no hash at all
        ] {
            scratch.write(name, "archive").unwrap();
            assert!(
                matches!(
                    InputKey::probe(&scratch.join(name)),
                    Some(InputKey::Content(_))
                ),
                "{name} was trusted on its path alone"
            );
        }
    }

    #[test]
    fn changed_content_changes_a_content_key() {
        let scratch = blinker_test_support::Scratch::dir("key-change").unwrap();
        scratch.write("a.o", "before").unwrap();
        let before = InputKey::probe(&scratch.join("a.o"));
        scratch.write("a.o", "after!").unwrap();
        assert_ne!(before, InputKey::probe(&scratch.join("a.o")));
    }

    /// The property the whole design rests on: rustc renames an object every
    /// build, and identical bytes under a new name must still be a hit.
    #[test]
    fn identical_bytes_under_a_different_name_share_a_content_key() {
        let scratch = blinker_test_support::Scratch::dir("key-rename").unwrap();
        scratch
            .write("crate-abc.cgu1.session1.rcgu.o", "same bytes")
            .unwrap();
        scratch
            .write("crate-abc.cgu1.session2.rcgu.o", "same bytes")
            .unwrap();
        assert_eq!(
            InputKey::probe(&scratch.join("crate-abc.cgu1.session1.rcgu.o")),
            InputKey::probe(&scratch.join("crate-abc.cgu1.session2.rcgu.o")),
        );
    }

    #[test]
    fn a_missing_input_probes_as_none_rather_than_failing() {
        assert_eq!(InputKey::probe(Path::new("/nonexistent/a.o")), None);
    }
}

#[cfg(test)]
mod dep_hash_tests {
    use super::*;

    /// The four addresses a symbol can have move independently: adding a GOT
    /// entry shifts every slot after it and no symbol at all.
    #[test]
    fn each_table_gives_a_symbol_a_distinct_identity() {
        let hashes = [Table::Symbol, Table::Got, Table::Stub, Table::ThreadLocal]
            .map(|table| dep_hash(GLOBAL, table, "_main"));
        let mut unique = hashes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 4, "two tables collided: {hashes:?}");
    }

    /// Finding 57: every Rust object defines its own `GCC_except_table1`.
    /// Merging them would make a change to one look like a change to all.
    #[test]
    fn the_same_local_name_in_two_objects_is_two_dependencies() {
        assert_ne!(
            dep_hash(3, Table::Symbol, "GCC_except_table1"),
            dep_hash(8, Table::Symbol, "GCC_except_table1"),
        );
    }

    /// A local definition and a global of the same name are different
    /// addresses, and `AddressMap::lookup` picks the local one. The hash has to
    /// make the same distinction or it would check the wrong address.
    #[test]
    fn a_scoped_name_is_distinct_from_the_global_of_the_same_name() {
        assert_ne!(
            dep_hash(3, Table::Symbol, "_helper"),
            dep_hash(GLOBAL, Table::Symbol, "_helper"),
        );
    }

    #[test]
    fn name_hash_is_the_global_symbol_case() {
        assert_eq!(name_hash("_main"), dep_hash(GLOBAL, Table::Symbol, "_main"));
    }

    #[test]
    fn hashing_is_stable_across_calls() {
        assert_eq!(name_hash("_main"), name_hash("_main"));
        assert_ne!(name_hash("_main"), name_hash("_other"));
    }
}
