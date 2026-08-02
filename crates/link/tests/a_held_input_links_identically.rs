//! A link that reuses parsed inputs must produce what a cold link produces.
//!
//! This is the property that makes a resident linker usable at all, and it is
//! not obvious: the second link takes objects and archive members out of
//! memory instead of off disk, and every one of them carries a byte window into
//! a buffer, an object id, and a symbol table that the rest of the link keys
//! on.
//!
//! The first version of the member cache handed back the parsed member paired
//! with *the whole archive* as its bytes, so every section was read from the
//! wrong offset. That failed loudly — a misaligned relocation, on the second
//! link — and it failed loudly only because the offsets happened not to line
//! up. A member whose sections landed somewhere plausible would have produced a
//! binary, and no test that checks "the link succeeded" would have noticed.
//!
//! So this checks the bytes.

use blinker_link::{link_to_file, link_to_file_in, LinkRequest, Session};
use blinker_test_support::Scratch;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAIN: &str = r#"
#include <stdio.h>
int helper(int n);
int other(int n);
int main(void) { printf("%d\n", helper(3) + other(4)); return 0; }
"#;

const HELPER: &str = "int helper(int n) { return n * 7; }\n";
const OTHER: &str = "int other(int n) { return n + 100; }\n";

fn compile(scratch: &Scratch, name: &str, source: &str) -> PathBuf {
    let path = scratch.join(name);
    std::fs::write(&path, source).expect("source written");
    let object = path.with_extension("o");
    let status = Command::new("cc")
        .args(["-c", "-o"])
        .arg(&object)
        .arg(&path)
        .status()
        .expect("cc runs");
    assert!(status.success(), "compiling {name} failed");
    object
}

/// The two helpers in an archive, so members are exercised and not only
/// top-level objects — members are the majority of a Rust link's inputs and
/// the half that was wrong.
fn archive(scratch: &Scratch, members: &[PathBuf]) -> PathBuf {
    let path = scratch.join("libhelpers.a");
    let mut command = Command::new("ar");
    command.arg("crs").arg(&path);
    for member in members {
        command.arg(member);
    }
    assert!(command.status().expect("ar runs").success(), "ar failed");
    path
}

fn inputs(scratch: &Scratch) -> Vec<PathBuf> {
    let main = compile(scratch, "main.c", MAIN);
    let helper = compile(scratch, "helper.c", HELPER);
    let other = compile(scratch, "other.c", OTHER);
    vec![main, archive(scratch, &[helper, other])]
}

fn run(binary: &Path) -> String {
    let output = Command::new(binary).output().expect("the program runs");
    assert!(output.status.success(), "the program exited non-zero");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The property: a second link through a warm session is byte-identical.
#[test]
fn a_second_link_through_one_session_is_byte_identical() {
    let scratch = Scratch::dir("session-identical").expect("scratch");
    let request = LinkRequest::new(inputs(&scratch));

    let cold = scratch.join("cold");
    link_to_file(&request, &cold).expect("the cold link succeeds");

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("the first link succeeds");
    let second = scratch.join("second");
    let timings = link_to_file_in(&request, &second, &mut session).expect("the second link");

    assert!(
        timings.inputs_held > 0,
        "nothing was held, so this proves nothing about holding"
    );
    assert_eq!(
        timings.inputs_read, 0,
        "an unchanged input was read again: {} held, {} read",
        timings.inputs_held, timings.inputs_read
    );
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold"),
        "the held link differs from a cold one"
    );
    assert_eq!(run(&second), "125\n");
}

/// And the case the byte comparison exists for: an archive member's bytes must
/// come from the member, not from the archive around it.
///
/// A held member with the wrong window still links; whether it *crashes*
/// depends on where its sections happen to land. Running the program is what
/// separates "it produced a file" from "it produced the program".
#[test]
fn a_held_archive_member_keeps_its_own_bytes() {
    let scratch = Scratch::dir("session-member-window").expect("scratch");
    let request = LinkRequest::new(inputs(&scratch));

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("first");
    let second = scratch.join("second");
    link_to_file_in(&request, &second, &mut session).expect("second");

    assert_eq!(run(&first), run(&second));
    assert_eq!(run(&second), "125\n", "3 * 7 + (4 + 100)");
}

