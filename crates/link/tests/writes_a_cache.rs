//! What a cold link records for the next one to reuse.
//!
//! The cache's own crate tests its structures against hand-built values. This
//! tests it against a real link, which is the only thing that can say whether
//! the *linker* filled them in correctly — a dependency list that is empty, or
//! ranges that describe a layout nobody used, would pass every unit test in
//! `blinker-cache` and be worthless.

use blinker_link::{link_to_file, LinkRequest};
use blinker_test_support::Scratch;
use std::path::PathBuf;
use std::process::Command;

const DEPLOYMENT_TARGET: &str = "-mmacosx-version-min=11.0";

const PROGRAM: &str = r#"
#include <stdio.h>
int shared_counter = 7;
int helper(int n);
int main(void) { printf("%d\n", helper(shared_counter)); return 0; }
"#;

const HELPER: &str = r#"
extern int shared_counter;
int helper(int n) { return n * 6 + shared_counter; }
"#;

fn compile(scratch: &Scratch, sources: &[(&str, &str)]) -> Vec<PathBuf> {
    sources
        .iter()
        .map(|(name, code)| {
            let source = scratch.write(name, *code).expect("writable");
            let object = scratch.join(format!("{name}.o"));
            let status = Command::new("cc")
                .args(["-arch", "arm64", DEPLOYMENT_TARGET, "-c"])
                .arg(&source)
                .arg("-o")
                .arg(&object)
                .status()
                .expect("cc runs");
            assert!(status.success(), "cc failed to compile {name}");
            object
        })
        .collect()
}

/// Link the two-object fixture with a cache, and return what it wrote.
fn link_with_cache(tag: &str) -> (Scratch, blinker_cache::LinkCache) {
    let scratch = Scratch::dir(tag).expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let cache_path = scratch.join("link.blinkcache");
    let request = LinkRequest::new(objects)
        .cached_at(cache_path.clone())
        .reusing_relocations(true);
    link_to_file(&request, &scratch.join("program")).expect("the link succeeds");
    let cache = blinker_cache::load(&cache_path).expect("a cache was written");
    (scratch, cache)
}

#[test]
fn a_cold_link_writes_a_cache_it_can_read_back() {
    let (_scratch, cache) = link_with_cache("cache-write");
    assert_eq!(cache.entries.len(), 2, "one entry per input object");
    assert!(!cache.addresses.is_empty(), "no addresses recorded");
    assert!(!cache.sections.is_empty(), "no patched bytes recorded");
}

/// The cached bytes must be the bytes that were written, not the unrelocated
/// input: reusing pre-relocation content would produce a binary that links and
/// crashes.
#[test]
fn the_cached_sections_are_the_relocated_output() {
    let (scratch, cache) = link_with_cache("cache-bytes");
    let binary = std::fs::read(scratch.join("program")).expect("the binary exists");
    let text = cache
        .sections
        .iter()
        .max_by_key(|(_, bytes)| bytes.len())
        .expect("some section has content");
    // Every cached section appears verbatim in the file that was produced.
    assert!(
        binary
            .windows(text.1.len())
            .any(|window| window == text.1.as_slice()),
        "the largest cached section is not present in the output"
    );
}

/// Condition 3 is the whole reason this is a graph. `main.o` calls `helper`
/// and reads `shared_counter`, both defined in the other object, so its entry
/// must say so — an empty dependency list would make it look reusable no
/// matter what happened to `helper.o`.
#[test]
fn an_entry_records_the_addresses_its_object_read() {
    let (_scratch, cache) = link_with_cache("cache-deps");
    assert!(
        cache.entries.iter().all(|entry| !entry.deps.is_empty()),
        "an object relocated against nothing at all"
    );
    let main = cache
        .entries
        .iter()
        .max_by_key(|entry| entry.deps.len())
        .expect("entries exist");
    for symbol in ["_helper", "_shared_counter"] {
        assert!(
            main.deps.contains(&blinker_cache::name_hash(symbol)),
            "{symbol} is missing from the dependency list"
        );
    }
}

