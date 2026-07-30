//! Parsing Apple `.tbd` text stubs.
//!
//! A `.tbd` describes a dynamic library's exported symbols without shipping the
//! library. Rust links against a handful of them — `libSystem`, `libc`, `libm`,
//! `libobjc`, `libiconv` — and the observed set is small enough that this
//! parses the TBD v4 subset those use rather than the whole format.
//!
//! # Two structural surprises, both load-bearing
//!
//! **A `.tbd` is a multi-document YAML file.** `libSystem.B.tbd` contains 40
//! documents in 3,760 lines: libSystem itself, then every library it
//! re-exports, inline. Treating the file as one document finds almost nothing —
//! libSystem's own `exports` block lists three symbols. `_malloc` and the rest
//! of libc live in the re-exported `libsystem_malloc.dylib` document further
//! down the same file.
//!
//! **Architecture matching cannot be exact.** libSystem declares no
//! `arm64-macos` target, only `arm64e-macos`, yet every arm64 Rust binary links
//! against it. See [`target`] for the rule and the evidence.
//!
//! Together these mean a naive reader — one document, exact target match —
//! would conclude that libSystem exports nothing usable, and every link would
//! fail with undefined symbols.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

pub mod target;
pub use target::{Architecture, Platform, Target};

mod error;
pub use error::TbdError;

/// A set of symbols exported for a particular set of targets.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolSet {
    pub targets: Vec<Target>,
    pub symbols: Vec<String>,
}

impl SymbolSet {
    /// Whether this set applies to a link for `wanted`.
    pub fn applies_to(&self, wanted: Target) -> bool {
        self.targets.iter().any(|t| t.satisfies(wanted))
    }
}

/// One `--- !tapi-tbd` document: a single dynamic library's stub.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TbdDocument {
    /// The library's install name, e.g. `/usr/lib/libSystem.B.dylib`. This is
    /// what gets recorded in the output's load commands.
    pub install_name: String,
    /// Targets this document as a whole is built for.
    pub targets: Vec<Target>,
    pub current_version: Option<String>,
    pub compatibility_version: Option<String>,
    /// Install names of libraries whose symbols this one re-exports. For
    /// libSystem these resolve to other documents in the same file.
    pub reexported_libraries: Vec<String>,
    /// Symbols this library defines.
    pub exports: Vec<SymbolSet>,
    /// Symbols re-exported from elsewhere, listed explicitly.
    pub reexports: Vec<SymbolSet>,
}

impl TbdDocument {
    /// Whether this document is built for `wanted`.
    pub fn applies_to(&self, wanted: Target) -> bool {
        self.targets.iter().any(|t| t.satisfies(wanted))
    }

    /// Symbols this document exports for `wanted`, ignoring re-exports.
    pub fn symbols_for(&self, wanted: Target) -> impl Iterator<Item = &str> {
        self.exports
            .iter()
            .chain(self.reexports.iter())
            .filter(move |set| set.applies_to(wanted))
            .flat_map(|set| set.symbols.iter().map(String::as_str))
    }
}

/// A parsed `.tbd` file: one or more documents.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TbdFile {
    pub path: PathBuf,
    pub documents: Vec<TbdDocument>,
}

impl TbdFile {
    /// The document describing the library this file is named for.
    ///
    /// By convention the umbrella library comes first; the rest are the
    /// libraries it re-exports.
    pub fn primary(&self) -> Option<&TbdDocument> {
        self.documents.first()
    }

    /// Find a document by install name.
    pub fn document(&self, install_name: &str) -> Option<&TbdDocument> {
        self.documents
            .iter()
            .find(|d| d.install_name == install_name)
    }