/// A changed input is re-read even though the session holds a parse of it, and
/// the result is the one the new bytes produce.
#[test]
fn a_changed_input_is_not_served_from_the_session() {
    let scratch = Scratch::dir("session-changed").expect("scratch");
    let objects = inputs(&scratch);
    let request = LinkRequest::new(objects.clone());

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("first");
    assert_eq!(run(&first), "125\n");

    // helper now multiplies by 8 rather than 7, which changes the answer by 3.
    compile(
        &scratch,
        "helper.c",
        "int helper(int n) { return n * 8; }\n",
    );
    let helper = scratch.join("helper.o");
    let other = scratch.join("other.o");
    archive(&scratch, &[helper, other]);

    let second = scratch.join("second");
    let timings = link_to_file_in(&request, &second, &mut session).expect("second");
    assert!(
        timings.inputs_read > 0,
        "the changed archive was served from memory"
    );
    assert_eq!(run(&second), "128\n", "the edit did not reach the output");

    // And it matches what a linker with no memory of the first build produces.
    let cold = scratch.join("cold");
    link_to_file(&LinkRequest::new(objects), &cold).expect("cold");
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold")
    );
}

/// An edit that changes what an object *needs* must change which archive
/// members are pulled in.
///
/// The session replays the previous extraction order whenever no input's
/// symbol interface moved, on the reasoning that the frontier reads nothing
/// else. This is the case that reasoning has to survive: `main.o` gains a call
/// to a function living in a member the previous link never extracted. Replay
/// the old order and the link is missing a member — an undefined symbol if you
/// are lucky, and a member resolved to the wrong definition if you are not.
#[test]
fn an_edit_that_needs_a_new_member_extracts_it() {
    let scratch = Scratch::dir("session-new-member").expect("scratch");

    let main = compile(&scratch, "main.c", MAIN);
    let helper = compile(&scratch, "helper.c", HELPER);
    let other = compile(&scratch, "other.c", OTHER);
    // A third member nothing reaches yet, so the first link leaves it out.
    let extra = compile(
        &scratch,
        "extra.c",
        "int extra(int n) { return n + 1000; }\n",
    );
    let library = archive(&scratch, &[helper, other, extra]);
    let request = LinkRequest::new(vec![main, library]);

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("first");
    assert_eq!(run(&first), "125\n");

    // main now calls `extra`, which lives in the member nothing wanted before.
    compile(
        &scratch,
        "main.c",
        r#"
#include <stdio.h>
int helper(int n);
int other(int n);
int extra(int n);
int main(void) { printf("%d\n", helper(3) + other(4) + extra(1)); return 0; }
"#,
    );

    let second = scratch.join("second");
    link_to_file_in(&request, &second, &mut session).expect("second");
    assert_eq!(
        run(&second),
        "1126\n",
        "the new member was not extracted: a replayed extraction order"
    );
}

/// A resident session records what each object read so the next link can skip
/// relocating it. That is the one reuse that rewrites the *output bytes* from
/// somewhere other than the inputs, so it gets its own property: the binary
/// must not depend on how much of it was reused.
///
/// The failure this guards against is silent. Reused bytes that are subtly
/// stale still link, still run for most inputs, and differ from a correct
/// binary in a handful of words — exactly the shape of bug that a "the link
/// succeeded" test cannot see. So two sessions link the same inputs the same
/// number of times, differing only in whether relocation reuse was on, and the
/// bytes are compared.
#[test]
fn relocation_reuse_does_not_change_the_binary() {
    let scratch = Scratch::dir("session-reuse-identical").expect("scratch");
    let objects = inputs(&scratch);
    let cache = scratch.join("cache");

    let mut outputs = Vec::new();
    for (tag, resident) in [("plain", false), ("reusing", true)] {
        let mut session = Session::default();
        session.set_resident(resident);
        let request = LinkRequest::new(objects.clone()).cached_at(cache.clone());
        let mut last = None;
        // Three, not two: the first link records, the second reuses what it
        // recorded, and the third reuses across a link that itself reused.
        for round in 0..3 {
            let output = scratch.join(format!("{tag}-{round}"));
            link_to_file_in(&request, &output, &mut session).expect("the link succeeds");
            last = Some(output);
        }
        let output = last.expect("three links ran");
        assert_eq!(run(&output), "125\n", "{tag} produced a broken program");
        outputs.push(std::fs::read(&output).expect("output"));
        std::fs::remove_file(&cache).ok();
    }

    assert_eq!(
        outputs[0], outputs[1],
        "reusing relocations changed the binary"
    );
}

