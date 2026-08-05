//! A link performed by the resident linker must be the link that would have
//! happened without it.
//!
//! The daemon's whole safety argument is that it changes *where* the work runs
//! and nothing else: the same argument vector, the same classification, the
//! same code. That is easy to say and easy to break — the working directory
//! moves, the argument vector travels through a socket, and the session
//! carries state from the previous link into this one.
//!
//! So this spawns a real daemon, links a real program through it, and compares
//! bytes with a link that never touched it.

use blinker_test_support::{blinker, workspace_binary, Scratch};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const MAIN: &str = r#"
#include <stdio.h>
int helper(int n);
int main(void) { printf("%d\n", helper(6)); return 0; }
"#;

const HELPER: &str = "int helper(int n) { return n * 7; }\n";

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

/// The driver arguments `cc` would hand a linker, near enough for a C program.
fn link_args(objects: &[PathBuf], output: &Path) -> Vec<String> {
    let mut argv = vec!["--blinker-internal".to_string()];
    for object in objects {
        argv.push(object.display().to_string());
    }
    argv.push("-o".to_string());
    argv.push(output.display().to_string());
    argv
}

/// A daemon of this test's own, and the directory its socket lives in.
///
/// Both halves are the point. Two tests sharing the ambient temporary
/// directory share a socket name, so one of them binds and the other exits —
/// and then the winner is killed by whichever test owns it while the other is
/// still using it. A short directory per test gives each its own daemon, and
/// short because a socket path has to fit in `sockaddr_un`.
struct Daemon {
    children: Vec<Child>,
    sockets: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.sockets);
    }
}

impl Daemon {
    /// Start one, and do not return until it is answering.
    ///
    /// Waiting is not politeness. A client that finds no daemon starts one and
    /// links in-process, so a test that raced its own daemon would measure the
    /// fallback and leave behind a second daemon that nothing owns.
    ///
    /// The whole set, not one. A client routes by output path, so which worker
    /// a link goes to is a hash the test does not control — and a worker that
    /// is not running is a client that falls back in process, silently passing
    /// every assertion below.
    fn start(tag: &str) -> Daemon {
        let sockets = PathBuf::from(format!("/tmp/bd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sockets);
        std::fs::create_dir_all(&sockets).expect("socket directory");
        let children = (0..blinker_cli::daemon::WORKERS)
            .map(|worker| {
                Command::new(workspace_binary("blinker"))
                    .env("TMPDIR", &sockets)
                    .arg(format!("--blinker-daemon-serve={worker}"))
                    .spawn()
                    .expect("the daemon starts")
            })
            .collect();
        let daemon = Daemon { children, sockets };

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !daemon.all_answering() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(daemon.all_answering(), "the daemons never came up");
        daemon
    }

    /// Whether every worker has bound and is answering.
    fn all_answering(&self) -> bool {
        let Ok(entries) = std::fs::read_dir(&self.sockets) else {
            return false;
        };
        let alive = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|kind| kind == "sock")
                    && blinker_cli::daemon::is_alive(path)
            })
            .count();
        alive == blinker_cli::daemon::WORKERS
    }

    /// Link through this daemon.
    ///
    /// The client falls back to linking in-process when no daemon answers,
    /// which is the right behaviour and the wrong thing here: a fallback would
    /// pass every assertion below while proving nothing. [`Daemon::start`] has
    /// already established that one is answering, and `TMPDIR` is what points
    /// the client at this test's rather than another's.
    fn link(&self, argv: &[String], output: &Path) -> bool {
        let _ = std::fs::remove_file(output);
        let mut full = vec!["--blinker-daemon".to_string()];
        full.extend(argv.iter().cloned());
        let status = Command::new(workspace_binary("blinker"))
            .env("TMPDIR", &self.sockets)
            .args(&full)
            .output()
            .expect("blinker runs");
        status.status.success() && output.exists()
    }
}

