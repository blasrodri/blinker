//! Input fingerprinting.
//!
//! Spec §13 defines a two-stage strategy: a cheap metadata probe, escalating to
//! a content hash when metadata cannot be trusted. M0 implements the **fast
//! path only** — it records the metadata and hashes on demand — because there
//! is no cache yet to invalidate. The verification-path *policy* (when a hash
//! becomes mandatory) belongs to M4, where reuse decisions actually depend on
//! it.
//!
//! What matters at M0 is that the recorded shape already carries everything the
//! later policy will need, so the JSON contract does not have to change when
//! M4 starts making decisions from it.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Identity of one input file at the moment it was observed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InputFingerprint {
    pub path: PathBuf,
    pub file_size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    ///
    /// `None` when the filesystem does not report one — which is itself a
    /// signal that the metadata fast path is not trustworthy for this input.
    pub modified_time_ns: Option<u128>,
    /// BLAKE3 content hash, hex-encoded. Populated only when explicitly
    /// requested; `None` means "not computed", never "no content".
    pub content_hash: Option<String>,
    /// True when the file could not be read at all. Recorded rather than
    /// treated as fatal: rustc sometimes names inputs the driver resolves.
    pub missing: bool,
    /// Where this input was copied to when the invocation was archived.
    ///
    /// rustc writes `symbols.o` and the per-CGU `.rcgu.o` files into a
    /// temporary directory that it deletes as soon as the link returns, so the
    /// original `path` is dangling by the time anyone replays the recording.
    /// An archived copy is what makes a recorded corpus genuinely replayable
    /// rather than merely readable.
    pub archived_path: Option<PathBuf>,
}

impl InputFingerprint {
    /// Fingerprint one input via the metadata fast path.
    ///
    /// `hash_contents` forces the verification path. A file that cannot be
    /// stat'd yields a `missing` fingerprint rather than an error, so one
    /// unreadable input does not abort the whole recording.
    pub fn probe(path: &Path, hash_contents: bool) -> Self {
        let Ok(meta) = std::fs::metadata(path) else {
            return InputFingerprint {
                path: path.to_path_buf(),
                file_size: 0,
                modified_time_ns: None,
                content_hash: None,
                missing: true,
                archived_path: None,
            };
        };

        let modified_time_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());

        let content_hash = hash_contents.then(|| hash_file(path)).flatten();

        InputFingerprint {
            path: path.to_path_buf(),
            file_size: meta.len(),
            modified_time_ns,
            content_hash,
            missing: false,
            archived_path: None,
        }
    }

    /// Construct a fingerprint directly, for tests that need a known size.
    #[doc(hidden)]
    pub fn for_test(path: &str, size: u64) -> Self {
        InputFingerprint {
            path: PathBuf::from(path),
            file_size: size,
            modified_time_ns: None,
            content_hash: None,
            missing: false,
            archived_path: None,
        }
    }
}

/// Hash a file's contents with BLAKE3, streaming so large archives do not need
/// to be held in memory.
fn hash_file(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(hasher.finalize().to_hex().to_string())
}

/// Fingerprint a batch of inputs.
pub fn fingerprint_input(paths: &[&Path], hash_contents: bool) -> Vec<InputFingerprint> {
    paths
        .iter()
        .map(|p| InputFingerprint::probe(p, hash_contents))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_test_support::Scratch;

    fn temp_file(tag: &str, contents: &[u8]) -> Scratch {
        Scratch::file(tag, contents).unwrap()
    }

    #[test]
    fn records_size_and_mtime_on_the_fast_path() {
        let file = temp_file("size", b"hello world");
        let fp = InputFingerprint::probe(file.path(), false);
        assert_eq!(fp.file_size, 11);
        assert!(fp.modified_time_ns.is_some());
        assert!(!fp.missing);
    }

    #[test]
    fn fast_path_does_not_hash() {
        // Hashing every input on every invocation would defeat the purpose of
        // having a fast path at all.
        let file = temp_file("nohash", b"hello");
        assert_eq!(
            InputFingerprint::probe(file.path(), false).content_hash,
            None
        );
    }

    #[test]
    fn verification_path_produces_a_stable_blake3_hash() {
        let file = temp_file("hash", b"hello world");
        let fp = InputFingerprint::probe(file.path(), true);
        // Known BLAKE3 of "hello world" — pins the algorithm, not just that
        // *some* hash was produced.
        assert_eq!(
            fp.content_hash.as_deref(),
            Some("d74981efa70a0c880b8d8c1985d075dbcbf679b99a5f9914e5aaf96b831a9e24")
        );
    }

    #[test]
    fn identical_contents_hash_identically_across_paths() {
        let a = temp_file("dup-a", b"same bytes");
        let b = temp_file("dup-b", b"same bytes");
        assert_eq!(
            InputFingerprint::probe(a.path(), true).content_hash,
            InputFingerprint::probe(b.path(), true).content_hash
        );
    }

    #[test]
    fn differing_contents_hash_differently() {
        let a = temp_file("diff-a", b"contents one");
        let b = temp_file("diff-b", b"contents two");
        assert_ne!(
            InputFingerprint::probe(a.path(), true).content_hash,
            InputFingerprint::probe(b.path(), true).content_hash
        );
    }

    #[test]
    fn empty_file_is_present_not_missing() {
        // Zero-length is a legitimate state and must not be confused with an
        // absent file — they lead to different invalidation decisions later.
        let file = temp_file("empty", b"");
        let fp = InputFingerprint::probe(file.path(), true);
        assert!(!fp.missing);
        assert_eq!(fp.file_size, 0);
        assert!(fp.content_hash.is_some());
    }

    #[test]
    fn missing_file_is_recorded_rather_than_failing_the_batch() {
        let fp = InputFingerprint::probe(Path::new("/nonexistent/blinker/x.o"), true);
        assert!(fp.missing);
        assert_eq!(fp.file_size, 0);
        assert_eq!(fp.content_hash, None);
    }

    #[test]
    fn batch_fingerprinting_preserves_order_including_missing_entries() {
        let file = temp_file("batch", b"data");
        let missing = Path::new("/nonexistent/blinker/y.o");
        let fps = fingerprint_input(&[file.path(), missing], false);
        assert_eq!(fps.len(), 2);
        assert!(!fps[0].missing);
        assert!(fps[1].missing);
    }
}
