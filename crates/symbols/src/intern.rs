//! Symbol name interning.
//!
//! Symbol names dominate a link's memory. A single Rust binary reaches ~9,000
//! symbols from libSystem alone, and mangled Rust names run past 100 characters
//! —  `__RNvXNtCsgtPOCBgevO_4core7convertReINtB2_5AsRefNtNtCs…` is typical.
//! Every reference to a name would otherwise be another allocation and another
//! string comparison.
//!
//! Interning gives each distinct name one [`SymbolNameId`], so resolution
//! compares integers rather than strings, and the storage is paid once.

use blinker_hashing::FastMap;

/// A interned symbol name.
///
/// Only meaningful relative to the [`SymbolNames`] table that produced it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct SymbolNameId(pub u32);

/// An interning table mapping names to [`SymbolNameId`]s and back.
///
/// # One allocation, not one per name
///
/// This held `Vec<Arc<str>>` plus a `HashMap<Arc<str>, _>` sharing the same
/// allocations — an improvement on holding the text twice (finding 152), and
/// still an allocation for every distinct name in the program: 477,532 of them
/// on a debug rust-analyzer link.
///
/// Worse than the allocations was what they did to the map. `HashMap` does not
/// store the hash it computed, so every growth of the table rehashes every key
/// already in it — and rehashing a key meant chasing an `Arc` pointer into
/// scattered memory and hashing sixty bytes of mangled name again. Growing to
/// 477,532 entries from empty is nineteen doublings, and the sum of them is
/// about a million string hashes nobody asked for. That is finding 135's
/// pattern, in the one container where it costs the most.
///
/// So the names live end to end in one buffer, an id is a span in it, and the
/// index is keyed by the *hash* rather than by the text. A `u64` key rehashes
/// in two instructions and dereferences nothing, so growth is nearly free;
/// comparing a candidate reads a contiguous slice rather than following a
/// pointer. Distinct names that hash alike are kept together and told apart by
/// their text, so the table is a lookup structure and not a claim that the
/// hash is unique.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolNames {
    /// Every name, concatenated.
    arena: Vec<u8>,
    /// Per id, where its bytes start in `arena` and how many there are.
    spans: Vec<(u32, u32)>,
    /// Name hash -> the ids whose text hashes there. Almost always exactly
    /// one, which is why `Few` keeps the first inline.
    ///
    /// Rebuilt on deserialization rather than stored, since it is derivable
    /// and would grow the cached size.
    #[serde(skip)]
    lookup: FastMap<u64, crate::Few<SymbolNameId>>,
}

/// Hash a name to the key the index is bucketed by.
///
/// Fast-hashed, not `std`'s SipHash. This is the most probed name map in the
/// linker — 981,253 times on a debug rust-analyzer link, once per symbol of
/// every object — and it was the one map `blinker_hashing`'s conversion
/// missed, despite that module's own note that names were the reason it was
/// written. Switching it alone was 87 ms of a cold link.
fn hash_of(name: &str) -> u64 {
    use std::hash::Hasher;
    let mut hasher = blinker_hashing::FastHasher::default();
    hasher.write(name.as_bytes());
    hasher.finish()
}

