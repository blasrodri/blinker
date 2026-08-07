//! R1: how quickly does one already-classified DIRECT patch cluster become
//! executable?
//!
//! Five things and no more: Cranelift straight to machine code, one MAP_JIT
//! arena, stable gates, minimal immutable generations, and measurements. No
//! test runner, no async restart, no debug registration, no reclamation
//! policy, no IPC, no macros.
mod arena;
mod bench;
mod codegen;
mod generation;

fn main() {
    let iterations = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);
    bench::run(iterations);
}
