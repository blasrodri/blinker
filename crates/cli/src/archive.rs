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
//! record, and so is every file named by a flag that reads one — see
//! `FILE_VALUED_FLAGS`. Those were missed at first on the reasoning that flags
//! carry no file identity, which is true of flags in general and false of the
//! `-exported_symbols_list` rustc passes on every binary link: it points into
//! the same doomed temporary directory as the objects.
//!
//! The output path is deliberately *not* rewritten here; replay redirects it so
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

/// Flags whose value is a path to a file the link *reads*.
///
/// These are not inputs in the sense `archive_inputs` means — nothing is linked
/// out of them — but they are read, and rustc writes them into the same
/// temporary directory it deletes on the way out. `-exported_symbols_list` is
/// the one that matters in practice: rustc passes it on every binary link, and
/// a record that named the deleted file replayed as an errno from ld64 rather
/// than as a link. A recorded invocation that cannot be replayed is not a
/// recording, so these travel with the objects.
const FILE_VALUED_FLAGS: &[&str] = &[
    "-exported_symbols_list",
    "-unexported_symbols_list",
    "-reexported_symbols_list",
    "-order_file",
    "-filelist",
    "-alias_list",
];

/// Copy the files named by `FILE_VALUED_FLAGS` into `dest`.
///
/// Named by flag rather than by index because they are not positional, and
/// prefixed with `flag-` so they cannot collide with an archived input.
pub fn archive_side_files(
    argv: &[String],
    dest: &Path,
) -> std::io::Result<HashMap<PathBuf, PathBuf>> {
    let mut mapping = HashMap::new();
    for (index, arg) in argv.iter().enumerate() {
        if !FILE_VALUED_FLAGS.contains(&arg.as_str()) {
            continue;
        }
        let Some(value) = argv.get(index + 1) else {
            continue;
        };
        let source = Path::new(value);
        if !source.is_file() {
            continue;
        }
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "list".to_string());
        std::fs::create_dir_all(dest)?;
        let archived = dest.join(format!("flag-{index:04}-{name}"));
        std::fs::copy(source, &archived)?;
        mapping.insert(source.to_path_buf(), archived);
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
    use blinker_test_support::Scratch;

    fn scratch(tag: &str) -> Scratch {
        Scratch::dir(tag).unwrap()
    }

    fn fingerprints(paths: &[&Path]) -> Vec<InputFingerprint> {
        paths
            .iter()
            .map(|p| InputFingerprint::probe(p, false))
            .collect()
    }

    #[test]
    fn copies_inputs_and_records_their_archived_paths() {
        let scratch = scratch("copy");
        let a = scratch.write("a.o", b"object a").unwrap();
        let dest = scratch.join("archive");

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
        let scratch = scratch("collide");
        let first = scratch.write("one/symbols.o", b"first").unwrap();
        let second = scratch.write("two/symbols.o", b"second").unwrap();
        let dest = scratch.join("archive");

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
        let scratch = scratch("missing");
        let real = scratch.write("real.o", b"data").unwrap();
        let dest = scratch.join("archive");

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

#[cfg(test)]
mod side_file_tests {
    use super::*;
    use blinker_test_support::Scratch;

    /// The property the recorder exists for, applied to a file that is not an
    /// object: after archiving, the replayed argument vector must name only
    /// files that survive rustc deleting its temporary directory.
    #[test]
    fn an_exported_symbols_list_is_archived_and_the_argv_points_at_the_copy() {
        let scratch = Scratch::dir("archive-side-files").unwrap();
        let temporary = scratch.join("rustcXXXX");
        std::fs::create_dir_all(&temporary).unwrap();
        let list = temporary.join("list");
        std::fs::write(&list, "_main\n").unwrap();

        let argv = vec![
            "-exported_symbols_list".to_string(),
            list.display().to_string(),
            "-o".to_string(),
            "program".to_string(),
        ];
        let dest = scratch.join("inputs");
        let mapping = archive_side_files(&argv, &dest).unwrap();
        let replayed = rewrite_argv(&argv, &mapping);

        // rustc's directory goes away the instant the link returns.
        std::fs::remove_dir_all(&temporary).unwrap();

        let archived = Path::new(&replayed[1]);
        assert!(
            archived.is_file(),
            "the replay argv still names the deleted file: {}",
            replayed[1]
        );
        assert_eq!(std::fs::read_to_string(archived).unwrap(), "_main\n");
        assert_eq!(replayed[3], "program", "unrelated arguments were rewritten");
    }

    /// A flag whose value is missing or is not a file is left alone rather than
    /// failing the whole recording.
    #[test]
    fn a_flag_with_no_readable_value_is_passed_through() {
        let scratch = Scratch::dir("archive-side-files-absent").unwrap();
        let argv = vec![
            "-order_file".to_string(),
            scratch.join("nothing-here").display().to_string(),
            "-exported_symbols_list".to_string(),
        ];
        let mapping = archive_side_files(&argv, &scratch.join("inputs")).unwrap();
        assert!(mapping.is_empty());
        assert_eq!(rewrite_argv(&argv, &mapping), argv);
    }
}
