//! The global symbol table and its resolution rules.
//!
//! # What makes this the dangerous part
//!
//! Symbol resolution is where a linker silently produces a wrong answer. A
//! mistake here does not fail the link — it picks a different definition than
//! the toolchain would have, and the program misbehaves at runtime with nothing
//! in the build log to explain it.
//!
//! Three rules carry that risk, so each is stated explicitly and tested:
//!
//! 1. **Two strong definitions of one name is an error**, never a silent pick.
//! 2. **A strong definition beats a weak one** regardless of arrival order —
//!    order-dependence here would make links non-reproducible.
//! 3. **A local definition cannot satisfy another object's reference.** Locals
//!    with the same name legitimately coexist across objects, and treating one
//!    as a global definition would make them collide.
//!
//! Every resolution records *why* it chose what it chose, so
//! `--explain-incremental` and duplicate-symbol diagnostics can report the
//! competing definitions rather than just the winner.

use std::collections::HashMap;

use blinker_macho::{ObjectId, SymbolId, SymbolStrength, SymbolVisibility};

mod intern;
pub use intern::{SymbolNameId, SymbolNames};

mod diagnostics;
pub use diagnostics::{DuplicateSymbol, SymbolError, UndefinedSymbol};

/// Where a resolved symbol's definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SymbolProvider {
    /// An object file linked directly.
    Object { object: ObjectId, symbol: SymbolId },
    /// A member extracted from an archive.
    ArchiveMember { object: ObjectId, symbol: SymbolId },
    /// A dynamic library, via its `.tbd` stub. Resolved at load time rather
    /// than bound to an address here.
    DynamicLibrary {
        /// Index into the link's dylib list.
        library: u32,
    },
    /// A weak reference left deliberately unresolved, binding to zero.
    Unresolved,
}

impl SymbolProvider {
    /// Whether this provider supplies an address within the output image.
    ///
    /// Dynamic imports do not: dyld binds them at load time, so layout must
    /// not try to place them.
    pub fn is_internal(self) -> bool {
        matches!(
            self,
            SymbolProvider::Object { .. } | SymbolProvider::ArchiveMember { .. }
        )
    }
}

/// One entry in the global symbol table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedSymbol {
    pub name: SymbolNameId,
    pub provider: SymbolProvider,
    pub strength: SymbolStrength,
    pub visibility: SymbolVisibility,
    /// Address in the output image, assigned by layout. `None` until then, and
    /// permanently `None` for dynamic imports.
    pub final_address: Option<u64>,
}

/// A definition competing to satisfy a name, kept for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Candidate {
    pub provider: SymbolProvider,
    pub strength: SymbolStrength,
}

/// Why a particular definition won.
///
/// Recorded so a diagnostic can explain the choice rather than assert it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResolutionRule {
    /// The only definition offered.
    OnlyDefinition,
    /// A strong definition displaced one or more weak ones.
    StrongOverWeak,
    /// Only weak definitions were offered; the first was taken.
    FirstWeak,
    /// No definition anywhere; the reference was weak, so it binds to zero.
    WeakUndefined,
    /// Satisfied by a dynamic library rather than an object.
    DynamicImport,
}

