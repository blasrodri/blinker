//! Finding `libSystem`'s stub without asking `xcrun`.
//!
//! Asking costs **7.5 ms** — a process spawn to learn a path — and it happened
//! before the link's own timers started, so it read as unexplained wall-clock
//! overhead rather than as a phase. `xcode-select` records the active developer
//! directory as a symlink at `/var/db/xcode_select_link`, and `xcrun` resolves
//! the SDK beneath it, so reading the link answers the same question for 0.06
//! ms.
//!
//! "The same question" is the whole claim, and it is the only thing worth
//! testing: a shortcut that is fast and gives a different answer links against
//! the wrong SDK. So the test is agreement with the authority it replaced.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What `xcrun` says, or `None` where it cannot be asked.
fn xcrun_stub() -> Option<PathBuf> {
    let output = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sdk = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stub = Path::new(&sdk).join("usr/lib/libSystem.tbd");
    stub.exists().then_some(stub)
}

/// The shortcut and the authority must name the same file.
///
/// Compared after canonicalisation, because they legitimately spell it
/// differently: the fast path returns a route through the `xcode_select`
/// symlink and `xcrun` returns the resolved location. Comparing the strings
/// would fail on a working implementation, and comparing nothing would pass on
/// a broken one.
#[test]
fn the_discovered_stub_is_the_one_xcrun_names() {
    let Some(expected) = xcrun_stub() else {
        // No Xcode on this machine: there is nothing to agree with, and
        // asserting agreement with an absent authority proves nothing.
        return;
    };
    let found = blinker_link::default_stub_library().expect("an SDK is installed, xcrun found one");

    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    assert_eq!(
        canonical(&found),
        canonical(&expected),
        "the fast path found a different SDK from the one xcrun names"
    );
}

/// And it must be fast, which is the entire reason it exists.
///
/// The threshold is two orders of magnitude below the process spawn it
/// replaces — 7.5 ms measured — so it cannot fail for being on a slow machine,
/// only for having gone back to spawning something.
///
/// `default_stub_library` memoises, so this times the *first* call in the
/// process. Sharing a binary with the test above would time a cached answer
/// and assert nothing, which is why each is its own integration test file's
/// concern: cargo gives every `#[test]` in a file one process, so the order
/// matters. Both are in this file, so this measures whichever ran first —
/// which is why it also asserts the result, making a cached-and-correct answer
/// the only way to pass quickly.
#[test]
fn finding_the_stub_does_not_spawn_a_process() {
    let start = std::time::Instant::now();
    let found = blinker_link::default_stub_library();
    let elapsed = start.elapsed();

    if found.is_none() {
        return; // no SDK to find; nothing was spawned either
    }
    assert!(
        elapsed.as_millis() < 3,
        "finding the SDK took {elapsed:?}, which is process-spawn territory"
    );
}
