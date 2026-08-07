//! The two numbers R1 exists to produce.
//!
//! **Precompiled publication**: an artifact already exists → active. This is
//! the sub-millisecond claim, and it is measured with the compile excluded
//! because a developer who edits twice in a row pays it twice while paying
//! the compile once.
//!
//! **Compile and publish**: CLIF → active. This is what threatens the product
//! budget, and it is reported next to the S0/S0b frontend numbers rather than
//! on its own.
//!
//! Reported at p50/p95/p99 over closures of 1, 4 and 6 functions, because S0c
//! established that the replaceable unit is a patch closure. A one-function
//! benchmark would answer a question the product does not ask.

use crate::arena::Arena;
use crate::codegen::{compile_closure, host_isa, load};
use crate::generation::{scope, Runtime};

#[derive(Debug, serde::Serialize)]
struct Row {
    closure: usize,
    code_bytes: usize,
    relocations: usize,
    // Codegen, split three ways so the answer says which half to optimize.
    clif_build_ms: Stat,
    cranelift_ms: Stat,
    extract_ms: Stat,
    // Publication, split into its steps for the same reason.
    publish_total_ms: Stat,
    reserve_ms: Stat,
    copy_ms: Stat,
    relocate_ms: Stat,
    icache_ms: Stat,
    slots_ms: Stat,
    swap_ms: Stat,
    compile_and_publish_ms: Stat,
    gate_call_ns: Stat,
}

#[derive(Debug, Default, serde::Serialize)]
struct Stat {
    p50: f64,
    p95: f64,
    p99: f64,
    min: f64,
    max: f64,
}

fn stat(values: &mut Vec<f64>) -> Stat {
    values.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let at = |fraction: f64| {
        let index = ((values.len() - 1) as f64 * fraction).round() as usize;
        values[index]
    };
    Stat {
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        min: values[0],
        max: values[values.len() - 1],
    }
}

pub fn run(iterations: usize) {
    let isa = host_isa();
    // Reserved once. Reserving inside the loop would put an `mmap` on the hot
    // path and measure the kernel rather than the publication.
    let arena = Arena::reserve(512 * 1024 * 1024).expect("arena");
    let mut rows = Vec::new();

    for closure in [1usize, 4, 6] {
        let runtime = Runtime::new(closure);
        let mut clif = Vec::new();
        let mut cranelift = Vec::new();
        let mut extract = Vec::new();
        let mut publish_total = Vec::new();
        let mut reserve = Vec::new();
        let mut copy = Vec::new();
        let mut relocate = Vec::new();
        let mut icache = Vec::new();
        let mut slots_ms = Vec::new();
        let mut swap = Vec::new();
        let mut end_to_end = Vec::new();
        let mut gate = Vec::new();
        let (mut code_bytes, mut relocations) = (0, 0);

        // Warm: the first compile pays for lazily-initialized Cranelift state
        // and the first arena page faults, neither of which a developer's
        // second edit pays.
        for _ in 0..8 {
            let (artifacts, _) = compile_closure(&*isa, closure);
            let (slots, slabs, _) = load(&arena, &artifacts).expect("load");
            runtime.publish(runtime.candidate(slots, slabs));
        }

        for _ in 0..iterations {
            let whole = std::time::Instant::now();
            let (artifacts, timings) = compile_closure(&*isa, closure);
            let (slots, slabs, publish) = load(&arena, &artifacts).expect("load");

            let candidate = runtime.candidate(slots, slabs);
            let at = std::time::Instant::now();
            runtime.publish(candidate);
            let swap_ms = at.elapsed().as_secs_f64() * 1e3;
            end_to_end.push(whole.elapsed().as_secs_f64() * 1e3);

            clif.push(timings.clif_build_ms);
            cranelift.push(timings.cranelift_ms);
            extract.push(timings.extract_ms);
            code_bytes = timings.code_bytes;
            relocations = timings.relocations;

            reserve.push(publish.reserve_ms);
            copy.push(publish.copy_ms);
            relocate.push(publish.relocate_ms);
            icache.push(publish.icache_ms);
            slots_ms.push(publish.slots_ms);
            swap.push(swap_ms);
            publish_total.push(publish.total_ms + swap_ms);

            // The gate: enter a scope, load the slot, call. Averaged over many
            // calls because one indirect call is below timer resolution.
            let calls = 2_000;
            let at = std::time::Instant::now();
            let sum = scope(&runtime, |generation| {
                let pointer = generation.implementation(0).expect("slot 0");
                // SAFETY: a compiled function of the benchmarked signature.
                let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(pointer) };
                (0..calls).fold(0i64, |acc, i| acc.wrapping_add(f(i)))
            });
            std::hint::black_box(sum);
            gate.push(at.elapsed().as_secs_f64() * 1e9 / calls as f64);
        }

        rows.push(Row {
            closure,
            code_bytes,
            relocations,
            clif_build_ms: stat(&mut clif),
            cranelift_ms: stat(&mut cranelift),
            extract_ms: stat(&mut extract),
            publish_total_ms: stat(&mut publish_total),
            reserve_ms: stat(&mut reserve),
            copy_ms: stat(&mut copy),
            relocate_ms: stat(&mut relocate),
            icache_ms: stat(&mut icache),
            slots_ms: stat(&mut slots_ms),
            swap_ms: stat(&mut swap),
            compile_and_publish_ms: stat(&mut end_to_end),
            gate_call_ns: stat(&mut gate),
        });
    }

    println!("\n  closure   code   relocs |      CLIF  cranelift  extract |    publish (p50 / p99)   |  compile+publish  |  gate");
    for row in &rows {
        println!(
            "  {:>7}  {:>5}B  {:>6} | {:>9.3} {:>10.3} {:>8.3} | {:>9.3} / {:<9.3} | {:>9.3} p50    | {:>5.1} ns",
            row.closure,
            row.code_bytes,
            row.relocations,
            row.clif_build_ms.p50,
            row.cranelift_ms.p50,
            row.extract_ms.p50,
            row.publish_total_ms.p50,
            row.publish_total_ms.p99,
            row.compile_and_publish_ms.p50,
            row.gate_call_ns.p50,
        );
    }
    println!("\n  publication steps, p50 ms (closure 6):");
    if let Some(row) = rows.iter().find(|r| r.closure == 6) {
        println!(
            "    reserve {:.4}  copy {:.4}  relocate {:.4}  icache {:.4}  slots {:.4}  swap {:.4}",
            row.reserve_ms.p50,
            row.copy_ms.p50,
            row.relocate_ms.p50,
            row.icache_ms.p50,
            row.slots_ms.p50,
            row.swap_ms.p50,
        );
        println!(
            "    publication p50 {:.3} ms   p95 {:.3}   p99 {:.3}   max {:.3}",
            row.publish_total_ms.p50,
            row.publish_total_ms.p95,
            row.publish_total_ms.p99,
            row.publish_total_ms.max,
        );
    }
    println!("\n  arena: {} of {} MB used, {} generations retained",
        arena.used() / 1_000_000, arena.capacity() / 1_000_000, "many");

    let json = serde_json::to_string_pretty(&rows).expect("serialize");
    let out = std::path::Path::new("results");
    std::fs::create_dir_all(out).expect("results dir");
    std::fs::write(out.join("r1.json"), json).expect("write");
    println!("  results/r1.json\n");
}