/// Ranges are condition 2, and they have to describe where the bytes actually
/// went — inside the section they name, and not overlapping another object's.
#[test]
fn the_recorded_ranges_lie_inside_the_sections_they_name() {
    let (_scratch, cache) = link_with_cache("cache-ranges");
    let sizes: std::collections::HashMap<u32, usize> = cache
        .sections
        .iter()
        .map(|(index, bytes)| (*index, bytes.len()))
        .collect();

    let mut seen: Vec<(u32, u64, u64)> = Vec::new();
    for entry in &cache.entries {
        assert!(!entry.ranges.is_empty(), "an object contributed nothing");
        for range in &entry.ranges {
            // A zero-filled section has no bytes but still holds ranges.
            if let Some(size) = sizes.get(&range.section) {
                assert!(
                    (range.start + range.len) as usize <= *size,
                    "range {range:?} runs past its {size}-byte section"
                );
            }
            seen.push((range.section, range.start, range.len));
        }
    }

    // Sorted before the sweep. Comparing entries in the order they happened to
    // be recorded only ever checks *adjacent* claims, which is why a first
    // version of this passed while every entry claimed every contribution:
    // the two copies of a duplicated range were never neighbours.
    seen.sort_unstable();
    for (a, b) in seen.iter().zip(seen.iter().skip(1)) {
        if a.0 == b.0 {
            assert!(
                a.1 + a.2 <= b.1,
                "two objects claim overlapping bytes: {a:?} and {b:?}"
            );
        }
    }
}

/// The default. A link that silently reads state from a previous run is a link
/// whose result depends on history.
#[test]
fn no_cache_is_written_unless_one_was_asked_for() {
    let scratch = Scratch::dir("cache-off").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    link_to_file(&LinkRequest::new(objects), &scratch.join("program")).expect("the link succeeds");
    assert!(!scratch.join("link.blinkcache").exists());
}

/// The only assertion that really matters: a link that reused cached bytes
/// must produce **the same binary** as one that did not.
///
/// Byte-for-byte, not "runs correctly" — a wrong pointer in a rarely-taken
/// path would pass an execution test and be exactly the failure this design
/// exists to prevent.
#[test]
fn a_second_link_reuses_the_cache_and_produces_an_identical_binary() {
    let scratch = Scratch::dir("cache-reuse").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let cache_path = scratch.join("link.blinkcache");
    let request = LinkRequest::new(objects)
        .cached_at(cache_path.clone())
        .reusing_relocations(true);

    let cold = scratch.join("cold");
    let first = blinker_link::link_to_file_timed(&request, &cold).expect("cold link");
    assert_eq!(
        first.reused_objects, 0,
        "the first link had nothing to reuse"
    );

    let warm = scratch.join("warm");
    let second = blinker_link::link_to_file_timed(&request, &warm).expect("warm link");
    assert_eq!(
        second.reused_objects, 2,
        "both objects should have been reused"
    );

    assert_eq!(
        std::fs::read(&cold).unwrap(),
        std::fs::read(&warm).unwrap(),
        "the incremental link produced a different binary"
    );
}

/// And it must still run.
#[test]
fn the_binary_from_a_reusing_link_runs_correctly() {
    let scratch = Scratch::dir("cache-run").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let request = LinkRequest::new(objects)
        .cached_at(scratch.join("link.blinkcache"))
        .reusing_relocations(true);

    let program = scratch.join("program");
    blinker_link::link_to_file(&request, &program).expect("cold link");
    let timings = blinker_link::link_to_file_timed(&request, &program).expect("warm link");
    assert!(timings.reused_objects > 0, "nothing was reused");

    let run = Command::new(&program).output().expect("the program runs");
    assert!(run.status.success(), "exit {:?}", run.status.code());
    // helper(7) = 7 * 6 + 7 = 49
    assert_eq!(String::from_utf8_lossy(&run.stdout), "49\n");
}

