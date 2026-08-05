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
pub fn hash_of(name: &str) -> u64 {
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
        self.intern_hashed(name, hash_of(name))
    }

    /// Intern `name`, whose [`hash_of`] the caller has already worked out.
    ///
    /// Interning splits into two halves that want opposite things. Hashing a
    /// name reads sixty-odd bytes and touches nothing shared, so it belongs on
    /// every core; the table probe that follows mutates one structure and has
    /// to be serial. Passing the hash in is what lets a caller separate them —
    /// hash a whole round's names in parallel, then walk them through the table
    /// with no hashing left to do.
    ///
    /// `hash` is not checked against `name`. A wrong one does not corrupt the
    /// table — the text comparison below still decides — it just files the name
    /// where no lookup will find it, so the same name would intern twice.
    /// One probe of the index, not two.
    ///
    /// This asked `get_hashed` first and then `entry` on the miss, which is two
    /// walks of the same bucket for every name the table has not seen — and on
    /// a cold link that is *every* name. `entry` answers both questions at
    /// once: the occupied case still compares the text, so a name that is
    /// genuinely present is returned exactly as before, and the vacant case
    /// skips a lookup that could only ever miss.
    ///
    /// It reads as a micro-optimisation and is not. Filing a round's new names
    /// is the serial half of interning, by construction, and interning is 43%
    /// of read-and-parse on a 552-input C++ link (finding 227).
    pub fn intern_hashed(&mut self, name: &str, hash: u64) -> SymbolNameId {
        // Destructured so the text comparison can borrow the arena while the
        // entry holds the index. They are separate fields; only the compiler
        // needed telling.
        let Self {
            arena,
            spans,
            lookup,
        } = self;
        let file = |arena: &mut Vec<u8>, spans: &mut Vec<(u32, u32)>| {
            let id = SymbolNameId(spans.len() as u32);
            let start = arena.len() as u32;
            arena.extend_from_slice(name.as_bytes());
            spans.push((start, name.len() as u32));
            id
        };
        match lookup.entry(hash) {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                let same = |id: &SymbolNameId| {
                    spans
                        .get(id.0 as usize)
                        .and_then(|&(start, len)| arena.get(start as usize..(start + len) as usize))
                        == Some(name.as_bytes())
                };
                if let Some(id) = held.get().all().find(same) {
                    return id;
                }
                let id = file(arena, spans);
                held.get_mut().push(id);
                id
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                let id = file(arena, spans);
                slot.insert(crate::Few::new(id));
                id
            }
        }
    }

    /// Make room for `names` more names holding `bytes` of text between them.
    ///
    /// # Why a caller can know this and the table cannot
    ///
    /// Filing a round's new names is serial, and every insert lands on a random
    /// bucket of a table too large for any cache — so it is memory latency, not
    /// instructions. Growing by doubling from empty pays that latency again for
    /// every name already filed, once per doubling: reaching 622,648 names costs
    /// roughly another 622,648 random inserts in rehashes nobody asked for, and
    /// on a 552-input C++ link that was most of the 58 ms (finding 227).
    ///
    /// The table cannot see this coming. The caller can: the parallel probe that
    /// precedes the serial pass has already asked, of every name in the round,
    /// whether the table holds it — so the count of misses is in hand before the
    /// first insert. It is an over-estimate by the names the round repeats
    /// within itself, which is the harmless direction for a reservation.
    pub fn reserve(&mut self, names: usize, bytes: usize) {
        self.arena.reserve(bytes);
        self.spans.reserve(names);
        self.lookup.reserve(names);
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

    /// Look up an already-hashed name without interning it.
    ///
    /// The half of [`SymbolNames::intern_hashed`] that touches nothing. Answering
    /// it is three loads that each have to wait for the last — the bucket, the
    /// span it names, and the arena text the span points at — and a loop that
    /// interned as it went could not start one name's chain until the previous
    /// name's insert had finished with the table. Split out, a caller can ask
    /// this of a whole batch at once, on every core, and be left with only the
    /// names that were new to file away in order.
    pub fn get_hashed(&self, name: &str, hash: u64) -> Option<SymbolNameId> {
        let found = self.lookup.get(&hash)?;
        // The common case is one id; distinct names that hash alike are told
        // apart here rather than by trusting the hash.
        found.all().find(|id| self.text(*id) == Some(name))
    }

    /// Look up an existing name without interning it.
    pub fn get(&self, name: &str) -> Option<SymbolNameId> {
        self.get_hashed(name, hash_of(name))
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

    /// Interning with the hash worked out elsewhere must be indistinguishable
    /// from interning without it — the whole point is that a caller can hash a
    /// batch of names ahead of the walk that files them.
    #[test]
    fn interning_with_a_precomputed_hash_matches_interning_without() {
        let (mut ahead, mut plain) = (SymbolNames::new(), SymbolNames::new());
        for name in ["_main", "_malloc", "", "_main", "_free", "_malloc"] {
            assert_eq!(
                ahead.intern_hashed(name, hash_of(name)),
                plain.intern(name),
                "{name}"
            );
        }
        assert_eq!(ahead.len(), plain.len());
        assert_eq!(ahead, plain);
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