/// A distinct directory holding an output with a *fixed* name.
///
/// The output's base name is signed into the image — it becomes the code
/// signature's identifier — so `direct` and `served` differ by design and a
/// byte comparison between them proves nothing. Same name, different
/// directory, and the comparison means what it says.
fn output_in(scratch: &Scratch, directory: &str) -> PathBuf {
    let path = scratch.join(directory);
    std::fs::create_dir_all(&path).expect("directory created");
    path.join("program")
}

fn run(binary: &Path) -> String {
    let output = Command::new(binary).output().expect("the program runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The property: same bytes, whoever did the work.
#[test]
fn a_daemon_served_link_is_byte_identical() {
    let scratch = Scratch::dir("daemon-identical").expect("scratch");
    let objects = vec![
        compile(&scratch, "main.c", MAIN),
        compile(&scratch, "helper.c", HELPER),
    ];

    // `blinker()` and not a bare command: a link that engaged a daemon would
    // not be the direct half of this comparison.
    let direct = output_in(&scratch, "direct");
    let status = blinker()
        .args(link_args(&objects, &direct))
        .status()
        .expect("blinker runs");
    assert!(status.success(), "the direct link failed");

    let daemon = Daemon::start("identical");
    let served = output_in(&scratch, "served");
    assert!(
        daemon.link(&link_args(&objects, &served), &served),
        "the daemon never produced an output"
    );

    assert_eq!(
        std::fs::read(&served).expect("served"),
        std::fs::read(&direct).expect("direct"),
        "the daemon produced a different binary"
    );
    assert_eq!(run(&served), "42\n");
}

/// The same property for a dylib, which the session now has more state about.
///
/// A dylib link carries three things an executable's does not — the output
/// kind, the install name, and the export list — and all three reach the image
/// through a session that is keyed by request and reused across links. A warm
/// link that kept an executable's shape, or another target's exports, would
/// still produce a loadable library, and only a byte comparison against a cold
/// link says which one it is.
#[test]
fn a_daemon_served_dylib_is_byte_identical() {
    let scratch = Scratch::dir("daemon-dylib").expect("scratch");
    let object = compile(
        &scratch,
        "lib.c",
        "int helper(int x) { return x * 7; }\nint answer(void) { return helper(6); }\n",
    );
    let list = scratch.join("exports.txt");
    std::fs::write(&list, "_answer\n").expect("list written");

    let dylib_args = |output: &std::path::Path| {
        let mut argv = link_args(std::slice::from_ref(&object), output);
        argv.push("-dynamiclib".to_string());
        argv.push("-lSystem".to_string());
        argv.push("-Wl,-dead_strip".to_string());
        argv.push("-Wl,-exported_symbols_list".to_string());
        argv.push(format!("-Wl,{}", list.display()));
        // Fixed, because ld64 defaults it to the output path and the two
        // outputs below deliberately differ in directory: without this the
        // images would differ for a reason that is not the daemon.
        argv.push("-Wl,-install_name".to_string());
        argv.push("-Wl,/usr/local/lib/libanswer.dylib".to_string());
        argv
    };

    let direct = output_in(&scratch, "direct-dylib").with_file_name("libanswer.dylib");
    let status = blinker()
        .args(dylib_args(&direct))
        .status()
        .expect("blinker runs");
    assert!(status.success(), "the direct dylib link failed");

    let daemon = Daemon::start("dylib");
    let served = output_in(&scratch, "served-dylib").with_file_name("libanswer.dylib");
    // Twice: the first fills the session, the second is the one served from a
    // worker that already holds this target's state.
    for round in 0..2 {
        assert!(
            daemon.link(&dylib_args(&served), &served),
            "round {round}: the daemon never produced a dylib"
        );
        assert_eq!(
            std::fs::read(&served).expect("served"),
            std::fs::read(&direct).expect("direct"),
            "round {round}: the daemon produced a different library"
        );
    }
}

/// And the case the session makes possible: a second link through the same
/// daemon, after an edit, must see the edit.
///
/// A resident linker holds the previous parse of every input. An edit that the
/// session serves from memory is a build that silently ignores the change —
/// the worst failure this design can have, because the binary is valid and
/// wrong.
#[test]
fn a_second_link_through_the_daemon_sees_an_edit() {
    let scratch = Scratch::dir("daemon-edit").expect("scratch");
    let objects = vec![
        compile(&scratch, "main.c", MAIN),
        compile(&scratch, "helper.c", HELPER),
    ];

    let daemon = Daemon::start("edit");
    let first = output_in(&scratch, "first");
    assert!(
        daemon.link(&link_args(&objects, &first), &first),
        "the daemon never produced an output"
    );
    assert_eq!(run(&first), "42\n");

    compile(
        &scratch,
        "helper.c",
        "int helper(int n) { return n * 8; }\n",
    );
    let second = output_in(&scratch, "second");
    assert!(daemon.link(&link_args(&objects, &second), &second));
    assert_eq!(run(&second), "48\n", "the daemon served a stale object");

    // And it matches a linker with no memory of the first build.
    let cold = output_in(&scratch, "cold");
    let status = blinker()
        .args(link_args(&objects, &cold))
        .status()
        .expect("blinker runs");
    assert!(status.success());
    assert_eq!(
        std::fs::read(&second).expect("second"),
        std::fs::read(&cold).expect("cold"),
        "the warm link differs from a cold one"
    );
}

/// A link with no daemon running links anyway — and starts one.
///
/// Both halves matter. The first is the older promise: a resident linker is an
/// optimisation, and a build must not fail because nobody started one. The
/// second is what makes the optimisation reach anybody. A user's whole setup is
/// one line of `.cargo/config.toml`, so if the first link does not arrange for
/// the second to find a daemon, no link ever does.
///
/// `TMPDIR` moves the socket into the scratch directory, which is the only way
/// to observe a start without touching a daemon the developer running the test
/// is using — and the only way to be sure the one this starts is stopped again.
#[test]
fn a_link_with_no_daemon_links_and_starts_one() {
    let scratch = Scratch::dir("daemon-absent-link").expect("scratch");
    let objects = vec![
        compile(&scratch, "main.c", MAIN),
        compile(&scratch, "helper.c", HELPER),
    ];
    // Not inside the scratch directory: a Unix socket path must fit in
    // `sockaddr_un`, and the scratch directory's own name — under a macOS
    // temporary directory that is already fifty characters — leaves no room
    // for one. Short, and removed below.
    let sockets = PathBuf::from(format!("/tmp/bd{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sockets);
    std::fs::create_dir_all(&sockets).expect("socket directory");

    let output = scratch.join("program");
    let result = Command::new(workspace_binary("blinker"))
        .env("TMPDIR", &sockets)
        .env_remove("BLINKER_NO_DAEMON")
        .args(link_args(&objects, &output))
        .output()
        .expect("blinker runs");
    assert!(
        result.status.success(),
        "it refused to link without a daemon: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(run(&output), "42\n");

    let started = wait_for_socket(&sockets);
    let Some(socket) = started else {
        let _ = std::fs::remove_dir_all(&sockets);
        panic!(
            "the link did not start a daemon for the next one; it said:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    // Whatever else this test proves, it must not leave a process behind. A
    // daemon removes its socket on the way out, so the file going away is the
    // acknowledgement — and it happens just after the reply, not with it.
    blinker_cli::daemon::stop(&socket);
    let deadline = Instant::now() + Duration::from_secs(10);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let stopped = !socket.exists();
    let _ = std::fs::remove_dir_all(&sockets);
    assert!(stopped, "the daemon ignored the request to stop");
}

/// The socket a daemon binds, once it has bound it.
///
/// Starting is asynchronous by design — the link that starts a daemon does not
/// wait for it — so the file appears some milliseconds after the link returns.
fn wait_for_socket(directory: &Path) -> Option<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|kind| kind == "sock")
                    && blinker_cli::daemon::is_alive(&path)
                {
                    return Some(path);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}
