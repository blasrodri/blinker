# R1 — codegen and publication

**Strong GO on every axis that was measured.** Publication is 100× under its
target; the backend is an order of magnitude under its budget. One component
is still not isolated and is named in §6.

```
Apple M5 Pro, 15 cores, 48 GB, macOS 26.5.2
Cranelift 0.134.3 (direct `Context::compile`, not `cranelift_jit`)
rustc 1.99.0-nightly (dc3f85158) with rustc-codegen-cranelift-preview
```

## 1. What was built

Five things, and deliberately nothing else — no test runner, async restart,
debug registration, reclamation policy, IPC, or macros.

| | |
|---|---|
| `arena.rs` | one preallocated `MAP_JIT` reservation, per-thread W^X toggle, i-cache flush |
| `generation.rs` | immutable generations, stable gates, one atomic swap, code rollback |
| `codegen.rs` | Cranelift `Context::compile` → `CompiledCode` → relocated arena slab |
| `bench.rs` | the measurements below |
| tests | 10, including the concurrency property |

No `cranelift_jit`, no object file, no Mach-O in the path. `Context::compile`
hands back unrelocated bytes plus `buffer.relocs()`, which is the artifact the
eventual design wants.

## 2. Codegen — split three ways

Closures of 1, 4 and 6 functions, chained so the calls are real relocations.
200 iterations after 8 warm-ups.

| closure | code | relocs | CLIF build | **Cranelift** | extract |
|---|---:|---:|---:|---:|---:|
| 1 | 76 B | 0 | 0.001 ms | **0.019 ms** | 0.000 ms |
| 4 | 412 B | 3 | 0.005 ms | **0.082 ms** | 0.000 ms |
| 6 | 636 B | 5 | 0.008 ms | **0.123 ms** | 0.000 ms |

Cranelift compiles the six-function closure S0c classified DIRECT in **0.12
ms**. Extraction and relocation normalization do not register.

The split matters and it answers the question it was built to answer:
**Cranelift is not the thing to optimize.** If a future budget is tight, the
time is upstream of here.

## 3. Publication — the sub-millisecond claim

| | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| closure 6, precompiled → active | **0.001 ms** | 0.001 ms | **0.010 ms** | 0.010 ms |

Per step, p50, closure 6:

```
reserve 0.0000   copy 0.0002   relocate 0.0001
icache  0.0001   slots 0.0000  swap 0.0000
```

**p99 is 10 µs against a 1 ms target — a hundredfold margin.** The whole of
publication is a bump allocation, a 636-byte copy, five relocations, one
`sys_icache_invalidate`, and one `AtomicPtr::store`.

Compile *and* publish, end to end: **0.134 ms p50** for the six-function
closure.

## 4. The gate

**~1–2 ns per call** (1.0 ns for a single function; 11.2 ns across a
six-deep chain, so ~1.9 ns per link). The target was 20 ns. This is an
ordinary indirect call through a slot the branch predictor learns immediately.

No gate optimization was attempted, and on this evidence none is warranted
before a macro workload says otherwise.

## 5. cg_clif on a real crate

`blinker_diagnostics`, compiled two ways with `-Ztime-passes`:

| phase | LLVM | **Cranelift** |
|---|---:|---:|
| backend codegen | 227 ms | **17 ms** |
| total | 325 ms | **106 ms** |

cg_clif's entire backend, for the whole crate, is 17 ms — 13× less than
LLVM's. A four-to-six function patch closure is a small fraction of a crate,
so 17 ms is a loose upper bound on the backend cost of any closure inside it.

## 6. What is still not measured

**MIR → CLIF lowering, in isolation.** The `CLIF build` column above is CLIF
built by hand, and it is labelled a stand-in in the source because that is
what it is. Real lowering does trait resolution, layout computation, ABI
adaptation and drop glue. It is bounded above by the 17 ms whole-crate figure
in §5, and it is not separately isolated.

**The end-to-end pipeline.** Every component is measured; they have not been
wired into one process. §7 is therefore an addition of measured parts, not an
observed number, and it is labelled as such.

## 7. The budget, with R1's numbers in it

| component | source | library-crate edit |
|---|---|---:|
| rustc validation | S0b, measured | 15–20 ms |
| DIRECT classifier | S0c, measured | 0.04–0.14 ms |
| Path D closure | S0, measured | 0.25 ms |
| Cranelift codegen | **R1, measured** | **0.12 ms** |
| MIR → CLIF lowering | R1 §5, bounded | ≤ 17 ms |
| publication | **R1, measured** | **0.001–0.010 ms** |
| **sum of parts** | | **~16–37 ms** |

Against the cargo debug baselines S0b measured — 434 ms for blinker, 762 ms
for ripgrep — that is **12–47×**, with the range dominated by an upper bound
that is almost certainly loose.

## 8. The headline that is actually defensible

Not "1 ms compilation". The measured, defensible claim is:

> **Rust edits active in tens of milliseconds, independent of downstream
> rebuild blast radius. Machine-code publication itself is ~10 µs.**

The blast-radius independence is the part that is new, and it comes from
S0b/S0c rather than from R1: cargo spends 434–762 ms rebuilding every
dependent because an rlib changed; a DIRECT edit changes no dependent's code,
which the oracle checked by rebuilding them and comparing.

## 9. Next

The first genuinely end-to-end number: one process that takes a source edit
through rustc validation → DIRECT → Path D → cg_clif → arena → next call
returns the new value, on `blinker_diagnostics` and `grep_matcher`. Everything
it needs now exists and is measured; what remains is wiring, and the wiring is
where the estimate in §7 either holds or does not.