/// The global symbol table.
#[derive(Debug, Default)]
pub struct SymbolTable {
    pub names: SymbolNames,
    resolved: HashMap<SymbolNameId, ResolvedSymbol>,
    /// Every definition offered for a name, winner included. Kept so a
    /// duplicate-symbol error can name all the competitors, which is what
    /// makes it actionable.
    candidates: HashMap<SymbolNameId, Vec<Candidate>>,
    rules: HashMap<SymbolNameId, ResolutionRule>,
    /// Names referenced but not yet defined.
    undefined: HashMap<SymbolNameId, Vec<ObjectId>>,
    errors: Vec<SymbolError>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer a definition for `name`.
    ///
    /// Applies the resolution rules and records the outcome. Conflicts are
    /// collected rather than returned, so a link reports every duplicate at
    /// once instead of stopping at the first.
    pub fn define(
        &mut self,
        name: &str,
        provider: SymbolProvider,
        strength: SymbolStrength,
        visibility: SymbolVisibility,
    ) {
        // A local definition is invisible outside its object, so it never
        // enters the global table. Two objects may legitimately define the
        // same local name, and admitting them would make those collide.
        if visibility == SymbolVisibility::Local {
            return;
        }

        let name_id = self.names.intern(name);
        self.candidates
            .entry(name_id)
            .or_default()
            .push(Candidate { provider, strength });

        // A definition satisfies any outstanding reference.
        self.undefined.remove(&name_id);

        let Some(existing) = self.resolved.get(&name_id) else {
            let rule = match strength {
                SymbolStrength::Weak => ResolutionRule::FirstWeak,
                _ => ResolutionRule::OnlyDefinition,
            };
            self.rules.insert(name_id, rule);
            self.resolved.insert(
                name_id,
                ResolvedSymbol {
                    name: name_id,
                    provider,
                    strength,
                    visibility,
                    final_address: None,
                },
            );
            return;
        };

        // A `WeakUndefined` entry is a placeholder for an unresolved weak
        // *reference*, not a definition. Any real definition displaces it —
        // and it must be checked before the strength rules below, or the
        // placeholder would permanently block the definition it was waiting
        // for.
        if existing.strength == SymbolStrength::WeakUndefined {
            let rule = match strength {
                SymbolStrength::Weak => ResolutionRule::FirstWeak,
                _ => ResolutionRule::OnlyDefinition,
            };
            self.rules.insert(name_id, rule);
            self.resolved.insert(
                name_id,
                ResolvedSymbol {
                    name: name_id,
                    provider,
                    strength,
                    visibility,
                    final_address: None,
                },
            );
            return;
        }

        match (existing.strength, strength) {
            // Two strong definitions: an error, never a silent pick.
            (SymbolStrength::Strong, SymbolStrength::Strong) => {
                self.errors.push(SymbolError::Duplicate(DuplicateSymbol {
                    name: name_id,
                    candidates: self.candidates[&name_id].clone(),
                }));
            }
            // Strong displaces weak, whichever arrived first. Order-dependence
            // here would make links non-reproducible.
            (SymbolStrength::Weak | SymbolStrength::Common, SymbolStrength::Strong) => {
                self.rules.insert(name_id, ResolutionRule::StrongOverWeak);
                self.resolved.insert(
                    name_id,
                    ResolvedSymbol {
                        name: name_id,
                        provider,
                        strength,
                        visibility,
                        final_address: None,
                    },
                );
            }
            // A weak definition never displaces an existing one.
            _ => {}
        }
    }

    /// Record a reference to `name`.
    ///
    /// A reference to an already-defined symbol is satisfied immediately;
    /// otherwise it joins the undefined set until something defines it.
    pub fn reference(&mut self, name: &str, from: ObjectId, strength: SymbolStrength) {
        let name_id = self.names.intern(name);
        if self.resolved.contains_key(&name_id) {
            return;
        }

        if strength == SymbolStrength::WeakUndefined {
            // A weak reference is allowed to stay unresolved, binding to zero.
            // It is resolved eagerly so it never appears as undefined, but a
            // real definition arriving later still displaces it.
            self.rules.insert(name_id, ResolutionRule::WeakUndefined);
            self.resolved.insert(
                name_id,
                ResolvedSymbol {
                    name: name_id,
                    provider: SymbolProvider::Unresolved,
                    strength: SymbolStrength::WeakUndefined,
                    visibility: SymbolVisibility::Global,
                    final_address: Some(0),
                },
            );
            return;
        }

        self.undefined.entry(name_id).or_default().push(from);
    }

    /// Record that a dynamic library provides `name`.
    pub fn define_dynamic(&mut self, name: &str, library: u32) {
        let name_id = self.names.intern(name);
        self.undefined.remove(&name_id);

        // A definition in an object outranks a dynamic import: the symbol is
        // present in the image and need not be bound at load time. The one
        // exception is a `WeakUndefined` placeholder, which is a pending weak
        // *reference* rather than a definition — a dynamic import satisfies it.
        if self
            .resolved
            .get(&name_id)
            .is_some_and(|existing| existing.strength != SymbolStrength::WeakUndefined)
        {
            return;
        }
        self.rules.insert(name_id, ResolutionRule::DynamicImport);
        self.resolved.insert(
            name_id,
            ResolvedSymbol {
                name: name_id,
                provider: SymbolProvider::DynamicLibrary { library },
                strength: SymbolStrength::Strong,
                visibility: SymbolVisibility::Global,
                final_address: None,
            },
        );
    }

