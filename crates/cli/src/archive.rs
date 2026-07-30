//! Snapshotting a link's inputs so the recording can actually be replayed.
//!
//! # Why this exists
//!
//! rustc writes the object files for a link — `symbols.o` and one `.rcgu.o` per
//! codegen unit — into a temporary directory that it removes as soon as the
//! linker returns. By the time anyone opens a recorded invocation, those paths
//! are dangling. Replaying the recorded argument vector verbatim then fails
//! with `no such file or directory` on every object file.
//!
//! This was found by actually replaying a recording rather than by reading the
//! spec, and it is the reason a recorded corpus needs archived inputs to be
//! worth anything: without them a recording documents a link but cannot
//! reproduce one.
//!
//! # What is archived
//!
//! Every positional input (`.o`, `.a`, `.rlib`, `.dylib`) is copied next to the
//! record. Flag arguments are left alone — they carry no file identity — and
//! the output path is deliberately *not* rewritten here; replay redirects it so
//! a replayed link cannot overwrite a real build artifact.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use blinker_diagnostics::InputFingerprint;

/// Copy every existing input into `dest`, returning the original→archived map.
///
/// Archived names are prefixed with the input's index to keep them unique:
/// a link routinely contains several files named `symbols.o` from different
/// temporary directories, and flattening them into one directory would
/// otherwise silently collapse them onto each other.
pub fn archive_inputs(
    inputs: &mut [InputFingerprint],
    dest: &Path,
) -> std::io::Result<HashMap<PathBuf, PathBuf>> {
    let mut mapping = HashMap::new();
    if inputs.iter().all(|i| i.missing) {
        return Ok(mapping);
    }
    std::fs::create_dir_all(dest)?;

    for (index, input) in inputs.iter_mut().enumerate() {
        if input.missing {
            continue;
        }
        let file_name = input
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "input".to_string());
        let archived = dest.join(format!("{index:04}-{file_name}"));

        std::fs::copy(&input.path, &archived)?;
        mapping.insert(input.path.clone(), archived.clone());
        input.archived_path = Some(archived);
    }
    Ok(mapping)
}

/// Rewrite an argument vector so input paths point at their archived copies.
///
/// Arguments with no mapping pass through unchanged, so flags, library
/// requests, and the output path are preserved exactly.
pub fn rewrite_argv(argv: &[String], mapping: &HashMap<PathBuf, PathBuf>) -> Vec<String> {
    argv.iter()
        .map(|arg| match mapping.get(Path::new(arg)) {
            Some(archived) => archived.display().to_string(),
            None => arg.clone(),
        })
        .collect()
}

/// Redirect the `-o` target of a replayed link to `output`.
///
/// Replaying must never overwrite the artifact the original link produced —
/// that would let a diagnostic action corrupt a real build tree.
pub fn redirect_output(argv: &[String], output: &Path) -> Vec<String> {
    let mut out = argv.to_vec();
    for i in 0..out.len() {
        if out[i] == "-o" && i + 1 < out.len() {
            out[i + 1] = output.display().to_string();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "blinker-arch-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn file(&self, rel: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fingerprints(paths: &[&Path]) -> Vec<InputFingerprint> {
        paths
            .iter()
            .map(|p| InputFingerprint::probe(p, false))
            .collect()
    }

    #[test]
    fn copies_inputs_and_records_their_archived_paths() {
        let scratch = Scratch::new("copy");
        let a = scratch.file("a.o", b"object a");
        let dest = scratch.0.join("archive");

        let mut inputs = fingerprints(&[&a]);
        let mapping = archive_inputs(&mut inputs, &dest).unwrap();

        let archived = inputs[0].archived_path.clone().unwrap();
        assert!(archived.exists());
        assert_eq!(std::fs::read(&archived).unwrap(), b"object a");
        assert_eq!(mapping.get(&a), Some(&archived));
    }

    /// A link genuinely contains several files named `symbols.o`, one per
    /// crate, living in different temp directories. Flattening by base name
    /// would silently overwrite them.
    #[test]
    fn identically_named_inputs_do_not_collide_in_the_archive() {
        let scratch = Scratch::new("collide");
        let first = scratch.file("one/symbols.o", b"first");
        let second = scratch.file("two/symbols.o", b"second");
        let dest = scratch.0.join("archive");

        let mut inputs = fingerprints(&[&first, &second]);
        archive_inputs(&mut inputs, &dest).unwrap();

        let a = inputs[0].archived_path.clone().unwrap();
        let b = inputs[1].archived_path.clone().unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(a).unwrap(), b"first");
        assert_eq!(std::fs::read(b).unwrap(), b"second");
    }

    #[test]
    fn missing_inputs_are_skipped_without_failing_the_archive() {
        let scratch = Scratch::new("missing");
        let real = scratch.file("real.o", b"data");
        let dest = scratch.0.join("archive");

        let mut inputs = fingerprints(&[&real, Path::new("/nonexistent/blinker/x.o")]);
        let mapping = archive_inputs(&mut inputs, &dest).unwrap();

        assert_eq!(mapping.len(), 1);
        assert!(inputs[0].archived_path.is_some());
        assert!(inputs[1].archived_path.is_none());
    }

    #[test]
    fn rewrites_only_arguments_that_name_archived_inputs() {
        let mut mapping = HashMap::new();
        mapping.insert(
            PathBuf::from("/tmp/a.o"),
            PathBuf::from("/archive/0000-a.o"),
        );

        let argv = vec![
            "/tmp/a.o".to_string(),
            "-lSystem".to_string(),
            "-o".to_string(),
            "/tmp/out".to_string(),
        ];
        assert_eq!(
            rewrite_argv(&argv, &mapping),
            vec!["/archive/0000-a.o", "-lSystem", "-o", "/tmp/out"]
        );
    }

    #[test]
    fn rewriting_with_an_empty_mapping_is_the_identity() {
        let argv = vec!["a.o".to_string(), "-o".to_string(), "out".to_string()];
        assert_eq!(rewrite_argv(&argv, &HashMap::new()), argv);
    }

    #[test]
    fn redirects_the_output_path() {
        let argv = vec![
            "a.o".to_string(),
            "-o".to_string(),
            "/real/build/artifact".to_string(),
        ];
        let out = redirect_output(&argv, Path::new("/tmp/replay-out"));
        assert_eq!(out, vec!["a.o", "-o", "/tmp/replay-out"]);
    }

    #[test]
    fn redirecting_without_an_output_flag_changes_nothing() {
        let argv = vec!["a.o".to_string()];
        assert_eq!(redirect_output(&argv, Path::new("/tmp/x")), argv);
    }

    #[test]
    fn a_trailing_output_flag_does_not_panic() {
        let argv = vec!["a.o".to_string(), "-o".to_string()];
        assert_eq!(redirect_output(&argv, Path::new("/tmp/x")), argv);
    }
}