/// An input list that gains a member renumbers everything after it, and the
/// session is now allowed to survive that.
///
/// Object ids are positional. Until finding 144 the session threw everything
/// away whenever the input list changed at all, which made this safe by making
/// it impossible — and made a resident linker go cold on every real rebuild,
/// because rustc renames the objects of a recompiled crate every time. The
/// session now keeps what it can and `load_objects` serves a held parse only
/// under the id this link would assign it.
///
/// So the hazard is real and the guard is somewhere else, which is exactly the
/// arrangement that needs a test: link, then link again with an extra object
/// *first*, so every held id is off by one. The result must equal a cold link
/// of the same list.
#[test]
fn an_input_list_that_renumbers_still_links_correctly() {
    let scratch = Scratch::dir("session-renumber").expect("scratch");
    let held = inputs(&scratch);

    // Prepended, so `main.o` and the archive both move up one position.
    let extra = compile(
        &scratch,
        "extra.c",
        "int unused_leaf(int n) { return n; }\n",
    );
    let mut renumbered = vec![extra];
    renumbered.extend(held.iter().cloned());

    let cold = scratch.join("cold");
    link_to_file(&LinkRequest::new(renumbered.clone()), &cold).expect("the cold link succeeds");

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&LinkRequest::new(held), &first, &mut session).expect("the first link");
    let second = scratch.join("second");
    link_to_file_in(&LinkRequest::new(renumbered), &second, &mut session)
        .expect("the renumbered link");

    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold"),
        "a renumbered link differed from a cold one — a held parse was served \
         under an id that now means a different object"
    );
    assert_eq!(run(&second), "125\n");
}

/// The other half: when the list changes but a path stays put, the session
/// must actually keep it. Otherwise the guard above is enforced by discarding
/// everything, which is the behaviour finding 144 removed.
#[test]
fn an_input_that_kept_its_place_is_kept_across_a_changed_list() {
    let scratch = Scratch::dir("session-kept").expect("scratch");
    let held = inputs(&scratch);

    // Appended, so nothing already in the list is renumbered.
    let extra = compile(
        &scratch,
        "extra.c",
        "int unused_leaf(int n) { return n; }\n",
    );
    let mut grown = held.clone();
    grown.push(extra);

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&LinkRequest::new(held), &first, &mut session).expect("the first link");
    let second = scratch.join("second");
    let timings =
        link_to_file_in(&LinkRequest::new(grown), &second, &mut session).expect("the grown link");

    assert!(
        timings.inputs_held > 0,
        "a changed input list discarded every input that survived it: \
         {} held, {} read",
        timings.inputs_held,
        timings.inputs_read
    );
    assert_eq!(run(&second), "125\n");
}

/// Dead-stripping reuses the previous link's answer when no object's
/// projection into reachability moved, and the answer must be the same one.
///
/// This is the cheapest case of incremental liveness and the one whose failure
/// is quietest: an atom stripped that is still reachable produces a binary that
/// links, runs, and crashes somewhere unrelated. So the check is not "it was
/// faster" — it is that the bytes equal a cold link's, and that the program
/// still runs.
#[test]
fn a_reused_dead_strip_answer_equals_a_cold_one() {
    let scratch = Scratch::dir("session-strip-reuse").expect("scratch");
    let request = LinkRequest::new(inputs(&scratch)).dead_stripped(true);

    let cold = scratch.join("cold");
    link_to_file(&request, &cold).expect("the cold link succeeds");

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("the first link");
    let second = scratch.join("second");
    let timings = link_to_file_in(&request, &second, &mut session).expect("the second link");

    assert!(
        timings.reused_strip,
        "the second link recomputed dead-stripping although nothing moved, \
         so this proves nothing about reuse"
    );
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold"),
        "the reused strip produced a different binary from a cold link"
    );
    assert_eq!(run(&second), "125\n");
}

