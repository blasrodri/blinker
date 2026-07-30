//! `blinker-corpus` — gather real linker invocations and report on them.
//!
//! This tool produces the two remaining M0 deliverables:
//!
//! - **The argument inventory.** Build every fixture shape through blinker with
//!   recording on, then report every argument category observed and, crucially,
//!   every argument the classifier could not model. Support is meant to be
//!   driven by observed Rust workloads rather than by guessing at `ld64`, and
//!   this is the mechanism that turns that intent into a list.
//! - **The baseline timing report.** Link each fixture through the system
//!   linker and through blinker on the same machine, so every later
//!   performance claim has a same-machine baseline to be measured against.
//!
//! Usage:
//!
//! ```text
//! blinker-corpus gather [--out DIR] [--offline] [--keep]
//! blinker-corpus report --records DIR
//! blinker-corpus baseline [--repeat N] [--offline]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use blinker_test_support::{catalog, FixtureKind, Network};

mod inventory;
mod report;

use inventory::Inventory;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("gather") => cmd_gather(&args[1..]),
        Some("report") => cmd_report(&args[1..]),
        Some("baseline") => cmd_baseline(&args[1..]),
        Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            return std::process::ExitCode::SUCCESS;
        }
        Some(other) => Err(format!("unknown command: {other}\n\n{USAGE}")),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("blinker-corpus: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
blinker-corpus — gather and analyse real linker invocations

USAGE:
    blinker-corpus gather [--out DIR] [--offline] [--keep]
    blinker-corpus report --records DIR
    blinker-corpus baseline [--repeat N] [--offline]

COMMANDS:
    gather     Build every fixture through blinker, recording each invocation,
               then print the argument inventory.
    report     Re-print the inventory from an existing directory of records.
    baseline   Time each fixture's link through the system linker and through
               blinker, on this machine.

OPTIONS:
    --out DIR      Where to write records (default: ./corpus)
    --records DIR  Directory of records to analyse
    --repeat N     Timed builds per fixture (default: 3)
    --offline      Skip fixtures that need crates.io
    --keep         Keep going after a fixture fails to build
";

/// Parse `--flag value` and `--flag` from an argument slice.
struct Flags(BTreeMap<String, Option<String>>);

impl Flags {
    fn parse(args: &[String]) -> Self {
        let mut map = BTreeMap::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(name) = arg.strip_prefix("--") {
                let takes_value = args.get(i + 1).is_some_and(|next| !next.starts_with("--"));
                if takes_value {
                    map.insert(name.to_string(), Some(args[i + 1].clone()));
                    i += 2;
                } else {
                    map.insert(name.to_string(), None);
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        Flags(map)
    }

    fn present(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|v| v.as_deref())
    }
}

/// Locate the blinker binary next to this tool.
fn blinker_binary() -> Result<PathBuf, String> {
    let mut dir = std::env::current_exe().map_err(|e| e.to_string())?;
    dir.pop();
    let candidate = dir.join("blinker");
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(format!(
        "blinker binary not found at {} — run `cargo build` first",
        candidate.display()
    ))
}

/// Fixtures to run, honouring `--offline`.
fn selected(offline: bool) -> Vec<FixtureKind> {
    catalog()
        .into_iter()
        .filter(|k| !(offline && k.network == Network::Required))
        .collect()
}

fn cmd_gather(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args);
    let out = PathBuf::from(flags.value("out").unwrap_or("corpus"));
    let offline = flags.present("offline");
    let keep_going = flags.present("keep");
    let blinker = blinker_binary()?;

    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;

    let kinds = selected(offline);
    println!(
        "Gathering {} fixture(s) into {}\n",
        kinds.len(),
        out.display()
    );

    let mut failures = Vec::new();
    for kind in &kinds {
        print!("  {:<14} {:<52}", kind.tag, kind.exercises);
        let fixture = kind
            .build()
            .map_err(|e| format!("cannot create fixture {}: {e}", kind.tag))?;

        let record_arg = format!(
            "--blinker-record-invocation={}",
            fixture.recording_dir().display()
        );
        let build = fixture
            .build_with_linker(&blinker, &[record_arg])
            .map_err(|e| format!("cannot build fixture {}: {e}", kind.tag))?;

        if !build.success {
            println!("FAILED");
            failures.push((kind.tag, build.stderr.clone()));
            if !keep_going {
                break;
            }
            continue;
        }

        // Copy each record out of the fixture's temp dir, which is deleted when
        // the fixture drops.
        let mut copied = 0;
        for record in &build.recordings {
            let name = record.file_name().unwrap();
            let dest = out.join(format!("{}-{}", kind.tag, name.to_string_lossy()));
            std::fs::copy(record, &dest).map_err(|e| format!("cannot copy record: {e}"))?;
            copied += 1;
        }
        println!("{copied} link(s)");
    }

    if !failures.is_empty() {
        println!("\n{} fixture(s) failed to build:", failures.len());
        for (tag, stderr) in &failures {
            println!("\n--- {tag} ---");
            for line in stderr.lines().filter(|l| l.contains("error")).take(6) {
                println!("  {line}");
            }
        }
    }

    println!();
    let inventory = Inventory::from_records_dir(&out)?;
    report::print_inventory(&inventory);

    if !failures.is_empty() {
        return Err(format!("{} fixture(s) failed", failures.len()));
    }
    Ok(())
}

fn cmd_report(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args);
    let dir = flags
        .value("records")
        .ok_or("report requires --records DIR")?;
    let inventory = Inventory::from_records_dir(Path::new(dir))?;
    report::print_inventory(&inventory);
    Ok(())
}

