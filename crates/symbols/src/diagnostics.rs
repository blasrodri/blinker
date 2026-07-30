//! Symbol-resolution diagnostics.
//!
//! Spec §16 asks that a resolution diagnostic identify the symbol, every
//! competing definition, the selected one, the inputs involved, and the rule
//! applied. The types here carry that information; rendering it for a user is
//! the `diagnostics` crate's job.

use blinker_macho::ObjectId;

use crate::{Candidate, SymbolNameId};

/// A name defined strongly more than once.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DuplicateSymbol {
    pub name: SymbolNameId,
    /// Every definition offered, so the diagnostic can list the competitors
    /// rather than only naming the conflict.
    pub candidates: Vec<Candidate>,
}

/// A name referenced but never defined.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UndefinedSymbol {
    pub name: SymbolNameId,
    /// Objects that referenced it — "who wanted this" is usually the question
    /// a user actually needs answered.
    pub referenced_by: Vec<ObjectId>,
}

/// A resolution failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SymbolError {
    Duplicate(DuplicateSymbol),
    Undefined(UndefinedSymbol),
}

impl SymbolError {
    /// The name this error concerns.
    pub fn name(&self) -> SymbolNameId {
        match self {
            SymbolError::Duplicate(d) => d.name,
            SymbolError::Undefined(u) => u.name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SymbolProvider;
    use blinker_macho::{SymbolId, SymbolStrength};

    #[test]
    fn errors_expose_the_name_they_concern() {
        let duplicate = SymbolError::Duplicate(DuplicateSymbol {
            name: SymbolNameId(7),
            candidates: vec![Candidate {
                provider: SymbolProvider::Object {
                    object: ObjectId(0),
                    symbol: SymbolId(0),
                },
                strength: SymbolStrength::Strong,
            }],
        });
        assert_eq!(duplicate.name(), SymbolNameId(7));

        let undefined = SymbolError::Undefined(UndefinedSymbol {
            name: SymbolNameId(9),
            referenced_by: vec![ObjectId(1)],
        });
        assert_eq!(undefined.name(), SymbolNameId(9));
    }

    #[test]
    fn diagnostics_round_trip_through_serde() {
        let error = SymbolError::Undefined(UndefinedSymbol {
            name: SymbolNameId(1),
            referenced_by: vec![ObjectId(0), ObjectId(2)],
        });
        let json = serde_json::to_string(&error).expect("serializes");
        let back: SymbolError = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(error, back);
    }
}