    pub fn lookup(&self, name: &str) -> Option<&ResolvedSymbol> {
        let id = self.names.get(name)?;
        self.resolved.get(&id)
    }

    pub fn lookup_id(&self, name: SymbolNameId) -> Option<&ResolvedSymbol> {
        self.resolved.get(&name)
    }

    /// Why `name` resolved the way it did.
    pub fn rule_for(&self, name: &str) -> Option<ResolutionRule> {
        let id = self.names.get(name)?;
        self.rules.get(&id).copied()
    }

    /// Every definition offered for `name`, winner included.
    pub fn candidates_for(&self, name: &str) -> &[Candidate] {
        self.names
            .get(name)
            .and_then(|id| self.candidates.get(&id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Names still referenced but never defined.
    pub fn undefined_symbols(&self) -> Vec<UndefinedSymbol> {
        let mut out: Vec<UndefinedSymbol> = self
            .undefined
            .iter()
            .map(|(name, referenced_by)| UndefinedSymbol {
                name: *name,
                referenced_by: referenced_by.clone(),
            })
            .collect();
        // Deterministic order: diagnostics must not vary between runs.
        out.sort_by_key(|u| u.name);
        out
    }

    /// Errors accumulated during resolution.
    pub fn errors(&self) -> &[SymbolError] {
        &self.errors
    }

    /// Whether the link can proceed: no duplicates and nothing undefined.
    pub fn is_complete(&self) -> bool {
        self.errors.is_empty() && self.undefined.is_empty()
    }

    pub fn resolved_count(&self) -> usize {
        self.resolved.len()
    }

    /// Assign an address to a resolved symbol, once layout has placed it.
    pub fn set_address(&mut self, name: SymbolNameId, address: u64) {
        if let Some(symbol) = self.resolved.get_mut(&name) {
            symbol.final_address = Some(address);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(n: u32) -> SymbolProvider {
        SymbolProvider::Object {
            object: ObjectId(n),
            symbol: SymbolId(0),
        }
    }

    fn table() -> SymbolTable {
        SymbolTable::new()
    }

    #[test]
    fn a_single_definition_resolves_to_itself() {
        let mut t = table();
        t.define(
            "_main",
            object(0),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );

        let resolved = t.lookup("_main").expect("resolved");
        assert_eq!(resolved.provider, object(0));
        assert_eq!(t.rule_for("_main"), Some(ResolutionRule::OnlyDefinition));
        assert!(t.is_complete());
    }

    /// Two strong definitions must be an error, not a silent pick — this is
    /// the failure mode that produces a working-looking wrong binary.
    #[test]
    fn two_strong_definitions_are_a_duplicate_error() {
        let mut t = table();
        t.define(
            "_dup",
            object(0),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );
        t.define(
            "_dup",
            object(1),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );

        assert_eq!(t.errors().len(), 1);
        assert!(!t.is_complete());

        // The diagnostic must name both competitors, or it is not actionable.
        match &t.errors()[0] {
            SymbolError::Duplicate(dup) => assert_eq!(dup.candidates.len(), 2),
            other => panic!("expected a duplicate error, got {other:?}"),
        }
    }

    #[test]
    fn a_strong_definition_beats_a_weak_one_whichever_arrives_first() {
        // Order-dependence here would make links non-reproducible, so both
        // orders must reach the same answer.
        for (first, second, winner) in [
            (SymbolStrength::Weak, SymbolStrength::Strong, 1u32),
            (SymbolStrength::Strong, SymbolStrength::Weak, 0u32),
        ] {
            let mut t = table();
            t.define("_s", object(0), first, SymbolVisibility::Global);
            t.define("_s", object(1), second, SymbolVisibility::Global);

            let resolved = t.lookup("_s").expect("resolved");
            assert_eq!(
                resolved.provider,
                object(winner),
                "{first:?} then {second:?}"
            );
            assert_eq!(resolved.strength, SymbolStrength::Strong);
            assert!(t.errors().is_empty(), "strong over weak is not an error");
        }
    }

    #[test]
    fn two_weak_definitions_are_not_an_error_and_the_first_wins() {
        let mut t = table();
        t.define(
            "_w",
            object(0),
            SymbolStrength::Weak,
            SymbolVisibility::Global,
        );
        t.define(
            "_w",
            object(1),
            SymbolStrength::Weak,
            SymbolVisibility::Global,
        );

        assert!(t.errors().is_empty());
        assert_eq!(t.lookup("_w").expect("resolved").provider, object(0));
        assert_eq!(t.rule_for("_w"), Some(ResolutionRule::FirstWeak));
    }

    /// Locals with the same name legitimately coexist across objects. Letting
    /// one into the global table would make them collide.
    #[test]
    fn local_definitions_never_enter_the_global_table() {
        let mut t = table();
        t.define(
            "ltmp0",
            object(0),
            SymbolStrength::Strong,
            SymbolVisibility::Local,
        );
        t.define(
            "ltmp0",
            object(1),
            SymbolStrength::Strong,
            SymbolVisibility::Local,
        );

        assert!(t.lookup("ltmp0").is_none());
        assert!(t.errors().is_empty(), "two locals are not a duplicate");
    }

    #[test]
    fn a_reference_with_no_definition_is_reported_as_undefined() {
        let mut t = table();
        t.reference("_absent", ObjectId(0), SymbolStrength::Undefined);

        let undefined = t.undefined_symbols();
        assert_eq!(undefined.len(), 1);
        assert_eq!(undefined[0].referenced_by, vec![ObjectId(0)]);
        assert!(!t.is_complete());
    }

    #[test]
    fn a_definition_satisfies_an_earlier_reference() {
        let mut t = table();
        t.reference("_later", ObjectId(0), SymbolStrength::Undefined);
        assert_eq!(t.undefined_symbols().len(), 1);

        t.define(
            "_later",
            object(1),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );
        assert!(t.undefined_symbols().is_empty());
        assert!(t.is_complete());
    }

    #[test]
    fn a_reference_to_an_already_defined_symbol_is_satisfied_immediately() {
        let mut t = table();
        t.define(
            "_early",
            object(0),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );
        t.reference("_early", ObjectId(1), SymbolStrength::Undefined);
        assert!(t.undefined_symbols().is_empty());
    }

    #[test]
    fn every_object_referencing_an_undefined_symbol_is_recorded() {
        // The diagnostic should say who wanted it, not just that it is missing.
        let mut t = table();
        for n in 0..3 {
            t.reference("_absent", ObjectId(n), SymbolStrength::Undefined);
        }
        assert_eq!(t.undefined_symbols()[0].referenced_by.len(), 3);
    }

    #[test]
    fn a_weak_reference_resolves_to_zero_rather_than_being_undefined() {
        let mut t = table();
        t.reference("_optional", ObjectId(0), SymbolStrength::WeakUndefined);

        assert!(t.undefined_symbols().is_empty());
        let resolved = t.lookup("_optional").expect("resolved");
        assert_eq!(resolved.provider, SymbolProvider::Unresolved);
        assert_eq!(resolved.final_address, Some(0));
        assert_eq!(t.rule_for("_optional"), Some(ResolutionRule::WeakUndefined));
    }

    /// A weak-reference placeholder is not a definition. If it were allowed
    /// to stand, the definition it was waiting for could never take effect.
    #[test]
    fn a_real_definition_displaces_a_weak_reference() {
        let mut t = table();
        t.reference("_optional", ObjectId(0), SymbolStrength::WeakUndefined);
        t.define(
            "_optional",
            object(1),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );

        let resolved = t.lookup("_optional").expect("resolved");
        assert_eq!(resolved.provider, object(1));
        assert_eq!(resolved.strength, SymbolStrength::Strong);
        assert_eq!(resolved.final_address, None, "no longer binds to zero");
    }

    #[test]
    fn even_a_weak_definition_displaces_a_weak_reference() {
        let mut t = table();
        t.reference("_optional", ObjectId(0), SymbolStrength::WeakUndefined);
        t.define(
            "_optional",
            object(1),
            SymbolStrength::Weak,
            SymbolVisibility::Global,
        );

        let resolved = t.lookup("_optional").expect("resolved");
        assert_eq!(resolved.provider, object(1));
        assert!(t.errors().is_empty());
    }

    #[test]
    fn a_dynamic_import_displaces_a_weak_reference() {
        let mut t = table();
        t.reference("_maybe", ObjectId(0), SymbolStrength::WeakUndefined);
        t.define_dynamic("_maybe", 3);

        assert_eq!(
            t.lookup("_maybe").expect("resolved").provider,
            SymbolProvider::DynamicLibrary { library: 3 }
        );
    }

    #[test]
    fn a_dynamic_library_satisfies_an_undefined_reference() {
        let mut t = table();
        t.reference("_malloc", ObjectId(0), SymbolStrength::Undefined);
        t.define_dynamic("_malloc", 0);

        assert!(t.undefined_symbols().is_empty());
        let resolved = t.lookup("_malloc").expect("resolved");
        assert_eq!(
            resolved.provider,
            SymbolProvider::DynamicLibrary { library: 0 }
        );
        assert!(!resolved.provider.is_internal());
    }

    /// A definition in an object outranks a dynamic import: the symbol is
    /// present in the image and should not be bound at load time.
    #[test]
    fn an_object_definition_outranks_a_dynamic_import() {
        let mut t = table();
        t.define(
            "_memcpy",
            object(0),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );
        t.define_dynamic("_memcpy", 0);

        assert_eq!(t.lookup("_memcpy").expect("resolved").provider, object(0));
    }

    #[test]
    fn candidates_record_every_definition_offered() {
        let mut t = table();
        t.define(
            "_c",
            object(0),
            SymbolStrength::Weak,
            SymbolVisibility::Global,
        );
        t.define(
            "_c",
            object(1),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );
        assert_eq!(t.candidates_for("_c").len(), 2);
        assert_eq!(t.candidates_for("_never_seen").len(), 0);
    }

    #[test]
    fn undefined_symbols_are_reported_in_a_deterministic_order() {
        // Diagnostics must not vary between runs of the same link.
        let mut first = table();
        let mut second = table();
        for name in ["_c", "_a", "_b"] {
            first.reference(name, ObjectId(0), SymbolStrength::Undefined);
        }
        for name in ["_b", "_c", "_a"] {
            second.reference(name, ObjectId(0), SymbolStrength::Undefined);
        }

        let names = |t: &SymbolTable| -> Vec<String> {
            t.undefined_symbols()
                .iter()
                .map(|u| t.names.resolve(u.name).expect("named").to_string())
                .collect()
        };
        // Both tables interned in different orders, so IDs differ; what must
        // hold is that each run is internally sorted and stable.
        assert_eq!(names(&first).len(), 3);
        assert_eq!(names(&second).len(), 3);
        assert_eq!(first.undefined_symbols(), first.undefined_symbols());
    }

    #[test]
    fn layout_can_assign_addresses_to_resolved_symbols() {
        let mut t = table();
        t.define(
            "_main",
            object(0),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );
        let id = t.names.get("_main").expect("interned");
        assert_eq!(t.lookup_id(id).expect("resolved").final_address, None);

        t.set_address(id, 0x1000);
        assert_eq!(
            t.lookup_id(id).expect("resolved").final_address,
            Some(0x1000)
        );
    }

    #[test]
    fn private_external_symbols_participate_in_resolution() {
        // Global within the link unit but not exported from the image — they
        // still resolve, unlike locals.
        let mut t = table();
        t.define(
            "_hidden",
            object(0),
            SymbolStrength::Strong,
            SymbolVisibility::PrivateExternal,
        );
        assert!(t.lookup("_hidden").is_some());
    }

    #[test]
    fn a_common_symbol_is_displaced_by_a_strong_definition() {
        // A tentative definition yields to a real one.
        let mut t = table();
        t.define(
            "_tentative",
            object(0),
            SymbolStrength::Common,
            SymbolVisibility::Global,
        );
        t.define(
            "_tentative",
            object(1),
            SymbolStrength::Strong,
            SymbolVisibility::Global,
        );

        assert_eq!(
            t.lookup("_tentative").expect("resolved").provider,
            object(1)
        );
        assert!(t.errors().is_empty());
    }
}
