//! `blinker-inspect` — dump what blinker parsed out of a Mach-O object.
//!
//! Exists to be checked against `nm` and `otool`. A parser that is confidently
//! wrong is worse than one that fails loudly, and the only way to know which we
//! have is to compare counts against tools that were right first.
//!
//!     blinker-inspect <file.o>...            # summary per file
//!     blinker-inspect --counts <file.o>...   # machine-checkable counts
//!     blinker-inspect --relocs <file.o>...   # relocation census

use blinker_macho::{parse_object_file, ObjectId, SectionKind};
use std::collections::BTreeMap;
use std::path::Path;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let counts_only = args.iter().any(|a| a == "--counts");
    let relocs_only = args.iter().any(|a| a == "--relocs");
    let paths: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    if paths.is_empty() {
        eprintln!("usage: blinker-inspect [--counts|--relocs] <file.o>...");
        return std::process::ExitCode::FAILURE;
    }

    let mut census: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut failures = 0;

    for path in &paths {
        let object = match parse_object_file(Path::new(path), ObjectId(0)) {
            Ok(object) => object,
            Err(err) => {
                eprintln!("blinker-inspect: {err}");
                failures += 1;
                continue;
            }
        };

        if relocs_only {
            for relocation in &object.relocations {
                *census.entry(relocation.kind.name()).or_default() += 1;
            }
            continue;
        }

        if counts_only {
            // Deliberately terse and stable: this is what the cross-check
            // script diffs against `nm` and `otool`.
            let defined = object
                .symbols
                .iter()
                .filter(|s| s.strength.is_definition())
                .count();
            let undefined = object.undefined_symbols().count();
            println!(
                "{path} sections={} symbols={} defined={defined} undefined={undefined} relocations={}",
                object.sections.len(),
                object.symbols.len(),
                object.relocations.len()
            );
            continue;
        }

        println!("=== {path} ===");
        println!(
            "  {:?}, {} bytes, debug={} unwind={}",
            object.architecture,
            object.metadata.file_size,
            object.metadata.has_debug_info,
            object.metadata.has_unwind_info
        );

        println!("  sections: {}", object.sections.len());
        for section in &object.sections {
            let relocations = object.relocations_for(section.id).len();
            println!(
                "    {:<28} {:<14} size={:<8} align={:<4} relocs={}",
                section.qualified_name(),
                format!("{:?}", section.kind),
                section.size,
                section.alignment,
                relocations
            );
        }

        let mut by_strength: BTreeMap<String, usize> = BTreeMap::new();
        for symbol in &object.symbols {
            *by_strength
                .entry(format!("{:?}", symbol.strength))
                .or_default() += 1;
        }
        println!("  symbols: {}", object.symbols.len());
        for (strength, count) in &by_strength {
            println!("    {strength:<16} {count}");
        }

        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        for relocation in &object.relocations {
            *by_kind.entry(relocation.kind.name()).or_default() += 1;
        }
        println!("  relocations: {}", object.relocations.len());
        for (kind, count) in &by_kind {
            println!("    {kind:<34} {count}");
        }

        let code: u64 = object
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Code)
            .map(|s| s.size)
            .sum();
        println!("  code bytes: {code}");
    }

    if relocs_only {
        let total: usize = census.values().sum();
        println!("relocation census over {} file(s):", paths.len());
        for (kind, count) in &census {
            println!(
                "  {kind:<34} {count:>8}  {:>5.1}%",
                *count as f64 / total as f64 * 100.0
            );
        }
        println!("  {:<34} {total:>8}", "TOTAL");
    }

    if failures > 0 {
        eprintln!("{failures} file(s) failed to parse");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
