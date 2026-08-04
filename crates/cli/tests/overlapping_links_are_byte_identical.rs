//! Links that overlap must produce what links that did not would have.
//!
//! This is the test the concurrent daemon exists to survive. Four clients
//! submit at once, from four different directories, naming their inputs and
//! their output *relatively* — and every directory uses the same names. If a
//! link ever resolved a relative path against another link's directory it
//! would read that link's `main.o`, write that link's `prog`, and sign it with
//! that link's identifier: a complete, valid, running binary that is the wrong
//! program. Nothing about that failure is visible except by comparing bytes,
//! which is what this does.
//!
//! That shape is why the workers are processes. A working directory belongs to
//! a process, so four of them hold four directories; four threads would hold
//! one between them.
//!
//! One client passes its objects through an `@response` file containing
//! relative names, because response files are expanded by the server against
//! the request's directory rather than by the client — a second place the same
//! mistake can be made, and a later one. (`ld64` has no linker scripts, so
//! there is no third: `INPUT`/`INCLUDE` are GNU `ld` and do not exist here.)

use blinker_test_support::{workspace_binary, Scratch};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Distinct programs, so a crossed link is a different binary rather than a
/// coincidence. The constant is the only difference and it reaches the output.
fn source(seed: u32) -> String {
    format!(
        "#include <stdio.h>\nint helper(void);\n\
         int main(void) {{ printf(\"%d\\n\", helper()); return 0; }}\n\
         int helper(void) {{ return {seed} * 7; }}\n"
    )
}

/// One program in its own directory, under names every other program shares.
struct Program {
    directory: PathBuf,
    /// Arguments as the client will pass them: relative, resolved by whoever
    /// performs the link against whatever directory it believes it is in.
    argv: Vec<String>,
}

impl Program {
    fn output(&self) -> PathBuf {
        self.directory.join("prog")
    }
}

fn compile(directory: &Path, seed: u32) {
    std::fs::create_dir_all(directory).expect("directory");
    let file = directory.join("main.c");
    std::fs::write(&file, source(seed)).expect("source");
    let status = Command::new("cc")
        .args(["-c", "-o"])
        .arg(directory.join("main.o"))
        .arg(&file)
        .status()
        .expect("cc runs");
    assert!(status.success(), "compiling seed {seed} failed");
}

/// `response` makes this program name its objects through an `@` file.
fn program(directory: PathBuf, seed: u32, response: bool) -> Program {
    compile(&directory, seed);
    let mut argv = vec!["--blinker-internal".to_string()];
    if response {
        // Relative inside the file as well as outside it: the name the server
        // reads has to be resolved twice, and both against the client.
        std::fs::write(directory.join("objects.rsp"), "main.o\n").expect("response file");
        argv.push("@objects.rsp".to_string());
    } else {
        argv.push("main.o".to_string());
    }
    argv.push("-o".to_string());
    argv.push("prog".to_string());
    Program { directory, argv }
}

/// One program per worker, so all four are busy at once.
///
/// Routing is a hash of the output path, and a scratch directory is different
/// every run: four programs dropped anywhere would land on one worker roughly
/// one run in sixty, and that run would serialise and still pass. Choosing
/// directories until each worker owns one makes the concurrency a property of
/// the test rather than of the day.
fn one_program_per_worker(root: &Path) -> Vec<Program> {
    let mut chosen: Vec<Option<PathBuf>> = vec![None; blinker_cli::daemon::WORKERS];
    let mut candidate = 0;
    while chosen.iter().any(Option::is_none) {
        let directory = root.join(format!("p{candidate}"));
        candidate += 1;
        assert!(candidate < 200, "no directory set covers every worker");
        let output = directory.join("prog").display().to_string();
        let slot = blinker_cli::daemon::worker_of(&["-o".to_string(), output]);
        if chosen[slot].is_none() {
            chosen[slot] = Some(directory);
        }
    }
    chosen
        .into_iter()
        .enumerate()
        .map(|(worker, directory)| {
            let last = worker + 1 == blinker_cli::daemon::WORKERS;
            program(
                directory.expect("a directory per worker"),
                worker as u32 + 1,
                last,
            )
        })
        .collect()
}

/// The whole set of workers, in a temporary directory of this test's own.
///
/// Every worker, not one: a client routes by output path, and a worker that is
/// not running is a client that quietly links in process — which would pass
/// every assertion here while testing nothing.
struct Workers {
    children: Vec<Child>,
    sockets: PathBuf,
}

impl Drop for Workers {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.sockets);
    }
}

