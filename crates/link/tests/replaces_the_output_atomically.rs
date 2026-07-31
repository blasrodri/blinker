//! A failed link must leave the previous output intact.
//!
//! Writing straight to the output path truncates it before the first byte
//! lands, so a link killed partway — `^C`, a full disk, a panic in the writer
//! — replaces a working executable with a fragment. Whatever the developer was
//! running a second ago stops existing, and the linker that did it exits
//! non-zero and looks blameless.
//!
//! The property is "the output path holds the old image or the new one, never
//! part of either", and these tests check both halves of it.

use blinker_link::{link_to_file, LinkRequest};
use blinker_test_support::Scratch;
use std::path::PathBuf;
use std::process::Command;

const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

fn compile(scratch: &Scratch, code: &str) -> Vec<PathBuf> {
    let source = scratch.write("c.c", code).expect("writable");
    let object = scratch.join("c.o");
    let status = Command::new("cc")
        .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("cc runs");
    assert!(status.success(), "cc failed");
    vec![object]
}

const PROGRAM: &str = "int main(void) { return 7; }\n";

/// The ordinary case still works: a successful link replaces the output and
/// the result runs.
#[test]
fn a_successful_link_replaces_the_output() {
    let scratch = Scratch::dir("atomic-ok").expect("scratch");
    let objects = compile(&scratch, PROGRAM);
    let out = scratch.join("program");

    std::fs::write(&out, b"an older build").expect("writable");
    link_to_file(&LinkRequest::new(objects), &out).expect("the link succeeds");

    let status = Command::new(&out).status().expect("the program runs");
    assert_eq!(status.code(), Some(7));
}

/// And a link that cannot write leaves what was there before.
///
/// **This does not reproduce the bug.** Reverting to an in-place
/// `fs::write` leaves it passing, because writing onto a directory fails
/// before it truncates anything. Kept for the weaker property it does check —
/// that a failed link reports the failure and changes nothing — and labelled
/// rather than trusted. The test with teeth is
/// `the_output_never_holds_a_partial_image`.
///
/// The failure is provoked by making the output a **directory**: the rename
/// onto it fails, which is the same code path a full disk or a cancelled write
/// takes. Anything the linker had already produced must not be visible at the
/// output path.
#[test]
fn a_failed_write_leaves_the_previous_output_alone() {
    let scratch = Scratch::dir("atomic-fail").expect("scratch");
    let objects = compile(&scratch, PROGRAM);

    // A non-empty directory cannot be replaced by a rename.
    let out = scratch.join("occupied");
    std::fs::create_dir_all(&out).expect("a directory");
    std::fs::write(out.join("keep-me"), b"contents").expect("writable");

    let result = link_to_file(&LinkRequest::new(objects), &out);
    assert!(
        result.is_err(),
        "the link reported success writing onto a directory"
    );

    // The thing that was there is still there, and unchanged.
    assert!(out.is_dir(), "the output path was replaced anyway");
    assert_eq!(
        std::fs::read(out.join("keep-me")).expect("still readable"),
        b"contents"
    );
}

/// A failed write must not leave its temporary file behind either.
///
/// Vacuous against the old in-place write, which had no temporary to leave;
/// it guards the new code's own litter rather than the bug that motivated it.
///
/// Litter in the output directory is how a build tree fills with
/// `program.blinker-12345.tmp` after a week of interrupted builds, and it is
/// invisible until someone looks.
#[test]
fn a_failed_write_cleans_up_after_itself() {
    let scratch = Scratch::dir("atomic-litter").expect("scratch");
    let objects = compile(&scratch, PROGRAM);

    let out = scratch.join("occupied");
    std::fs::create_dir_all(&out).expect("a directory");
    std::fs::write(out.join("keep-me"), b"contents").expect("writable");
    let _ = link_to_file(&LinkRequest::new(objects), &out);

    let leftovers: Vec<_> = std::fs::read_dir(scratch.path())
        .expect("readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".blinker-") && name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

/// The window is what matters, and a rename has none: at no instant does the
/// output path hold a partial image.
///
/// This is the one that fails against the old code, and it fails loudly:
/// `the output path held 0 bytes, which is neither the old image (4096) nor
/// the new one (16694): [4096, 0, 16694]`. `fs::write` truncates before it
/// writes, so the zero is the window itself, observed.
///
/// Checked by linking over a file that is being watched: the observer records
/// every distinct size the path takes. With a rename it sees the old size then
/// the new one; with an in-place write it sees intermediate sizes, or a
/// zero-length file, because `fs::write` truncates first.
#[test]
fn the_output_never_holds_a_partial_image() {
    let scratch = Scratch::dir("atomic-window").expect("scratch");
    let objects = compile(&scratch, PROGRAM);
    let out = scratch.join("program");

    // A previous build, distinctively sized so a truncation is unmistakable.
    let previous = vec![0xABu8; 4096];
    std::fs::write(&out, &previous).expect("writable");

    let watching = out.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer = {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut seen: Vec<u64> = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(metadata) = std::fs::metadata(&watching) {
                    let size = metadata.len();
                    if seen.last() != Some(&size) {
                        seen.push(size);
                    }
                }
            }
            seen
        })
    };

    link_to_file(&LinkRequest::new(objects), &out).expect("the link succeeds");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let seen = observer.join().expect("the observer did not panic");

    let final_size = std::fs::metadata(&out).expect("exists").len();
    for size in &seen {
        assert!(
            *size == previous.len() as u64 || *size == final_size,
            "the output path held {size} bytes, which is neither the old \
             image ({}) nor the new one ({final_size}): {seen:?}",
            previous.len()
        );
    }
}