    /// Every symbol available to a link for `wanted`, following re-exports.
    ///
    /// This is the function that matters. Asking the primary document alone
    /// yields three symbols for libSystem; following its `reexported-libraries`
    /// into the sibling documents yields the several thousand a Rust binary
    /// actually needs.
    pub fn exported_symbols(&self, wanted: Target) -> BTreeSet<String> {
        let mut symbols = BTreeSet::new();
        let Some(primary) = self.primary() else {
            return symbols;
        };

        // Breadth-first over re-exports, guarding against cycles: the graph is
        // declared data and nothing guarantees it is acyclic.
        let mut visited = BTreeSet::new();
        let mut queue = vec![primary.install_name.clone()];

        while let Some(install_name) = queue.pop() {
            if !visited.insert(install_name.clone()) {
                continue;
            }
            let Some(document) = self.document(&install_name) else {
                // A re-export naming a library packaged in a different file.
                // Resolving it needs the library resolver, which is M3's job;
                // here it is simply not available.
                continue;
            };

            // A document that is not built for this target contributes nothing,
            // even if it is reachable.
            if !document.applies_to(wanted) {
                continue;
            }

            symbols.extend(document.symbols_for(wanted).map(str::to_string));
            queue.extend(document.reexported_libraries.iter().cloned());
        }
        symbols
    }

    /// Install names of every document in the file, for diagnostics.
    pub fn install_names(&self) -> impl Iterator<Item = &str> {
        self.documents.iter().map(|d| d.install_name.as_str())
    }
}