/// And it must stop reusing when an object's projection does move.
///
/// The edit adds a function and calls it, so the changed object's atoms and
/// edges both differ. If the digest missed that, the second link would strip
/// against the old graph.
#[test]
fn an_edit_that_moves_the_graph_is_not_served_the_old_answer() {
    let scratch = Scratch::dir("session-strip-invalidate").expect("scratch");
    let inputs = inputs(&scratch);
    let request = LinkRequest::new(inputs.clone()).dead_stripped(true);

    let mut session = Session::default();
    let first = scratch.join("first");
    link_to_file_in(&request, &first, &mut session).expect("the first link");

    // `helper` gains a call to a function that did not exist before, so its
    // atoms and its edges both change.
    let helper = compile(
        &scratch,
        "helper.c",
        "static int added(int n) { return n + 1; }\n\
         int helper(int n) { return added(n) * 7; }\n",
    );
    let other = compile(&scratch, "other.c", OTHER);
    let edited = vec![inputs[0].clone(), archive(&scratch, &[helper, other])];
    let request = LinkRequest::new(edited).dead_stripped(true);

    let second = scratch.join("second");
    let timings = link_to_file_in(&request, &second, &mut session).expect("the second link");
    assert!(
        !timings.reused_strip,
        "an object whose call graph changed was served the previous answer"
    );

    let cold = scratch.join("cold");
    link_to_file(&request, &cold).expect("the cold link succeeds");
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold"),
    );
    // helper(3) = (3 + 1) * 7 = 28, other(4) = 104.
    assert_eq!(run(&second), "132\n");
}

/// A session interns every symbol name it has ever seen and hands out ids in
/// the order it first saw them. Those ids are what resolution, the archive
/// frontier and the owners map are keyed by — so on the second program a
/// session links, every id is numbered by the *first* program's names.
///
/// That is a real hazard and not a hypothetical one: the order the frontier
/// wants names in decides which archive member is pulled first, which decides
/// what object id it gets, which reaches the output. Sorting by id instead of
/// by name would pass every test that links one program twice, because there
/// the two orders agree — ids are handed out in parse order, and the second
/// link parses nothing new.
///
/// So the warm-up below links `_other`'s definition ahead of `_helper`'s,
/// which is the opposite of their name order. It takes two objects to do it:
/// within one object the symbol table is already sorted, so intern order can
/// only be reversed across objects. After it `_other` holds the lower id, and
/// a frontier ordered by id asks the archive for its members back to front —
/// numbering them the other way round and moving every byte after them.
///
/// Verified to fail when the frontier sorts by id.
#[test]
fn an_interner_warmed_by_another_program_does_not_reorder_the_link() {
    let scratch = Scratch::dir("session-warm-interner").expect("scratch");

    let warm_other = compile(&scratch, "warm_other.c", OTHER);
    let warm_helper = compile(&scratch, "warm_helper.c", HELPER);
    let warm_main = compile(&scratch, "warm_main.c", MAIN);

    let mut session = Session::default();
    let warmup = scratch.join("warmup");
    link_to_file_in(
        // `_other` first, so it interns first.
        &LinkRequest::new(vec![warm_other, warm_helper, warm_main]),
        &warmup,
        &mut session,
    )
    .expect("the warmup link succeeds");
    assert_eq!(run(&warmup), "125\n");

    // Now the program under test, through the same session — whose interner
    // numbered `_other` below `_helper` — and through a fresh one.
    let request = LinkRequest::new(inputs(&scratch));
    let warm = scratch.join("warm");
    link_to_file_in(&request, &warm, &mut session).expect("the warm link succeeds");

    let cold = scratch.join("cold");
    link_to_file(&request, &cold).expect("the cold link succeeds");

    assert_eq!(
        std::fs::read(&warm).expect("warm"),
        std::fs::read(&cold).expect("cold"),
        "a session warmed by another program produced different bytes"
    );
    assert_eq!(run(&warm), "125\n");
}
