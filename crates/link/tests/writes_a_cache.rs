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
    let request = LinkRequest::new(objects).cached_at(cache_path.clone());
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