/// Parse a `.tbd` file's text.
pub fn parse_tbd(text: &str, path: &Path) -> Result<TbdFile, TbdError> {
    let documents = YamlLoader::load_from_str(text).map_err(|e| TbdError::Malformed {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;

    let mut parsed = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        // A document without an install-name is not a library stub — skip it
        // rather than failing, so an unfamiliar auxiliary document does not
        // take the whole file down.
        let Some(install_name) = string_field(document, "install-name") else {
            continue;
        };

        parsed.push(TbdDocument {
            install_name,
            targets: parse_targets(&document["targets"]),
            current_version: string_field(document, "current-version"),
            compatibility_version: string_field(document, "compatibility-version"),
            reexported_libraries: parse_reexported_libraries(&document["reexported-libraries"]),
            exports: parse_symbol_sets(&document["exports"]),
            reexports: parse_symbol_sets(&document["reexports"]),
        });

        if index > 10_000 {
            return Err(TbdError::Malformed {
                path: path.to_path_buf(),
                detail: "implausibly many documents".to_string(),
            });
        }
    }

    if parsed.is_empty() {
        return Err(TbdError::NoDocuments {
            path: path.to_path_buf(),
        });
    }

    Ok(TbdFile {
        path: path.to_path_buf(),
        documents: parsed,
    })
}

/// Read and parse a `.tbd` from disk.
pub fn parse_tbd_file(path: &Path) -> Result<TbdFile, TbdError> {
    let text = std::fs::read_to_string(path).map_err(|source| TbdError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_tbd(&text, path)
}

/// Read a scalar field, accepting both string and integer forms.
///
/// `current-version: 1356` is an integer in YAML but a version string to us.
fn string_field(document: &Yaml, key: &str) -> Option<String> {
    match &document[key] {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        _ => None,
    }
}

/// Parse a `targets:` list, dropping entries that are not well formed.
fn parse_targets(node: &Yaml) -> Vec<Target> {
    node.as_vec()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .filter_map(Target::parse)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `reexported-libraries:`, which is a list of `{targets, libraries}`.
fn parse_reexported_libraries(node: &Yaml) -> Vec<String> {
    let Some(entries) = node.as_vec() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries {
        let Some(libraries) = entry["libraries"].as_vec() else {
            continue;
        };
        names.extend(
            libraries
                .iter()
                .filter_map(|l| l.as_str())
                .map(str::to_string),
        );
    }
    names
}

/// Parse an `exports:` or `reexports:` block into symbol sets.
///
/// Each entry pairs a `targets` list with the symbols valid for it, so a single
/// library can export different symbols per architecture — which libSystem
/// does, with x86-only compatibility aliases.
fn parse_symbol_sets(node: &Yaml) -> Vec<SymbolSet> {
    let Some(entries) = node.as_vec() else {
        return Vec::new();
    };

    let mut sets = Vec::new();
    for entry in entries {
        let targets = parse_targets(&entry["targets"]);
        if targets.is_empty() {
            continue;
        }

        // `symbols` is the common case. Objective-C class and ivar entries are
        // parsed too: they are exported names like any other, and omitting
        // them would silently under-report what a library provides.
        let mut symbols = Vec::new();
        for key in ["symbols", "objc-classes", "objc-ivars", "weak-symbols"] {
            if let Some(list) = entry[key].as_vec() {
                symbols.extend(list.iter().filter_map(|s| s.as_str()).map(str::to_string));
            }
        }

        if !symbols.is_empty() {
            sets.push(SymbolSet { targets, symbols });
        }
    }
    sets
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature of libSystem's real structure: an umbrella whose own
    /// exports are nearly empty, with the useful symbols in a re-exported
    /// document in the same file, tagged arm64e only.
    const UMBRELLA: &str = r#"--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64e-macos ]
install-name:    '/usr/lib/libSystem.B.dylib'
current-version: 1356
reexported-libraries:
  - targets:         [ x86_64-macos, arm64e-macos ]
    libraries:       [ '/usr/lib/system/libsystem_malloc.dylib' ]
exports:
  - targets:         [ x86_64-macos, arm64e-macos ]
    symbols:         [ _mach_init_routine ]
--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64e-macos ]
install-name:    '/usr/lib/system/libsystem_malloc.dylib'
current-version: 1
parent-umbrella:
  - targets:         [ x86_64-macos, arm64e-macos ]
    umbrella:        System
exports:
  - targets:         [ x86_64-macos, arm64e-macos ]
    symbols:         [ _malloc, _free, _calloc ]
...
"#;

    fn parse(text: &str) -> TbdFile {
        parse_tbd(text, Path::new("/test.tbd")).expect("parses")
    }

    #[test]
    fn parses_every_document_in_a_multi_document_file() {
        let file = parse(UMBRELLA);
        assert_eq!(file.documents.len(), 2);
        assert_eq!(
            file.install_names().collect::<Vec<_>>(),
            vec![
                "/usr/lib/libSystem.B.dylib",
                "/usr/lib/system/libsystem_malloc.dylib"
            ]
        );
    }

    #[test]
    fn reads_document_metadata() {
        let file = parse(UMBRELLA);
        let primary = file.primary().expect("has a primary document");
        assert_eq!(primary.install_name, "/usr/lib/libSystem.B.dylib");
        // An integer in YAML, a version string to us.
        assert_eq!(primary.current_version.as_deref(), Some("1356"));
        assert_eq!(
            primary.reexported_libraries,
            vec!["/usr/lib/system/libsystem_malloc.dylib"]
        );
    }

    /// The central behaviour. Reading only the primary document finds one
    /// symbol; following re-exports finds the ones a link actually needs.
    #[test]
    fn following_reexports_is_what_makes_symbols_findable() {
        let file = parse(UMBRELLA);
        let wanted = Target::aarch64_macos();

        let direct: Vec<&str> = file
            .primary()
            .expect("primary")
            .symbols_for(wanted)
            .collect();
        assert_eq!(direct, vec!["_mach_init_routine"]);

        let all = file.exported_symbols(wanted);
        assert!(all.contains("_malloc"), "re-exported symbol not found");
        assert!(all.contains("_free"));
        assert!(all.contains("_mach_init_routine"));
        assert_eq!(all.len(), 4);
    }

    /// The architecture rule, end to end: nothing here is tagged `arm64-macos`,
    /// yet an arm64 link must find everything.
    #[test]
    fn an_arm64_link_resolves_against_arm64e_only_stubs() {
        let file = parse(UMBRELLA);
        let symbols = file.exported_symbols(Target::aarch64_macos());
        assert!(
            symbols.contains("_malloc"),
            "arm64 link found nothing in an arm64e-tagged stub"
        );
    }

    #[test]
    fn a_link_for_an_unrelated_architecture_finds_nothing() {
        let file = parse(UMBRELLA);
        let wanted = Target {
            architecture: Architecture::Other,
            platform: Platform::MacOs,
        };
        assert!(file.exported_symbols(wanted).is_empty());
    }

    #[test]
    fn per_target_symbol_sets_are_filtered() {
        // libSystem really does this: x86-only compatibility aliases that an
        // arm64 link must not see.
        let text = r#"--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64e-macos ]
install-name:    '/usr/lib/libTest.dylib'
exports:
  - targets:         [ x86_64-macos ]
    symbols:         [ _x86_only ]
  - targets:         [ x86_64-macos, arm64e-macos ]
    symbols:         [ _everywhere ]
"#;
        let file = parse(text);
        let symbols = file.exported_symbols(Target::aarch64_macos());
        assert!(symbols.contains("_everywhere"));
        assert!(
            !symbols.contains("_x86_only"),
            "an x86-only symbol leaked into an arm64 link"
        );
    }

    #[test]
    fn objc_classes_are_treated_as_exported_names() {
        let text = r#"--- !tapi-tbd
tbd-version:     4
targets:         [ arm64e-macos ]
install-name:    '/usr/lib/libobjc.A.dylib'
exports:
  - targets:         [ arm64e-macos ]
    objc-classes:    [ NSObject, Protocol ]
    symbols:         [ _objc_msgSend ]
"#;
        let file = parse(text);
        let symbols = file.exported_symbols(Target::aarch64_macos());
        assert!(symbols.contains("NSObject"));
        assert!(symbols.contains("_objc_msgSend"));
    }

    #[test]
    fn a_reexport_cycle_terminates() {
        // The re-export graph is declared data; nothing guarantees it is
        // acyclic, and a naive walk would hang.
        let text = r#"--- !tapi-tbd
tbd-version:     4
targets:         [ arm64e-macos ]
install-name:    '/usr/lib/a.dylib'
reexported-libraries:
  - targets:         [ arm64e-macos ]
    libraries:       [ '/usr/lib/b.dylib' ]
exports:
  - targets:         [ arm64e-macos ]
    symbols:         [ _a ]
--- !tapi-tbd
tbd-version:     4
targets:         [ arm64e-macos ]
install-name:    '/usr/lib/b.dylib'
reexported-libraries:
  - targets:         [ arm64e-macos ]
    libraries:       [ '/usr/lib/a.dylib' ]
exports:
  - targets:         [ arm64e-macos ]
    symbols:         [ _b ]
"#;
        let file = parse(text);
        let symbols = file.exported_symbols(Target::aarch64_macos());
        assert_eq!(symbols.len(), 2);
        assert!(symbols.contains("_a") && symbols.contains("_b"));
    }

    #[test]
    fn a_reexport_naming_a_library_in_another_file_is_skipped_not_fatal() {
        // Resolving it needs the library resolver, which is M3's job.
        let text = r#"--- !tapi-tbd
tbd-version:     4
targets:         [ arm64e-macos ]
install-name:    '/usr/lib/a.dylib'
reexported-libraries:
  - targets:         [ arm64e-macos ]
    libraries:       [ '/usr/lib/elsewhere.dylib' ]
exports:
  - targets:         [ arm64e-macos ]
    symbols:         [ _a ]
"#;
        let file = parse(text);
        assert_eq!(file.exported_symbols(Target::aarch64_macos()).len(), 1);
    }

    #[test]
    fn rejects_text_that_contains_no_library_documents() {
        for text in ["", "not: a stub\n", "--- !tapi-tbd\ntbd-version: 4\n"] {
            assert!(
                parse_tbd(text, Path::new("/x.tbd")).is_err(),
                "{text:?} should not yield a usable stub"
            );
        }
    }

    #[test]
    fn rejects_malformed_yaml() {
        let err = parse_tbd("--- !tapi-tbd\n  [unclosed", Path::new("/x.tbd")).unwrap_err();
        assert!(matches!(err, TbdError::Malformed { .. }));
    }

    #[test]
    fn round_trips_through_serde() {
        let file = parse(UMBRELLA);
        let json = serde_json::to_string(&file).expect("serializes");
        let back: TbdFile = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(file, back);
    }
}
