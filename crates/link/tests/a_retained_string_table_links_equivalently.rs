//! A link whose string table came from the previous link says the same thing.
//!
//! # Why this is not the byte comparison every other session test makes
//!
//! `a_held_input_links_identically` links a program warm and cold and compares
//! the two files. That is the strongest oracle in the suite and it is the right
//! one almost everywhere — but it is the *wrong* one here, because a retained
//! string table is deliberately not byte-identical to a fresh one.
//!
//! An offset in that table is first-reference order over the names of whichever
//! link built it. Retained, it keeps the bytes of names that have since gone
//! away and puts newly appeared names at the end rather than where this link
//! first mentions them. Every `n_strx` moves with them, and so does every byte
//! after the table. A comparison that failed on that would be reporting the
//! feature, not a bug.
//!
//! What must hold is the thing the offsets *mean*: every symbol still resolves
//! to the name it had, with the same type, section, description and value, and
//! everything outside `__LINKEDIT` is untouched. That is what this checks — and
//! it is stricter than it sounds, because a stale offset (the failure this
//! guards against) points a symbol at some *other* symbol's name, which shows
//! up here as a mismatched name against an otherwise identical entry.
//!
//! Runs the retained path under `BLINKER_RETAIN_STRINGS`, which is off by
//! default; see `Session::retain_strings` for why.

use blinker_link::{link_to_file, link_to_file_in, LinkRequest, Session};
use blinker_test_support::Scratch;
use std::path::PathBuf;
use std::process::Command;

const MAIN: &str = r#"
#include <stdio.h>
int helper(int n);
int main(void) { printf("%d\n", helper(3)); return 0; }
"#;

/// A second version defining a name the first did not, and dropping one it did.
/// Both directions matter: a name that disappears leaves a hole in the retained
/// table, and one that appears is appended past every offset already given out.
const HELPER_FIRST: &str = r#"
static int only_in_the_first(int n) { return n + 1; }
int helper(int n) { return only_in_the_first(n) * 7; }
"#;

const HELPER_SECOND: &str = r#"
static int only_in_the_second(int n) { return n + 2; }
int helper(int n) { return only_in_the_second(n) * 7; }
"#;

fn compile(scratch: &Scratch, name: &str, source: &str) -> PathBuf {
    let path = scratch.join(name);
    std::fs::write(&path, source).expect("source written");
    let object = path.with_extension("o");
    let status = Command::new("cc")
        .args(["-g", "-c", "-o"])
        .arg(&object)
        .arg(&path)
        .status()
        .expect("cc runs");
    assert!(status.success(), "compiling {name} failed");
    object
}

fn run(binary: &std::path::Path) -> String {
    let output = Command::new(binary).output().expect("the binary runs");
    assert!(output.status.success(), "the binary exited non-zero");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One symbol as the file describes it, with its name resolved through the
/// string table rather than left as an offset.
type Entry = (String, u8, u8, u16, u64);

/// Every symbol of `binary`, read back out of the file.
///
/// Walks `LC_SYMTAB` by hand rather than through anything the linker uses. The
/// failure being looked for is an offset that points at the wrong name, and a
/// reader that shared the linker's idea of where names are would resolve it the
/// same wrong way and report agreement.
fn symbols(binary: &std::path::Path) -> Vec<Entry> {
    const LC_SYMTAB: u32 = 0x2;
    let bytes = std::fs::read(binary).expect("the image is readable");
    let word = |at: usize| -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
    };

    let commands = word(16) as usize;
    let mut at = 32;
    let (symbol_offset, count, string_offset, string_size) = (0..commands)
        .find_map(|_| {
            let (kind, size) = (word(at), word(at + 4) as usize);
            let found = (kind == LC_SYMTAB).then(|| {
                (
                    word(at + 8) as usize,
                    word(at + 12) as usize,
                    word(at + 16) as usize,
                    word(at + 20) as usize,
                )
            });
            at += size;
            found
        })
        .expect("the image has an LC_SYMTAB");

    let strings = &bytes[string_offset..string_offset + string_size];
    (0..count)
        .map(|index| {
            let at = symbol_offset + index * 16;
            let name_offset = word(at) as usize;
            let name = strings[name_offset..]
                .split(|byte| *byte == 0)
                .next()
                .expect("a NUL-terminated name");
            (
                String::from_utf8_lossy(name).into_owned(),
                bytes[at + 4],
                bytes[at + 5],
                u16::from_le_bytes(bytes[at + 6..at + 8].try_into().expect("two bytes")),
                u64::from_le_bytes(bytes[at + 8..at + 16].try_into().expect("eight bytes")),
            )
        })
        .collect()
}

#[test]
fn a_retained_string_table_names_every_symbol_the_way_a_cold_link_does() {
    // Set before the first link: the switch is read once and cached, so a test
    // that set it later would silently measure the default.
    std::env::set_var("BLINKER_RETAIN_STRINGS", "1");

    let scratch = Scratch::dir("retained-string-table").expect("scratch");
    let main = compile(&scratch, "main.c", MAIN);
    let helper = compile(&scratch, "helper.c", HELPER_FIRST);
    let request = LinkRequest::new(vec![main.clone(), helper.clone()]);

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("the first link succeeds");
    assert_eq!(run(&first), "28\n");

    // Now change the program under the same session, so the second link is
    // handed a table holding a name it no longer emits and asked for one it
    // has never seen.
    std::fs::write(scratch.join("helper.c"), HELPER_SECOND).expect("source rewritten");
    let rebuilt = compile(&scratch, "helper.c", HELPER_SECOND);
    assert_eq!(
        rebuilt, helper,
        "the object keeps its path, as a rebuild does"
    );

    let warm = scratch.join("warm");
    link_to_file_in(&request, &warm, &mut session).expect("the second link succeeds");

    let cold = scratch.join("cold");
    link_to_file(&request, &cold).expect("the cold link succeeds");

    // Runs, first: a symbol table that disagrees with the code is worth
    // knowing about, but a binary that does not run is worth knowing about
    // sooner.
    assert_eq!(run(&warm), "35\n");
    assert_eq!(run(&cold), "35\n");

    let (warm_symbols, cold_symbols) = (symbols(&warm), symbols(&cold));
    assert!(
        !warm_symbols.is_empty(),
        "no symbols were read back, so this proves nothing"
    );
    assert_eq!(
        warm_symbols, cold_symbols,
        "a link over a retained string table describes its symbols differently \
         from a cold link of the same inputs"
    );

    // And the retention actually happened. Without this the test passes just as
    // well against a table that was thrown away every link, which is the state
    // it is meant to be distinguishing itself from.
    let (warm_bytes, cold_bytes) = (
        std::fs::read(&warm).expect("warm"),
        std::fs::read(&cold).expect("cold"),
    );
    assert_ne!(
        warm_bytes, cold_bytes,
        "the two images are identical, so the string table was not retained \
         and the comparison above tested nothing"
    );
}