fn cmd_baseline(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args);
    let offline = flags.present("offline");
    let repeat: usize = flags
        .value("repeat")
        .unwrap_or("3")
        .parse()
        .map_err(|_| "--repeat expects a number")?;
    let blinker = blinker_binary()?;

    let kinds = selected(offline);
    let mut rows = Vec::new();

    println!(
        "Timing {} fixture(s), {repeat} run(s) each — this rebuilds from clean each time\n",
        kinds.len()
    );

    for kind in &kinds {
        print!("  {:<14} ", kind.tag);
        let fixture = kind
            .build()
            .map_err(|e| format!("cannot create fixture {}: {e}", kind.tag))?;

        // Warm the dependency graph once so timings measure the build we care
        // about rather than a first-time crates.io fetch.
        let warmup = fixture
            .build_with_system_linker()
            .map_err(|e| format!("warmup failed for {}: {e}", kind.tag))?;
        if !warmup.success {
            println!("SKIPPED (build failed)");
            continue;
        }

        let mut system_ms = Vec::new();
        let mut blinker_ms = Vec::new();
        for _ in 0..repeat {
            fixture.clean().map_err(|e| e.to_string())?;
            let sys = fixture
                .build_with_system_linker()
                .map_err(|e| e.to_string())?;
            system_ms.push(sys.elapsed.as_secs_f64() * 1000.0);

            fixture.clean().map_err(|e| e.to_string())?;
            let bl = fixture
                .build_with_linker(&blinker, &[])
                .map_err(|e| e.to_string())?;
            blinker_ms.push(bl.elapsed.as_secs_f64() * 1000.0);
        }

        let sys = report::median(&mut system_ms);
        let bl = report::median(&mut blinker_ms);
        println!(
            "system {sys:>8.0} ms   blinker {bl:>8.0} ms   overhead {:+.1}%",
            (bl / sys - 1.0) * 100.0
        );
        rows.push(report::BaselineRow {
            tag: kind.tag.to_string(),
            system_ms: sys,
            blinker_ms: bl,
        });
    }

    println!();
    report::print_baseline(&rows);
    Ok(())
}