impl SymbolNames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `name`, returning its existing ID if already present.
    pub fn intern(&mut self, name: &str) -> SymbolNameId {
        let hash = hash_of(name);
        if let Some(found) = self.lookup.get(&hash) {
            // Copied out before the table is touched, and cheap to copy: the
            // common case is one id.
            for id in found.all() {
                if self.text(id) == Some(name) {
                    return id;
                }
            }
        }
        let id = SymbolNameId(self.spans.len() as u32);
        let start = self.arena.len() as u32;
        self.arena.extend_from_slice(name.as_bytes());
        self.spans.push((start, name.len() as u32));
        match self.lookup.entry(hash) {
            std::collections::hash_map::Entry::Occupied(mut held) => held.get_mut().push(id),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(crate::Few::new(id));
            }
        }
        id
    }

    /// The name behind an ID.
    pub fn resolve(&self, id: SymbolNameId) -> Option<&str> {
        self.text(id)
    }

    fn text(&self, id: SymbolNameId) -> Option<&str> {
        let &(start, len) = self.spans.get(id.0 as usize)?;
        let bytes = self.arena.get(start as usize..(start + len) as usize)?;
        std::str::from_utf8(bytes).ok()
    }

    /// Look up an existing name without interning it.
    pub fn get(&self, name: &str) -> Option<SymbolNameId> {
        let found = self.lookup.get(&hash_of(name))?;
        found.all().find(|id| self.text(*id) == Some(name))
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Rebuild the reverse index after deserialization.
    ///
    /// The index is skipped when serializing because it is derivable; a table
    /// read back from the cache must be repaired before `intern` or `get` will
    /// behave, and forgetting to call this would silently produce duplicate
    /// IDs for names already in the table.
    pub fn rebuild_index(&mut self) {
        self.lookup = FastMap::default();
        for index in 0..self.spans.len() {
            let id = SymbolNameId(index as u32);
            let Some(name) = self.text(id) else {
                continue;
            };
            let hash = hash_of(name);
            match self.lookup.entry(hash) {
                std::collections::hash_map::Entry::Occupied(mut held) => held.get_mut().push(id),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(crate::Few::new(id));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_the_same_name_twice_yields_one_id() {
        let mut names = SymbolNames::new();
        let first = names.intern("_main");
        let second = names.intern("_main");
        assert_eq!(first, second);
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn distinct_names_get_distinct_ids() {
        let mut names = SymbolNames::new();
        let main = names.intern("_main");
        let malloc = names.intern("_malloc");
        assert_ne!(main, malloc);
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn ids_resolve_back_to_their_names() {
        let mut names = SymbolNames::new();
        let id = names.intern("_malloc");
        assert_eq!(names.resolve(id), Some("_malloc"));
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing() {
        let names = SymbolNames::new();
        assert_eq!(names.resolve(SymbolNameId(99)), None);
    }

    #[test]
    fn get_does_not_intern() {
        let mut names = SymbolNames::new();
        assert_eq!(names.get("_absent"), None);
        assert!(names.is_empty());

        names.intern("_present");
        assert!(names.get("_present").is_some());
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn handles_the_long_mangled_names_rust_actually_produces() {
        let mangled = "__RNvXNtCsgtPOCBgevO_4core7convertReINtB2_5AsRefNtNtCsa9BKTri5B3M_\
                       3std4path4PathE6as_refCs7edmagIO8a1_6ignore";
        let mut names = SymbolNames::new();
        let id = names.intern(mangled);
        assert_eq!(names.resolve(id), Some(mangled));
        assert_eq!(names.intern(mangled), id);
    }

    #[test]
    fn empty_names_are_distinct_from_absent_ones() {
        // A Mach-O symbol table can contain an empty name; it must intern to a
        // real ID rather than being confused with "not present".
        let mut names = SymbolNames::new();
        let id = names.intern("");
        assert_eq!(names.resolve(id), Some(""));
        assert_eq!(names.len(), 1);
    }

    /// The index is derivable, so it is not cached — but a table read back
    /// without repairing it would hand out duplicate IDs for names it already
    /// holds.
    #[test]
    fn a_deserialized_table_works_once_its_index_is_rebuilt() {
        let mut names = SymbolNames::new();
        let main = names.intern("_main");
        names.intern("_malloc");

        let json = serde_json::to_string(&names).expect("serializes");
        let mut back: SymbolNames = serde_json::from_str(&json).expect("deserializes");

        // Names survive, and resolution works immediately.
        assert_eq!(back.len(), 2);
        assert_eq!(back.resolve(main), Some("_main"));

        back.rebuild_index();
        assert_eq!(back.get("_main"), Some(main));
        // The decisive check: re-interning must not allocate a new ID.
        assert_eq!(back.intern("_main"), main);
        assert_eq!(back.len(), 2);
    }
}