/// Editing one object must invalidate that object and leave the other alone —
/// and the result must still match a link that used no cache at all.
#[test]
fn editing_one_object_invalidates_only_what_depended_on_it() {
    let scratch = Scratch::dir("cache-edit").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let request = LinkRequest::new(objects.clone())
        .cached_at(scratch.join("link.blinkcache"))
        .reusing_relocations(true);
    blinker_link::link_to_file(&request, &scratch.join("first")).expect("cold link");

    // Same symbols, same sizes, different constant: the body edit that
    // findings 44 and 45 say is the common case.
    compile(&scratch, &[("helper.c", &HELPER.replace("n * 6", "n * 9"))]);

    let edited = scratch.join("edited");
    let timings = blinker_link::link_to_file_timed(&request, &edited).expect("warm link");
    assert_eq!(
        timings.reused_objects, 1,
        "the edited object should have been rebuilt and the other reused"
    );

    // Against a linker with no cache at all, on the same inputs.
    let reference = scratch.join("reference");
    blinker_link::link_to_file(&LinkRequest::new(objects), &reference).expect("uncached link");
    assert_eq!(
        std::fs::read(&edited).unwrap(),
        std::fs::read(&reference).unwrap(),
        "reusing across an edit produced a different binary"
    );

    let run = Command::new(&edited).output().expect("the program runs");
    // helper(7) = 7 * 9 + 7 = 70
    assert_eq!(String::from_utf8_lossy(&run.stdout), "70\n");
}

/// Condition 3, which the size-preserving edit above never exercises: an
/// object that did not change and did not move, but which reads an address
/// that did.
///
/// `main.o` is byte-identical here and lands in the same place. Only
/// `helper.o` was edited — but the edit inserts a function *ahead* of
/// `helper`, so `_helper` moves, and every byte in `main.o` that branches to
/// it is now wrong. An implementation that checked only "did this input
/// change" would happily reuse it.
#[test]
fn an_unchanged_object_is_rebuilt_when_a_symbol_it_reads_moves() {
    let scratch = Scratch::dir("cache-moved").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let request = LinkRequest::new(objects.clone())
        .cached_at(scratch.join("link.blinkcache"))
        .reusing_relocations(true);
    blinker_link::link_to_file(&request, &scratch.join("first")).expect("cold link");

    let grown = format!(
        "int padding(int n) {{ return n + {}; }}\n{HELPER}",
        (0..40)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" + ")
    );
    compile(&scratch, &[("helper.c", &grown)]);

    let after = scratch.join("after");
    blinker_link::link_to_file_timed(&request, &after).expect("warm link");

    let reference = scratch.join("reference");
    blinker_link::link_to_file(&LinkRequest::new(objects), &reference).expect("uncached link");
    assert_eq!(
        std::fs::read(&after).unwrap(),
        std::fs::read(&reference).unwrap(),
        "an object was reused across a move of a symbol it reads"
    );

    let run = Command::new(&after).output().expect("the program runs");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "49\n");
}

/// An object with a zero-filled contribution is still reused.
///
/// **This test does not reproduce the bug that motivated it.** Reverting the
/// fix leaves it passing; only the real Rust link goes from 4.9 ms of relocate
/// back to 10.6. Whatever section Rust has that this fixture does not — the
/// thread-local block is the suspect — is what actually triggered it, and
/// finding a C fixture that reaches the same path is unfinished work.
///
/// It is kept because the weaker property is worth holding, and recorded
/// honestly rather than described as the guard it is not: a test whose
/// negative control passes is a test that proves nothing, and labelling it
/// otherwise is worse than not having it. See FINDINGS.md 64.
#[test]
fn objects_contributing_to_a_zero_filled_section_are_still_reused() {
    const WITH_BSS: &str = r#"
#include <stdio.h>
// File-local, so it is a plain `__bss` definition. A non-static
// uninitialised global would be a *tentative* (common) symbol, which
// blinker does not yet resolve — a separate gap, recorded as finding 65
// rather than worked around by testing something else.
static int big_uninitialised[4096];
int shared_counter = 7;
int helper(int n);
int main(void) {
    big_uninitialised[17] = helper(shared_counter);
    printf("%d\n", big_uninitialised[17]);
    return 0;
}
"#;
    let scratch = Scratch::dir("cache-bss").expect("scratch");
    let objects = compile(&scratch, &[("main.c", WITH_BSS), ("helper.c", HELPER)]);
    let request = LinkRequest::new(objects)
        .cached_at(scratch.join("link.blinkcache"))
        .reusing_relocations(true);

    let cold = scratch.join("cold");
    blinker_link::link_to_file(&request, &cold).expect("cold link");
    let warm = scratch.join("warm");
    let timings = blinker_link::link_to_file_timed(&request, &warm).expect("warm link");

    assert_eq!(
        timings.reused_objects, 2,
        "a zero-filled contribution should not disqualify its object"
    );
    assert_eq!(std::fs::read(&cold).unwrap(), std::fs::read(&warm).unwrap());
    let run = Command::new(&warm).output().expect("the program runs");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "49\n");
}