impl Workers {
    fn start() -> Workers {
        let sockets = PathBuf::from(format!("/tmp/bo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sockets);
        std::fs::create_dir_all(&sockets).expect("socket directory");
        let children = (0..blinker_cli::daemon::WORKERS)
            .map(|worker| {
                Command::new(workspace_binary("blinker"))
                    .env("TMPDIR", &sockets)
                    .arg(format!("--blinker-daemon-serve={worker}"))
                    .spawn()
                    .expect("a worker starts")
            })
            .collect();
        let workers = Workers { children, sockets };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !workers.all_answering() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(workers.all_answering(), "the workers never came up");
        workers
    }

    fn all_answering(&self) -> bool {
        let Ok(entries) = std::fs::read_dir(&self.sockets) else {
            return false;
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|kind| kind == "sock")
                    && blinker_cli::daemon::is_alive(path)
            })
            .count()
            == blinker_cli::daemon::WORKERS
    }

    /// Submit every program at once and wait for all of them.
    ///
    /// Spawned before any is waited on, which is the only part that matters:
    /// waiting between them would serialise the very thing under test.
    fn link_together(&self, programs: &[Program], trace: &Path) {
        let running: Vec<Child> = programs
            .iter()
            .map(|program| {
                Command::new(workspace_binary("blinker"))
                    .current_dir(&program.directory)
                    .env("TMPDIR", &self.sockets)
                    .env("BLINKER_TRACE_WAIT", trace)
                    .arg("--blinker-daemon")
                    .args(&program.argv)
                    .spawn()
                    .expect("blinker runs")
            })
            .collect();
        for (program, mut child) in programs.iter().zip(running) {
            let status = child.wait().expect("blinker exits");
            assert!(
                status.success(),
                "the link in {} failed",
                program.directory.display()
            );
            assert!(program.output().exists(), "no output was written");
        }
    }
}

/// Link alone, in this process, with no daemon anywhere near it.
fn link_cold(program: &Program) -> Vec<u8> {
    let status = Command::new(workspace_binary("blinker"))
        .current_dir(&program.directory)
        .arg("--blinker-no-daemon")
        .args(&program.argv)
        .status()
        .expect("blinker runs");
    assert!(status.success(), "the cold link failed");
    let bytes = std::fs::read(program.output()).expect("cold output");
    std::fs::remove_file(program.output()).expect("cold output removed");
    bytes
}

#[test]
fn four_links_at_once_are_each_the_link_they_would_have_been_alone() {
    let scratch = Scratch::dir("overlapping").expect("scratch");
    // Resolved, because the client's key is built from `current_dir`, which
    // the kernel returns with symlinks already gone: on macOS a scratch under
    // `/tmp` is reached by the client as `/private/tmp`. Asking about the
    // unresolved spelling would predict a different worker than the one the
    // link actually goes to — which is the routing property being tested, and
    // it is worth having the test depend on it rather than paper over it.
    let root = scratch.join("").canonicalize().expect("scratch resolved");
    // One per worker, the last of them through a response file. Everything
    // else about that one is identical, so a difference between it and its
    // cold link is about the expansion and nothing else.
    let programs = one_program_per_worker(&root);

    // Cold first, and each on its own: this is the answer the concurrent run
    // has to reproduce, produced the way it would be with no daemon at all.
    let cold: Vec<Vec<u8>> = programs.iter().map(link_cold).collect();
    // Distinct by construction — if two programs ever linked to the same bytes
    // the comparison below could not tell a crossed link from a correct one.
    for (i, left) in cold.iter().enumerate() {
        for right in &cold[i + 1..] {
            assert_ne!(left, right, "two programs are indistinguishable");
        }
    }

    let workers = Workers::start();
    // Every link a client hands to a daemon appends a line here. Without it a
    // client that found no worker would link in process, produce exactly these
    // bytes, and prove nothing at all — which is the failure this whole file
    // is arranged to make impossible.
    let trace = scratch.join("served.trace");
    // Twice: the first round fills the sessions, the second is the one where a
    // worker holds another target's retained state while serving this one.
    for round in 0..2 {
        workers.link_together(&programs, &trace);
        for (program, expected) in programs.iter().zip(&cold) {
            let served = std::fs::read(program.output()).expect("served output");
            assert_eq!(
                &served,
                expected,
                "round {round}: the concurrent link of {} is not the link it would have been alone",
                program.directory.display()
            );
            std::fs::remove_file(program.output()).expect("served output removed");
        }
        // One traced line per link, every round, or somebody fell back.
        let served = std::fs::read_to_string(&trace).unwrap_or_default();
        let lines: Vec<&str> = served.lines().collect();
        assert_eq!(
            lines.len(),
            programs.len() * (round + 1),
            "round {round}: {} links were served by a daemon, not {}",
            lines.len(),
            programs.len() * (round + 1)
        );
        // And by different workers: same-worker links serialise, so a run in
        // which every target routed to one process would pass every byte
        // comparison above without two links ever overlapping.
        let workers_used: std::collections::BTreeSet<&str> = lines
            .iter()
            .filter_map(|line| line.split(' ').nth(4))
            .collect();
        assert_eq!(
            workers_used.len(),
            blinker_cli::daemon::WORKERS,
            "round {round}: the links were served by {workers_used:?}, not by every worker"
        );
    }
}
