//! Link the same inputs repeatedly, so a sampling profiler has something to
//! sample.
//!
//! A link is 40-odd milliseconds. `sample` needs a process that stays alive
//! long enough to collect stacks from, and spawning the CLI in a shell loop
//! gives it a different pid every time. This keeps one process linking for a
//! fixed duration instead.
//!
//! It exists because the alternative was guessing. `cargo flamegraph` uses
//! `dtrace` on macOS, which needs root; `sample` needs nothing, but it needs a
//! target.
//!
//! ```text
//!   cargo build --release --example relink_loop
//!   ./target/release/examples/relink_loop <args-file> 20 &
//!   sample $! 15 -file /tmp/profile.txt
//! ```

use blinker_link::{link_to_file_in, LinkRequest, Session};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let list = args
        .next()
        .expect("usage: relink_loop <args-file> [seconds]");
    let seconds: u64 = args.next().map_or(20, |s| s.parse().expect("a number"));

    // The captured linker command line, one argument per line. Only the inputs
    // matter here; the flags that select an output kind are the CLI's business.
    let lines: Vec<String> = std::fs::read_to_string(&list)
        .expect("the args file is readable")
        .lines()
        .map(str::to_string)
        .collect();
    let inputs: Vec<PathBuf> = lines
        .iter()
        .filter(|line| line.ends_with(".o") || line.ends_with(".rlib") || line.ends_with(".a"))
        .map(PathBuf::from)
        .collect();
    assert!(!inputs.is_empty(), "no objects or archives in {list}");

    let out = PathBuf::from("/tmp/relink-loop-out");
    let request = LinkRequest::new(inputs).dead_stripped(true);

    // One session across every link, which is what a resident linker has and
    // a process per link cannot.
    let mut session = Session::default();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut links = 0u64;
    let mut last = None;
    while Instant::now() < deadline {
        last = Some(link_to_file_in(&request, &out, &mut session).expect("the link succeeds"));
        links += 1;
    }
    if let Some(timings) = last {
        println!(
            "  inputs: {} held, {} read   read+parse {:.2} ms   link {:.2} ms",
            timings.inputs_held, timings.inputs_read, timings.read_and_parse_ms, timings.total_ms
        );
    }
    // Printed rather than returned: this is a tool, and the count is how you
    // tell a profile of the linker from a profile of a process that spent the
    // window failing.
    println!("{links} links in {seconds}s");
}