/// The fast path: every input unchanged, so the finished binary is the one
/// already on disk and none of the pipeline needs to run.
///
/// Proving the inputs unchanged costs 0.18 ms on a 56-input Rust link against
/// 22.6 ms to link it (finding 67), which is the whole argument for checking
/// before reading rather than after.
#[test]
fn an_unchanged_relink_reuses_the_finished_binary_outright() {
    let scratch = Scratch::dir("cache-whole").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let request = LinkRequest::new(objects)
        .cached_at(scratch.join("link.blinkcache"))
        .reusing_relocations(true);
    let out = scratch.join("program");

    let cold = blinker_link::link_to_file_timed(&request, &out).expect("cold link");
    assert!(!cold.reused_finished_image, "the first link has no cache");
    let first = std::fs::read(&out).expect("a binary");

    let warm = blinker_link::link_to_file_timed(&request, &out).expect("warm link");
    assert!(warm.reused_finished_image, "the fast path did not fire");
    assert_eq!(warm.read_and_parse_ms, 0.0, "inputs were read anyway");
    assert_eq!(first, std::fs::read(&out).expect("a binary"));

    let run = Command::new(&out).output().expect("the program runs");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "49\n");
}

/// Changing any input must defeat it. The fast path does no reasoning about
/// what moved, so it has to be certain that nothing did.
#[test]
fn editing_an_input_defeats_the_whole_image_path() {
    let scratch = Scratch::dir("cache-whole-edit").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let request = LinkRequest::new(objects.clone())
        .cached_at(scratch.join("link.blinkcache"))
        .reusing_relocations(true);
    let out = scratch.join("program");

    blinker_link::link_to_file_timed(&request, &out).expect("cold link");
    compile(&scratch, &[("helper.c", &HELPER.replace("n * 6", "n * 9"))]);
    let after = blinker_link::link_to_file_timed(&request, &out).expect("warm link");
    assert!(!after.reused_finished_image, "a changed input was reused");

    let reference = scratch.join("reference");
    blinker_link::link_to_file(&LinkRequest::new(objects), &reference).expect("uncached");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&reference).unwrap()
    );
}

/// And so must changing the request. The same objects linked with a different
/// entry point are a different binary, and no input key would say so.
#[test]
fn changing_the_request_defeats_the_whole_image_path() {
    let scratch = Scratch::dir("cache-whole-request").expect("scratch");
    let objects = compile(&scratch, &[("main.c", PROGRAM), ("helper.c", HELPER)]);
    let cache = scratch.join("link.blinkcache");
    let out = scratch.join("program");

    let first = LinkRequest::new(objects.clone())
        .cached_at(cache.clone())
        .reusing_relocations(true);
    blinker_link::link_to_file_timed(&first, &out).expect("cold link");

    let renamed = LinkRequest::new(objects)
        .cached_at(cache)
        .reusing_relocations(true)
        .identifier("something-else");
    let after = blinker_link::link_to_file_timed(&renamed, &out).expect("second link");
    assert!(
        !after.reused_finished_image,
        "a different request reused the previous binary"
    );
}
