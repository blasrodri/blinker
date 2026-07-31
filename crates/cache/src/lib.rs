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
//! 1. **its input is unchanged** — by content hash, never by path. rustc's
//!    object filenames carry a per-build session component that changes every
//!    build (finding 15), so a path-keyed cache has a 0% hit rate by
//!    construction;
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
pub const SCHEMA: u32 = 1;

const MAGIC: &[u8; 8] = b"BLNKCAC\x01";

/// A symbol name reduced to a hash.
///
/// Names are the bulk of a link's memory and most of them are Rust mangled
/// symbols hundreds of bytes long. The cache never needs to *read* a name back,
/// only to tell whether the set of addresses changed, so it stores 8 bytes
/// instead of the string.
pub type NameHash = u64;

/// Hash a symbol name into the form the cache stores.
pub fn name_hash(name: &str) -> NameHash {
    let bytes = blake3::hash(name.as_bytes());
    u64::from_le_bytes(bytes.as_bytes()[..8].try_into().expect("8 bytes"))
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

/// One object's cached contribution to the link.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    /// BLAKE3 of the input file. Condition 1.
    pub content_hash: [u8; 32],
    /// Where this object's bytes sit in the output. Condition 2.
    pub ranges: Vec<Range>,
    /// Name hashes this object's relocations resolved against, sorted and
    /// deduplicated. Condition 3.
    pub deps: Vec<NameHash>,
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
        content_hash: &[u8; 32],
        ranges: &[Range],
        changed: &std::collections::HashSet<NameHash>,
    ) -> bool {
        if &self.content_hash != content_hash || self.ranges != ranges {
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
        out.bytes_raw(&entry.content_hash);
        out.u32(entry.ranges.len() as u32);
        for range in &entry.ranges {
            out.u32(range.section);
            out.u64(range.start);
            out.u64(range.len);
        }
        out.u32(entry.deps.len() as u32);
        for dep in &entry.deps {
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
    out.finish()
}

fn decode(bytes: &[u8]) -> Option<LinkCache> {
    let mut input = Decoder::new(bytes);
    if input.bytes_raw(MAGIC.len())? != MAGIC || input.u32()? != SCHEMA {
        return None;
    }

    let mut entries = Vec::new();
    for _ in 0..input.u32()? {
        let mut entry = Entry {
            content_hash: input.bytes_raw(32)?.try_into().ok()?,
            ..Entry::default()
        };
        for _ in 0..input.u32()? {
            entry.ranges.push(Range {
                section: input.u32()?,
                start: input.u64()?,
                len: input.u64()?,
            });
        }
        for _ in 0..input.u32()? {
            entry.deps.push(input.u64()?);
        }
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

    // Trailing bytes mean the file is not what it claims to be.
    input.at_end().then_some(LinkCache {
        entries,
        addresses,
        sections,
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
                content_hash: [7u8; 32],
                ranges: vec![Range {
                    section: 1,
                    start: 0x40,
                    len: 0x100,
                }],
                deps: vec![name_hash("_main"), name_hash("_puts")],
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
        assert!(entry.is_reusable(&[7u8; 32], &entry.ranges, &HashSet::new()));
    }

    #[test]
    fn a_changed_input_makes_its_entry_unreusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        assert!(!entry.is_reusable(&[9u8; 32], &entry.ranges, &HashSet::new()));
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
        assert!(!entry.is_reusable(&[7u8; 32], &moved, &HashSet::new()));
    }

    /// Condition 3, and the one an isolated per-file cache would get wrong:
    /// this object did not change and did not move, but something it points at
    /// did.
    #[test]
    fn an_unchanged_entry_whose_dependency_moved_is_unreusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        let changed = HashSet::from([name_hash("_puts")]);
        assert!(!entry.is_reusable(&[7u8; 32], &entry.ranges, &changed));
    }

    #[test]
    fn a_change_to_a_symbol_this_entry_ignores_leaves_it_reusable() {
        let cache = sample();
        let entry = &cache.entries[0];
        let changed = HashSet::from([name_hash("_something_else")]);
        assert!(entry.is_reusable(&[7u8; 32], &entry.ranges, &changed));
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
