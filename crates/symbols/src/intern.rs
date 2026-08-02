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

use std::collections::HashMap;
use std::sync::Arc;

/// A interned symbol name.
///
/// Only meaningful relative to the [`SymbolNames`] table that produced it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct SymbolNameId(pub u32);

/// An interning table mapping names to [`SymbolNameId`]s and back.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolNames {
    /// Shared with `lookup`, so a name is allocated once rather than twice.
    ///
    /// Interning stored every name in both the vector and the map key. Half
    /// of a debug rust-analyzer link's 981,253 `intern` calls are misses, so
    /// that was 955,064 allocations where 477,532 will do — and interning is
    /// the whole cost of building the symbol table (finding 152).
    names: Vec<Arc<str>>,
    /// Reverse index. Rebuilt on deserialization rather than stored, since it
    /// is derivable and would double the cached size.
    #[serde(skip)]
    lookup: HashMap<Arc<str>, SymbolNameId>,
}

impl SymbolNames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `name`, returning its existing ID if already present.
    pub fn intern(&mut self, name: &str) -> SymbolNameId {
        if let Some(id) = self.lookup.get(name) {
            return *id;
        }
        let id = SymbolNameId(self.names.len() as u32);
        let shared: Arc<str> = Arc::from(name);
        self.names.push(Arc::clone(&shared));
        self.lookup.insert(shared, id);
        id
    }

    /// The name behind an ID.
    pub fn resolve(&self, id: SymbolNameId) -> Option<&str> {
        self.names.get(id.0 as usize).map(AsRef::as_ref)
    }

    /// Look up an existing name without interning it.
    pub fn get(&self, name: &str) -> Option<SymbolNameId> {
        self.lookup.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Rebuild the reverse index after deserialization.
    ///
    /// The index is skipped when serializing because it is derivable; a table
    /// read back from the cache must be repaired before `intern` or `get` will
    /// behave, and forgetting to call this would silently produce duplicate
    /// IDs for names already in the table.
    pub fn rebuild_index(&mut self) {
        self.lookup = self
            .names
            .iter()
            .enumerate()
            .map(|(index, name)| (Arc::clone(name), SymbolNameId(index as u32)))
            .collect();
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
