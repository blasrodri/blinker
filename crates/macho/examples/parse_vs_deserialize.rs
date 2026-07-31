//! Is deserialising a `ParsedObject` faster than parsing the object again?
//!
//! This is the premise M4's parse cache rests on, and it is not obvious. A
//! `ParsedObject` is mostly `Vec<InputSymbol>`, and every symbol carries an
//! owned `String` name — so both paths allocate thousands of strings, and the
//! cache only wins if the Mach-O structure walking it skips costs more than
//! the decoding it adds.
//!
//! JSON is deliberately the *wrong* codec for a cache: it is among the slowest
//! options available. That makes it a useful bound. If JSON deserialisation is
//! already competitive with parsing, a compact binary format wins outright and
//! the cache is worth building. If JSON is many times slower, the answer is
//! ambiguous and the next step is to change what is stored rather than how.
//!
//! ```text
//! cargo run --release -p blinker-macho --example parse_vs_deserialize -- <object>...
//! ```

use blinker_macho::{parse_object, ObjectId};
use std::time::Instant;

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: parse_vs_deserialize <object>...");
        std::process::exit(2);
    }

    let mut loaded = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Ok(parsed) = parse_object(
            &data,
            std::path::Path::new(path),
            None,
            ObjectId(index as u32),
        ) else {
            continue;
        };
        let json = serde_json::to_vec(&parsed).expect("serializable");
        loaded.push((data, path.clone(), json));
    }

    let symbols: usize = loaded.len();
    let bytes: usize = loaded.iter().map(|(d, _, _)| d.len()).sum();
    let json_bytes: usize = loaded.iter().map(|(_, _, j)| j.len()).sum();
    println!(
        "{symbols} objects, {:.1} MB of Mach-O, {:.1} MB of JSON",
        bytes as f64 / 1e6,
        json_bytes as f64 / 1e6
    );

    const ROUNDS: u32 = 20;

    // Warm both paths so neither pays first-touch costs the other avoids.
    for (data, path, json) in &loaded {
        let _ = parse_object(data, std::path::Path::new(path), None, ObjectId(0));
        let _: blinker_macho::ParsedObject = serde_json::from_slice(json).expect("round trips");
    }

    let start = Instant::now();
    for _ in 0..ROUNDS {
        for (data, path, _) in &loaded {
            std::hint::black_box(
                parse_object(data, std::path::Path::new(path), None, ObjectId(0)).ok(),
            );
        }
    }
    let parse_ms = start.elapsed().as_secs_f64() * 1000.0 / ROUNDS as f64;

    let start = Instant::now();
    for _ in 0..ROUNDS {
        for (_, _, json) in &loaded {
            let value: blinker_macho::ParsedObject =
                serde_json::from_slice(json).expect("round trips");
            std::hint::black_box(value);
        }
    }
    let json_ms = start.elapsed().as_secs_f64() * 1000.0 / ROUNDS as f64;

    println!("\n  parse Mach-O      {parse_ms:6.2} ms");
    println!(
        "  deserialise JSON  {json_ms:6.2} ms   ({:.2}x parse)",
        json_ms / parse_ms
    );
    println!(
        "\n  JSON is a slow codec; a binary format is typically 5-10x faster.\n  \
         Implied binary deserialise: roughly {:.2}-{:.2} ms ({:.0}-{:.0}% of parse).",
        json_ms / 10.0,
        json_ms / 5.0,
        json_ms / 10.0 / parse_ms * 100.0,
        json_ms / 5.0 / parse_ms * 100.0
    );
}
