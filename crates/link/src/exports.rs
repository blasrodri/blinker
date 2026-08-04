//! Which of an image's definitions are visible from outside it.
//!
//! `-exported_symbols_list <file>` names every symbol the image may export;
//! everything else it defines becomes local. rustc passes one on every
//! `-dynamiclib` link, and for a proc-macro crate it holds two names out of
//! the tens of thousands the crate defines — so honouring it is not a detail.
//! A dylib that exports all of them is not merely larger: it offers a
//! definition for every Rust symbol in it, and two such libraries loaded into
//! one process are two answers to the same name.
//!
//! # The file format
//!
//! One pattern per line. `#` starts a comment, blank lines are ignored, and
//! `*` matches any run of characters — `ld64` also honours `?` and character
//! classes, which nothing observed emits and which are therefore matched
//! literally here rather than half-implemented.

/// The names an image is allowed to export.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportList {
    patterns: Vec<String>,
}

impl ExportList {
    /// Read a list file's contents.
    pub fn parse(text: &str) -> Self {
        let patterns = text
            .lines()
            .map(|line| match line.find('#') {
                Some(at) => &line[..at],
                None => line,
            })
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        ExportList { patterns }
    }

    /// Whether `name` may be exported.
    pub fn allows(&self, name: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| matches(pattern.as_str(), name))
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Feed the list to a hasher, so a link keyed by its request cannot be
    /// served the symbol table of a link that exported something else.
    pub fn hash_into(&self, hasher: &mut blake3::Hasher) {
        for pattern in &self.patterns {
            hasher.update(pattern.as_bytes());
            hasher.update(&[0]);
        }
    }
}

/// `*`-globbing, greedy and iterative.
///
/// Iterative rather than recursive because the input is a file the caller
/// supplies: a pattern of alternating stars against a long mangled name is a
/// stack overflow in the recursive form, and a linker does not get to crash on
/// its own command line.
fn matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.as_bytes();
    let name = name.as_bytes();
    let (mut p, mut n) = (0, 0);
    // Where to resume if the current `*` turns out to have matched too little.
    let (mut star, mut resume) = (None, 0);
    while n < name.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            resume = n;
            p += 1;
        } else if p < pattern.len() && pattern[p] == name[n] {
            p += 1;
            n += 1;
        } else if let Some(at) = star {
            // Give the star one more character and try again.
            p = at + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_list_exports_exactly_what_it_names() {
        let list = ExportList::parse("_answer\n_rust_metadata_pm\n");
        assert!(list.allows("_answer"));
        assert!(list.allows("_rust_metadata_pm"));
        assert!(!list.allows("_answer_twice"), "a prefix is not a match");
        assert!(!list.allows("_private"));
    }

    #[test]
    fn comments_and_blank_lines_are_not_symbols() {
        let list = ExportList::parse("# what rustc exports\n\n  _answer  # the entry\n");
        assert!(list.allows("_answer"));
        assert!(!list.allows("#"));
        assert!(!list.allows(""));
    }

    /// An empty list exports nothing, which is different from having no list at
    /// all — the difference between "the caller said none" and "the caller said
    /// nothing", and the two must not collapse.
    #[test]
    fn an_empty_list_exports_nothing() {
        let list = ExportList::parse("\n# only a comment\n");
        assert!(list.is_empty());
        assert!(!list.allows("_answer"));
    }

    #[test]
    fn a_star_matches_a_run_of_anything() {
        let list = ExportList::parse("_rust_metadata_*\n*_decls\n_a*b\n");
        assert!(list.allows("_rust_metadata_pm_e20cf83b"));
        assert!(list.allows("__rustc_proc_macro_decls"));
        assert!(list.allows("_ab"));
        assert!(list.allows("_axxb"));
        assert!(!list.allows("_axxbc"));
        assert!(!list.allows("_rust_metadat"));
    }

    /// The pathological pattern: alternating stars against a long name, which
    /// is what the iterative matcher exists for.
    #[test]
    fn many_stars_against_a_long_name_terminate() {
        let pattern = "*a".repeat(64);
        let name = "a".repeat(4096);
        assert!(matches(&pattern, &name));
        assert!(!matches(&pattern, &"b".repeat(4096)));
    }

    #[test]
    fn two_different_lists_hash_differently() {
        let hash = |text: &str| {
            let mut hasher = blake3::Hasher::new();
            ExportList::parse(text).hash_into(&mut hasher);
            *hasher.finalize().as_bytes()
        };
        assert_ne!(hash("_a\n"), hash("_b\n"));
        assert_ne!(
            hash("_a\n_b\n"),
            hash("_ab\n"),
            "the separator is load-bearing"
        );
        assert_eq!(hash("_a\n"), hash("# c\n_a\n"));
    }
}
