//! Finding the dynamic libraries a command line asks for.
//!
//! A link names its dynamic dependencies three ways: `-l<name>`, `-framework
//! <name>`, and a path spelled out in full. The first two are *requests* — the
//! name is resolved against a search path, and which file answers it depends
//! on where the SDK is and what `-L` and `-F` were passed.
//!
//! # Why this did not exist
//!
//! blinker linked one program for its whole life: itself. A pure-Rust binary
//! whose only dynamic dependency is `libSystem`, which the SDK keeps at a
//! fixed path — so a single hardcoded lookup was indistinguishable from a
//! working library search, and stayed that way until the linker was pointed at
//! a second program (finding 138).
//!
//! # What a name resolves to
//!
//! In the order `ld` uses, and stopping at the first hit:
//!
//! - `-l<name>` -> `lib<name>.tbd`, then `lib<name>.dylib`, then
//!   `lib<name>.a`, in each `-L` directory and then in the SDK's `usr/lib`.
//! - `-framework <name>` -> `<name>.framework/<name>.tbd`, then
//!   `<name>.framework/<name>`, in each `-F` directory and then in the SDK's
//!   framework directories.
//!
//! The `.tbd` is preferred over the library it describes because it is what
//! the SDK actually ships: `CoreFoundation.framework` in the SDK contains a
//! `CoreFoundation.tbd` and no Mach-O at all.

use std::path::{Path, PathBuf};

/// Extensions a `-l<name>` request may resolve to, most preferred first.
const LIBRARY_SUFFIXES: [&str; 3] = ["tbd", "dylib", "a"];

/// Where the SDK keeps frameworks, relative to its root.
const FRAMEWORK_DIRECTORIES: [&str; 2] = ["System/Library/Frameworks", "Library/Frameworks"];

/// A dynamic library this link imports from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRequest {
    /// How it was named on the command line, for error messages.
    pub requested: String,
    pub path: PathBuf,
}

/// Resolve `-l<name>` against the search paths and then the SDK.
pub fn find_library(name: &str, search: &[PathBuf], sdk: Option<&Path>) -> Option<PathBuf> {
    let file = format!("lib{name}");
    let mut directories: Vec<PathBuf> = search.to_vec();
    if let Some(sdk) = sdk {
        directories.push(sdk.join("usr/lib"));
        directories.push(sdk.join("usr/local/lib"));
    }
    first_match(&directories, |directory| {
        LIBRARY_SUFFIXES
            .iter()
            .map(|suffix| directory.join(format!("{file}.{suffix}")))
            .find(|path| path.is_file())
    })
}

/// Resolve `-framework <name>` against the search paths and then the SDK.
///
/// A framework is a directory, and the library inside it has no extension —
/// `CoreFoundation.framework/CoreFoundation`. The SDK ships a `.tbd` beside
/// where that binary would be, and that is what a link against the SDK reads.
pub fn find_framework(name: &str, search: &[PathBuf], sdk: Option<&Path>) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = search.to_vec();
    if let Some(sdk) = sdk {
        directories.extend(FRAMEWORK_DIRECTORIES.iter().map(|d| sdk.join(d)));
    }
    first_match(&directories, |directory| {
        let bundle = directory.join(format!("{name}.framework"));
        [
            bundle.join(format!("{name}.tbd")),
            bundle.join(name),
            bundle.join(format!("Versions/A/{name}.tbd")),
            bundle.join(format!("Versions/A/{name}")),
        ]
        .into_iter()
        .find(|path| path.is_file())
    })
}

fn first_match(
    directories: &[PathBuf],
    mut probe: impl FnMut(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    directories.iter().find_map(|directory| probe(directory))
}

/// What the stub libraries export, and which one exports each name.
///
/// The ownership half is not decoration. Mach-O's two-level namespace records,
/// for every imported symbol, the *ordinal of the library it came from*, and
/// dyld looks in that library and nowhere else. A link that resolves
/// `_CFRelease` against CoreFoundation but records libSystem's ordinal
/// produces a binary that links cleanly and fails to launch — so "which
/// library" has to be carried from resolution all the way to the bind
/// opcodes, and cannot be recovered later from the name alone.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StubExports {
    /// Install names, in the order the command line named the libraries.
    libraries: Vec<String>,
    /// Symbol -> index into `libraries`.
    ///
    /// First definition wins, which is the search order: a name exported by
    /// two libraries binds to the one that came first, as `ld` does it.
    owners: std::collections::BTreeMap<String, u16>,
}

impl StubExports {
    /// Add a library and return its index, reusing one already present.
    pub fn library(&mut self, install_name: &str) -> u16 {
        if let Some(index) = self.libraries.iter().position(|n| n == install_name) {
            return index as u16;
        }
        self.libraries.push(install_name.to_string());
        (self.libraries.len() - 1) as u16
    }

    /// Record that `library` exports `name`, unless something already does.
    pub fn export(&mut self, library: u16, name: String) {
        self.owners.entry(name).or_insert(library);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.owners.contains_key(name)
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// How many names are importable.
    pub fn count(&self) -> usize {
        self.owners.len()
    }

    /// The one-based `LC_LOAD_DYLIB` ordinal to bind `name` against.
    ///
    /// Falls back to the first library rather than to zero: zero means
    /// flat-namespace lookup, which would turn a bug here into a program that
    /// searches every library and usually still works — exactly the kind of
    /// failure that hides.
    pub fn ordinal(&self, name: &str) -> u8 {
        let index = self.owners.get(name).copied().unwrap_or(0);
        u8::try_from(index + 1).unwrap_or(1)
    }

    /// The libraries, in the order their ordinals number them.
    pub fn install_names(&self) -> &[String] {
        &self.libraries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk() -> Option<PathBuf> {
        crate::sdk_root()
    }

    #[test]
    fn the_sdk_answers_the_libraries_every_rust_link_asks_for() {
        let Some(sdk) = sdk() else {
            return;
        };
        for name in ["System", "c", "m", "iconv"] {
            assert!(
                find_library(name, &[], Some(&sdk)).is_some(),
                "-l{name} did not resolve under {}",
                sdk.display()
            );
        }
    }

    #[test]
    fn the_sdk_answers_the_frameworks_a_file_watcher_asks_for() {
        let Some(sdk) = sdk() else {
            return;
        };
        // What `notify` pulls in on macOS, and what rust-analyzer failed on.
        for name in ["CoreFoundation", "CoreServices"] {
            let found = find_framework(name, &[], Some(&sdk));
            assert!(found.is_some(), "-framework {name} did not resolve");
            assert!(found.unwrap().to_string_lossy().ends_with(".tbd"));
        }
    }

    #[test]
    fn a_search_path_is_consulted_before_the_sdk() {
        let scratch = std::env::temp_dir().join("blinker-library-search");
        std::fs::create_dir_all(&scratch).expect("scratch");
        let shadow = scratch.join("libSystem.tbd");
        std::fs::write(&shadow, "").expect("write");
        let found = find_library("System", std::slice::from_ref(&scratch), sdk().as_deref());
        assert_eq!(found.as_deref(), Some(shadow.as_path()));
        std::fs::remove_file(&shadow).ok();
    }

    #[test]
    fn a_name_nothing_provides_resolves_to_nothing() {
        assert!(find_library("definitely-not-a-library", &[], sdk().as_deref()).is_none());
        assert!(find_framework("DefinitelyNotAFramework", &[], sdk().as_deref()).is_none());
    }
}
