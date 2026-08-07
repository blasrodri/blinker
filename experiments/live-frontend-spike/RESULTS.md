# Spike S0 — Rust frontend lower bound

**S0c is complete and DIRECT holds on this corpus** (§16–§19).
**The end-to-end pipeline runs: a real source edit becomes live machine code
in 33–75 ms against a 434–762 ms cargo baseline** (§22–§25).

**Recommendation: A for an edit to a library crate, B for an edit to a large
binary crate** — the envelope must be stated over the crate containing the hot
function, not over the project. S0 alone recommended B; S0b (§12) measured the
case S0 had flagged as its largest gap and moved the common topology into A.

---

## 1. Machine

```
Apple M5 Pro, 15 cores, 48 GB, macOS 26.5.2, APFS
rustc 1.99.0-nightly (dc3f85158 2026-07-26)   toolchain nightly-2026-07-27
cg_clif: not used — S0 generates no code
```

No rustc patch was applied. Path D is implemented entirely against public
`rustc_private` APIs (`optimized_mir`, `instance_mir`, `Instance::try_resolve`),
which is itself a result: the algorithm V2 §5 expected might need "a tiny pinned
rustc/cg_clif patch" needs none.

## 2. Fixtures

| | crate compiled | what it is | whole-crate mono items |
|---|---|---|---:|
| `small` | `spike_small` | generated, 300 functions, **no dependencies** | 43 |
| `medium` | `blinker` | this repo's binary crate, atop a 17-crate workspace | 143 |
| `large` | `rg` | ripgrep's binary crate, atop a 12-crate workspace | 10 633 |

A hot root of identical shape is injected into each, so an edit class means the
same thing in all three. `medium` and `large` are compiled by **the exact rustc
invocation cargo runs**, captured from `cargo build -v` and replayed — externs,
cfgs, edition and all.

`medium` is a smaller *compilation* than `large` despite `blinker` being the
larger project, because `crates/cli/src/main.rs` is a thin binary over a
library. That is not a flaw in the fixture; it is the headline finding (§5).

## 3. What was measured

Every path runs the full analysis phase — resolution, type check, trait
selection, borrow check — then forces `optimized_mir`, the MIR a codegen
backend consumes. Nothing is reported as "MIR ready" that cg_clif could not
compile. `required_frontend` is `expand + analysis + hot MIR + Path D closure +
ABI/layout`; whole-crate collection is excluded because it is the control, not
a cost the product would pay.

`expand` is an inclusive scope: it runs from session start through macro
expansion and therefore contains rustc's own startup and the load of the
incremental dependency graph. Those could not be separated reliably and are not
reported as if they had been (V2 §8).

## 4. Results — 30 warm iterations per cell, alternating real edits

All figures milliseconds.

| fixture | edit | expand p50 | analysis p50 | **frontend p50** | **p95** | p99 |
|---|---|---:|---:|---:|---:|---:|
| small | body_arith | 5.8 | 3.4 | **9.5** | 10.4 | 10.6 |
| small | body_existing_call | 5.7 | 3.4 | **9.3** | 11.7 | 32.4 |
| small | body_new_generic | 5.8 | 3.7 | **10.4** | 12.5 | 18.0 |
| small | signature | 5.8 | 3.6 | **9.7** | 14.4 | 15.7 |
| small | type_layout | 5.9 | 8.8 | **14.9** | 15.6 | 15.9 |
| medium | body_arith | 8.0 | 2.5 | **10.7** | 15.5 | 19.5 |
| medium | body_existing_call | 7.8 | 2.4 | **10.5** | 11.0 | 11.1 |
| medium | body_new_generic | 7.6 | 2.5 | **11.3** | 14.6 | 23.8 |
| medium | signature | 7.6 | 2.6 | **10.5** | 11.2 | 11.3 |
| medium | type_layout | 8.3 | 6.7 | **15.2** | 21.0 | 30.8 |
| large | body_arith | 39.9 | 29.1 | **68.9** | 109.6 | 119.6 |
| large | body_existing_call | 35.1 | 26.4 | **62.1** | 70.9 | 84.9 |
| large | body_new_generic | 34.6 | 26.5 | **61.7** | 75.1 | 104.7 |
| large | signature | 34.3 | 25.9 | **60.8** | 102.2 | 141.4 |
| large | type_layout | 35.4 | 32.9 | **69.0** | 93.8 | 121.4 |

### Path C against Path D

| fixture | Path C (whole crate) | Path D (hot root) | items C | items D | ratio |
|---|---:|---:|---:|---:|---:|
| small | 4.4 ms | 0.24 ms | 43 | 4–6 | 18× |
| medium | 18.2 ms | 0.31 ms | 143 | 4–6 | 59× |
| large | 363 ms | 0.25 ms | 10 633 | 4–6 | **1471×** |

Path D distinguishes the edit classes exactly as designed: 4 instances for an
arithmetic edit, 5 when the body calls an existing function, 6 when it
introduces `spike_convert::<u32>`. It finds the new monomorphization and
nothing else.

### ABI and layout (§7)

**0.001–0.002 ms** on every fixture and every edit. Establishing the signature,
calling convention and parameter/return layouts of the hot root is free once
analysis has run.

### Process startup

`--noop` — start the process, load 150 MB of rustc dylibs, exit — costs
**10 ms** warm (260 ms on the first cold run of a boot).

## 5. What the numbers say

**1. Path D succeeds completely, and it is not where the time goes.** The
experiment V2 §5 called "the most important" works, needs no rustc patch, and
costs a quarter of a millisecond against 363 ms for the whole-crate route. It
also cannot rescue the product on its own, because it removes 363 ms from a
path that still costs 62 ms of analysis that nothing can skip: the edit has to
be *validated* before it can be published, and validation is type-checking the
crate. **The mono-discovery problem is solved; the analysis cost is the
product's actual constraint.**

**2. The determining variable is the size of the crate you edit, not the size
of the project.** `blinker` is the larger project — 17 crates, 50 000 lines —
and costs 10.5 ms, because the crate holding the hot root is a thin binary.
ripgrep costs 62 ms because `crates/core/main.rs` is a large crate. A latency
envelope for L1 must therefore be stated over **the crate containing the hot
function**, not over the workspace. That is a property a developer and an agent
can both influence, and one the product can measure and report.

**3. Residency buys about 10 ms.** Process startup is 10 ms warm, so V2's Path
B — repeated sessions in one process — saves roughly a sixth of the `large`
budget and half the `medium` one. Real, worth having, and much smaller than the
architecture implied. The expensive thing a persistent process could retain is
compiler *state*, and `TyCtxt` does not survive a session, which S0 does not
attempt to change.

**4. The layout-change control behaves as the classifier predicts.** The edit
that must fall back is also the most expensive to analyse — 8.8 ms against 3.4
on `small`, 32.9 against 29.1 on `large`. Rejection is not cheaper than
acceptance, so an L3 fallback pays the full frontend cost before it learns it
must fall back.

## 6. Against V2 §1's decision bands

| fixture | p50 | p95 | band |
|---|---:|---:|---|
| medium (`blinker` bin) | 10.5 | 11–21 | **A** (≤30 / ≤60) |
| large (`rg` bin) | 61–69 | 71–110 | **B** (30–150 / 60–300) |

V2 §1 grants Outcome A on "at least one realistic medium/large fixture", and
`medium` satisfies that as written. **The recommendation is nonetheless B.**

Taking the fixture that passes as decisive is precisely the error that produced
findings 230 through 241 in this repository: five consecutive bugs whose common
cause was a corpus that could not express the failing shape. A product cannot
choose which crate its user edits. `large` is the honest bound, and `large` is
Outcome B.

It is a good B. 62 ms of frontend on the worst fixture is far better than a
150–500 ms warm loop, and it leaves room for codegen and publication inside a
100 ms envelope for many real edits.

## 7. A prediction of mine that the measurement refuted

Reviewing V1, I claimed the spec's illustrative `frontend_ms: 12.4` was
"optimistic by an order of magnitude for anything but a toy crate". Measured:
10.5 ms on a real binary crate atop a 17-crate workspace, and 62 ms on a large
one. The figure was accurate for the first case and about 5× optimistic for the
second. rustc's incremental frontend is considerably faster than I asserted.

## 8. Known limitations as of S0 — the first one is answered in §12

**S0 measured a single-crate edit.** In every fixture the edited file belongs to
the crate being compiled, so the measurement covers one compilation. A real edit
to a library crate forces recompilation of every crate downstream of it: this
repository's own relink harness reports a 13-input blast radius for a one-line
edit to `pulsevm`, and 70 inputs for `blinker`. **The 62 ms figure is a lower
bound for the easiest topology, not the expected case**, and the expected case
could be several times larger.

This is the single most important thing S0 did not answer, and it should be
answered before R1 is begun rather than after.

**Answered in §12, and the prediction above is wrong.**

Other limitations, none of which changes the recommendation:

- **No `pulsevm` fixture.** Its C++ FFI build script fails under `cmake` in a
  fresh clone. Its Rust binary crate is 1093 lines atop 23 crates, so it would
  most likely land near `large`. Not measured, so not claimed.
- **The classifier was costed, not implemented.** §7's facts are gathered and
  timed; they are not compared before and after an edit, so S0 shows that
  computing them is free and does not show that the L1/L2/L3 decision is
  correct.
- **`expand` is an inclusive scope**, containing rustc startup and the
  incremental dep-graph load.
- **One machine, one project shape.** Three fixtures, all built by one compiler
  on one host.

## 9. An error in this harness, recorded because the first numbers were wrong

The driver originally returned `Compilation::Stop` after analysis — there was
no reason to let codegen run, and everything being measured was already
recorded. rustc writes its dependency graph when a session *completes*; a
session that stops leaves `s-…-working` and finalizes nothing. Every iteration
therefore started cold.

Those numbers were **200 ms of analysis on `large`, flat across four
consecutive iterations of the same edit** — five times the true figure, and
stable enough to look like a result rather than an artefact. The flatness was
the tell: an incremental compiler that reuses nothing is one that saved
nothing. Letting compilation finish costs wall time and no accuracy, because
every phase is timed before the callback returns.

Had this not been caught, S0 would have recommended C.

## 10. Proposed next step

Not R1. **S0b: the blast radius.** Measure source-edit-to-validated-MIR when
the edit lands in a library crate with dependents, across all three fixtures,
using the same captured-invocation replay extended to a sequence. If that lands
under ~150 ms the product's shape is settled and R1 follows as written. If it
lands at 500 ms, the L1 envelope has to be defined over edits to leaf crates,
and that is a product decision rather than an engineering one.

## 11. Reproducing

```sh
cd experiments/live-frontend-spike
./fixtures.py                                     # the generated fixture
./capture.py --name medium --project <repo> --package blinker-cli \
    --target blinker --file crates/cli/src/main.rs
./run.py --fixtures small medium large --iterations 30
```

Raw per-iteration records are in `results/*.json`; `results/machine.json` holds
the host metadata.


---

# S0b — the blast radius

S0's §8 named one gap as decisive: every fixture edited the crate being
compiled, which is the easiest topology. This measures the common one — an edit
to a library crate that other crates depend on.

## 12. What an edit to a library costs

Two real leaf libraries, injected with the same hot root, edited through the
same five classes.

| | crate edited | crates cargo rebuilds |
|---|---|---:|
| `blinker-lib` | `blinker_diagnostics` | 2 (`blinker-diagnostics`, `blinker-cli`) |
| `rg-lib` | `grep_matcher` | 6 (`grep-matcher` → `grep-searcher` → `grep-regex` → `grep-printer` → `grep` → `ripgrep`) |

### Validating only the edited crate (the Live lower bound)

| fixture | edit | p50 ms | p95 ms | instances |
|---|---|---:|---:|---:|
| blinker-lib | body_arith | **19.5** | 20.8 | 4 |
| blinker-lib | body_existing_call | 20.4 | 23.1 | 5 |
| blinker-lib | body_new_generic | 21.1 | 25.2 | 6 |
| blinker-lib | signature | 20.8 | 23.0 | 4 |
| blinker-lib | type_layout | 24.0 | 24.9 | 4 |
| rg-lib | body_arith | **14.7** | 16.0 | 4 |
| rg-lib | body_existing_call | 14.2 | 15.4 | 5 |
| rg-lib | body_new_generic | 15.1 | 16.3 | 6 |
| rg-lib | signature | 15.1 | 17.2 | 4 |
| rg-lib | type_layout | 17.9 | 20.2 | 4 |

### What a developer pays today

`cargo build` of the binary at the top of the graph, alternating real edits:

| fixture | debug | release |
|---|---:|---:|
| blinker-lib | **434 ms** | 9 855 ms |
| rg-lib | **762 ms** | 3 038 ms |

Debug is the honest baseline: nobody runs an edit–test loop in release. The
first version of this measured only release and reported a 512× ratio, which is
a fact about optimization levels and not about this product.

## 13. What S0b says

**1. My §8 prediction was wrong, in the favourable direction.** I wrote that 62
ms was "a lower bound for the easiest topology" and that the expected case
"could be several times larger". It is *smaller*: 14.7–19.5 ms, against 62 ms
for the large binary crate. The reason is S0's finding 2 restated — the cost is
set by the size of the crate you edit, and leaf libraries are small crates. The
blast radius is expensive for **cargo**, which must rebuild every dependent
because an rlib changed; it is not expensive for **validation**, because
nothing downstream changed semantically.

**2. The product's value on this edit class is 22–52×, measured.** 434 ms
against 19.5, and 762 against 14.7. V2 §10.5 struck the 10× claim until a
baseline existed; this is that baseline, and it supports considerably more than
10× — *provided* the two exclusions below are honoured.

**3. The whole gap is contingent on a component that does not exist.** The
19.5 ms number is only reachable if the runtime can *prove* the crate's
exported interface is unchanged, and therefore that no dependent needs
revalidating. The `signature` and `type_layout` rows above are measured at the
same cost as the others, but they are edits that **must** fall back to the full
rebuild — and the classifier that tells them apart was costed by S0 at 0.002 ms
and never implemented. **The critical path to the product is the classifier,
not the JIT.**

**4. Two things are excluded from the Live figure and must be added before any
end-to-end claim**: cg_clif codegen of the changed function, and publication.
S0 measured neither. A realistic live total is the 15–20 ms here plus both.

## 14. Revised recommendation

| topology | p50 | p95 | band |
|---|---:|---:|---|
| edit to a library crate — `blinker-lib`, `rg-lib` | 14.7–19.5 | 16–21 | **A** |
| edit to a small binary crate — `medium` | 10.5 | 11–21 | **A** |
| edit to a large binary crate — `large` (`rg`) | 61–69 | 71–110 | **B** |

Proceed on the basis of **A**, with the L1 envelope defined over the crate
containing the hot function and reported per crate rather than asserted for the
project. The `large` row is not an outlier to be explained away: a hot function
in a big crate costs 62 ms of frontend before anything is generated, and the
product should measure and say so rather than discover it.

## 15. Proposed next step, revised

Not R1, and not the blast radius — that is now answered. **The interface
classifier.** It is the component the 22–52× depends on, it is the one piece
whose correctness is a safety property rather than a latency one, and S0 showed
that computing the facts it needs is free. Build it as an extension of this
same spike: compute an exported-interface fingerprint before and after each of
the five edit classes, and check that classes 1–3 compare equal and classes 4–5
compare different. That is a day of work against a harness that already exists,
and it converts the product's headline number from contingent to earned.


---

# S0c — the classifier

S0b left the 22–52× contingent on a component that did not exist: something
that can *prove* an edit is invisible downstream. This is that component,
its adversarial suite, and the differential evidence that it is right.

## 16. What DIRECT means, and why it is defensible

The rule is not "the public interface is unchanged". Enumerating a Rust
crate's public API from scratch means guessing what rustc considers
downstream-visible, and every item guessed wrong is a silently wrong program.

Instead the classifier asks rustc the question rustc already asks itself.
`rustc_metadata::rmeta::encoder::should_encode_mir` decides whether a body is
serialized into crate metadata, and therefore whether any downstream crate can
inline it, monomorphize it, or const-evaluate it:

```rust
DefKind::AnonConst | AssocConst | Const  => (true, false)    // CTFE MIR
DefKind::Closure if is_coroutine         => (false, true)    // layout
DefKind::AssocFn | Fn | Closure          => {
    opt = always_encode_mir
       || (should_codegen() && reachable(def_id)
           && (generics.requires_monomorphization() || cross_crate_inlinable(def_id)))
    opt = opt && !constness(def_id).is_always_const()
    (is_const_fn(def_id), opt)
}
```

`classify.rs::mir_is_downstream_observable` is that function reimplemented
against the same queries, with the original quoted beside it so that drift is
visible. **DIRECT requires that rustc would not encode this body.** Everything
else — signature, `type_of`, generics, predicates, codegen attributes, `FnAbi`,
symbol name — is a contract comparison across two revisions, because the rmeta
rule answers "can another crate see this body?" and not "does replacing it
keep the program's meaning?".

## 17. The adversarial suite — 14 cases, all passing

A classifier that says DIRECT to everything passes S0's five edit classes and
is worthless. A case passes here only when the verdict **and the reason**
match: a FALLBACK for the wrong reason means the predicate that should have
refused it is untested.

| case | verdict | classify |
|---|---|---:|
| ordinary_arith | DIRECT | 0.08 ms |
| ordinary_existing_call | DIRECT | 0.09 ms |
| ordinary_new_local_fn | DIRECT | 0.12 ms |
| ordinary_reads_existing_static | DIRECT | 0.09 ms |
| new_generic_instantiation | DIRECT (closure 6) | 0.06 ms |
| inline_hint | FALLBACK(cross_crate_inlinable) | 0.06 ms |
| generic | FALLBACK(requires_monomorphization) | 5.8 ms |
| const_fn | FALLBACK(const_evaluable) | 0.13 ms |
| async_fn | FALLBACK(opaque_in_signature) | 0.14 ms |
| rpit | FALLBACK(opaque_in_signature) | 0.07 ms |
| trait_method | FALLBACK(associated_fn) | 0.13 ms |
| sneaky_signature | FALLBACK(contract_changed) | 0.08 ms |
| target_feature | FALLBACK(contract_changed) | 0.11 ms |
| new_static | FALLBACK(new_static_required) | 0.04 ms |

**The classifier costs 0.04–0.14 ms**, against 15–20 ms of frontend. It is
free, as S0 predicted from the 0.002 ms ABI/layout measurement.

`new_generic_instantiation` is DIRECT with a closure of 6 rather than 4: the
artifact must carry `convert::<u32>` as well as the root. That case is the one
that shows **DIRECT means "replace this closure", not "replace this
function"** — V2 §9.4's patch cluster, arrived at from the measurement rather
than assumed.

## 18. Two soundness holes the suite found

Both were in the classifier, and both would have shipped wrong programs.

**A body that introduces a new `static` was called DIRECT.** Path D walks
`TerminatorKind::Call`, and a static does not reach a body through the call
graph — it arrives as a constant holding a pointer into its allocation. The
fix mirrors the collector's `collect_alloc`, through a MIR visitor over every
constant. `body.required_consts()` is *not* enough: it holds only constants
whose evaluation the body's validity depends on, and a plain static read is
not one, so scanning it found no statics at all.

The first version of that fix then refused **reading an existing static**,
because it asked "did this function start reading a new static" instead of
"does the body need storage the base image does not have". The comparison is
against the crate's defined statics, not the function's previous reads.

**Unevaluated constants are not a hazard, and treating them as one broke every
integer cast.** Debug MIR wraps `as` casts in `u32::MIN`/`u32::MAX` checks,
which are associated consts in `core`. A const is a value the backend
materialises; unlike a static it has no address in the base image that must
already exist. Statics are the hazard; only statics are collected.

## 19. The oracle — and what it took to make it mean anything

The classifier's claim is checkable offline: apply a DIRECT edit to a library,
rebuild every dependent anyway, and compare each dependent's `__TEXT,__text`.

| fixture | control `#[inline]` twin | DIRECT `#[inline(never)]` root |
|---|---|---|
| blinker-lib (50 dependents) | **2 changed** | **0 changed** |
| rg-lib (32 dependents) | **1 changed** | **0 changed** |

The control and the DIRECT case are the *same function* differing only in the
inline attribute, both called from the same injected downstream caller. So the
table shows two things at once: the oracle can detect a downstream change, and
the classifier's `cross_crate_inlinable` refusal is load-bearing rather than
decorative.

Getting there took three corrections, each of which had produced a confident
wrong answer:

1. **`otool -s` prints the object's path as its first line**, and the objects
   are extracted to a fresh temporary directory each call — so every rlib
   differed from itself on every run. The oracle reported 50/50 dependents
   changed. The tell was `libahash` in the list, a crate that does not depend
   on the edited one. Now only the hex dump is hashed, and the oracle hashes
   the same build twice and refuses to continue if the two disagree.
2. **Incremental compilation repartitions codegen units** when a dependency
   changes, moving functions between objects without changing what the program
   means. With `CARGO_INCREMENTAL=0` the difference vanished entirely.
3. **No dependent called the injected function**, so "all dependents
   identical" was trivially true and proved nothing. This was caught by the
   control assertion rather than by inspection: a signature change *also* left
   every dependent identical, which is impossible for a real control. The
   fixture now injects a caller into a downstream crate.

Only the third of those was found by reading. The first two were found by
guards that make the harness prove it can measure before it is allowed to
report — which is the same discipline that caught the `Compilation::Stop`
error in §9.

## 20. What the oracle does not prove

It shows that on this corpus, a DIRECT verdict changed no dependent's code. It
cannot show that no such edit exists. The adversarial suite probes the cases
that were thought of; the oracle checks the ones that were not; neither is a
proof. Two fixtures and 14 cases is a starting safety case, not a finished one.

Also still true, and unchanged by S0c: **cg_clif codegen and publication remain
unmeasured**, and the DIRECT class deliberately excludes trait and inherent
methods, generics, `const fn`, `async fn`, and RPIT.

## 21. Next

R1, as the plan says. The budget for a library-crate edit now reads:

| component | measured |
|---|---:|
| rustc validation | 15–20 ms |
| Path D closure | 0.25 ms |
| classifier | 0.04–0.14 ms |
| cg_clif codegen | **unmeasured** |
| publication | **unmeasured** |
| known subtotal | **~15–20 ms** |

against a 434–762 ms cargo debug baseline. Codegen and publication have
roughly 20–50 ms of room before the product stops being compelling.


---

# E2E — a source edit becoming live code

Every component was measured separately; this runs them in one process.

```
source edit
  → rustc validation            a real compiler session
  → DIRECT classifier            S0c
  → Path D closure               S0, now returning symbol names
  → cg_clif machine code         -Zcodegen-backend=cranelift
  → MAP_JIT arena                R1's arena and generation table, by #[path]
  → the next call returns the new value
```

## 22. The proof, before the numbers

The fixture's hot root is
`spike_hot_root(reading) = reading.total().wrapping_mul(7).wrapping_add(1)`,
and `body_arith` changes it to `.wrapping_mul(11).wrapping_add(2)`. Called with
`value = 3, scale = 5`, `total()` is 15, so:

| revision | expected | **returned by the published code** |
|---|---:|---:|
| pristine | 15 × 7 + 1 = 106 | **106** |
| body_arith | 15 × 11 + 2 = 167 | **167** |

Both fixtures, every iteration. The harness asserts the exact value rather
than reporting it, so a publication that ran the *old* body would fail rather
than print a plausible number.

## 23. End to end, p50 over 8 iterations after 3 warm-ups

All milliseconds.

| fixture | edit | validate | classify | closure | codegen | extract | **publish** | **total** |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| blinker-lib | pristine | 26.7 | 0.04 | 0.12 | 14.1 | 0.68 | **0.0020** | |
| blinker-lib | body_arith | 32.1 | 0.20 | 0.05 | 59.2 | 0.64 | **0.0022** | **75.1** |
| rg-lib | pristine | 13.9 | 0.03 | 0.06 | 7.2 | 0.18 | **0.0016** | |
| rg-lib | body_arith | 25.5 | 0.16 | 0.04 | 14.5 | 0.20 | **0.0017** | **32.9** |

Against the cargo debug baselines S0b measured on the same crates:

| fixture | cargo debug | **live** | **speedup** |
|---|---:|---:|---:|
| blinker-lib | 434 ms | **75 ms** | **5.8×** |
| rg-lib | 762 ms | **33 ms** | **23×** |

Publication is 1.6–2.2 µs, consistent with R1's isolated measurement.

## 24. Codegen is the whole crate, and that is the loose part

`codegen` above is cg_clif compiling the **entire crate** and writing an
object, because `rustc_codegen_cranelift` is a backend that exposes "compile
this crate" and not "lower this `Instance`". The product would lower the
4-instance closure Path D found, which R1 measured at **0.12 ms for six
functions**.

Removing whole-crate codegen from the totals gives ~16 ms and ~18 ms, which
would be **27×** and **42×**. That is not claimed as a result — it is what the
remaining engineering is *for*, and it is why §24 exists rather than a single
optimistic number.

The object file is not part of the eventual design.

## 25. Two harness errors, both of which produced a working-looking demo

**The end-to-end total exceeded the sum of its own stages by 44 ms.** The
harness spawned `rustc --print=sysroot` on every session, outside the session
timer. A total larger than its parts is the harness measuring itself; the
sysroot is now asked for once.

**The published function was called with the wrong ABI.** `spike_hot_root`
takes `SpikeReading { value: u64, scale: u32 }` — a 16-byte aggregate, two
registers under AAPCS — and the first version called it as `fn(i64) -> i64`.
It returned 105866, then 72866, then something else: `scale` was whatever `x1`
happened to hold. It looked like a working demonstration and was reading
uninitialised register state. Only fixing the signature and asserting the
exact expected value turned it into evidence.

Neither was found by reading the code. The first was found because the
arithmetic did not add up, the second because the "result" changed between
runs of an identical input.

## 26. What this does and does not establish

**Does:** the whole path works, on two real library crates, with the value
verified against what the source says. Publication is microseconds. The
classifier runs inside the same compiler session the developer was already
paying for.

**Does not:** the DIRECT class is still one shape — free, non-generic,
non-const, non-async, `#[inline(never)]`. Codegen is whole-crate. Two
fixtures. And the runtime differential that V2 §12.2 asks for — running the
live program against a clean rebuild across a mutation suite — does not exist
yet; §22 checks one value on one edit, which is a start and not that.

## 27. Closure-only codegen — §24 closed

§24 named cg_clif's whole-crate codegen as the last loose number and said the
product would lower only Path D's closure. It now does, and the fix was not to
fork the backend.

A rustc backend compiles whatever `collect_and_partition_mono_items` hands it.
That is a **query**, and `Config::override_queries` replaces it. So the
compiler runs its ordinary frontend — the validation the developer was paying
for anyway — and then codegens exactly the instances Path D found, in one
codegen unit, and nothing else. Forty lines, in `patch_universe`.

It works: the object for `blinker-lib` goes from **922 defined symbols to 5**,
and from 15.7 KB to 3.1 KB for `rg-lib`.

### The result, `body_arith`, p50 over 12 iterations after 4 warm-ups

All milliseconds. `expand`/`analysis`/`hot mir` are the old `validate` column,
split — see §28 for why.

| fixture | mode | expand | analysis | hot mir | classify | closure | **codegen** | extract | **publish** | **total** |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| small | whole-crate | 8.28 | 4.17 | 0.06 | 0.137 | 0.033 | 7.31 | 0.90 | **0.0015** | 20.89 |
| small | **closure-only** | 7.60 | 3.96 | 0.05 | 0.122 | 0.029 | **3.54** | 0.59 | **0.0015** | **16.40** |
| rg-lib | whole-crate | 9.79 | 6.53 | 0.07 | 0.098 | 0.029 | 9.22 | 0.15 | **0.0016** | 26.37 |
| rg-lib | **closure-only** | 9.29 | 6.27 | 0.07 | 0.097 | 0.028 | **6.99** | 0.13 | **0.0015** | **23.05** |
| blinker-lib | whole-crate | 14.58 | 7.04 | 0.08 | 0.119 | 0.035 | 42.03 | 0.50 | **0.0015** | 64.59 |
| blinker-lib | **closure-only** | 14.76 | 6.69 | 0.08 | 0.112 | 0.030 | **7.13** | 0.10 | **0.0015** | **29.00** |

| fixture | cargo debug | whole-crate | **closure-only** |
|---|---:|---:|---:|
| blinker-lib | 434 ms | 64.6 ms (6.7×) | **29.0 ms (15.0×)** |
| rg-lib | 762 ms | 26.4 ms (28.9×) | **23.1 ms (33.1×)** |

72 revisions across three fixtures and both modes, every one returning the
exact value §22 requires: 106 pristine, 167 edited.

### The control, which is where the actual answer is

`codegen` in that table is not lowering. It is *everything after analysis* —
lowering, Cranelift, writing the object, finalizing the incremental session,
dropping the arena. Watching it fall from 42 ms to 7 ms says the universe got
smaller; it does not say how much of the remaining 7 ms is the four functions
and how much is fixed cost that no narrowing will ever remove.

So there is a third mode that codegens **nothing at all** and still emits an
object. The difference is the closure's real backend cost:

| fixture | whole-crate | closure-only | empty universe | **the closure itself** |
|---|---:|---:|---:|---:|
| small | 7.31 | 3.54 | 2.86 | **0.68 ms** |
| rg-lib | 9.22 | 6.99 | 6.35 | **0.64 ms** |
| blinker-lib | 42.03 | 7.13 | 6.79 | **0.33 ms** |

**Lowering and compiling a real four-instance Rust patch closure through
cg_clif costs 0.33–0.68 ms.** That is real MIR lowering — trait resolution,
layout, ABI adaptation — not R1's hand-built CLIF, and it lands in the same
order of magnitude as R1's 0.12 ms. It is inside the band this was called
exceptional at, by a factor of three.

The corollary is the useful part: **the backend is now the smallest measured
component of the pipeline except publication.** Optimizing it further would
buy nothing.

### Where the time actually goes now

For `blinker-lib`, of 29.0 ms:

```
expand      14.8   parse and macro-expand the crate
analysis     6.7   resolve, type check, trait select, borrow check
session      6.8   write the object, finalize incremental, drop the arena
closure      0.33  lower and compile the patch closure          <- the backend
extract      0.10  lift four functions out of the object
classify     0.11  the DIRECT proof obligation
Path D       0.03  find the closure
publish      0.0015 reserve, copy, relocate, i-cache, swap
```

Two things dominate and neither is compilation of the edit. `expand` is
whole-crate parsing that a resident compiler would not repeat; the 6.8 ms
`session` cost is a batch compiler finishing a batch — writing an object file
that the eventual design does not want and serializing a dependency graph.
R1 already showed the artifact going straight from `Context::compile` to the
arena in 0.13 ms with no object file involved.

Neither is claimed as recoverable here. They are named because they are what
is left, and because they are both *frontend and harness* costs — which is
exactly what S0b predicted the answer would be.

## 28. Two things the control caught

**An apparent regression that was not one.** The first closure-only run showed
`validate` rising from 23.7 ms to 34.2 ms on `blinker-lib` and 17.3 to 23.3 on
`rg-lib` — consistently, on both fixtures, in the wrong direction for a change
that only removes work. It was noise: eight iterations, interleaved with other
heavy runs. Splitting the column into expand/analysis/hot-mir and running
twelve iterations showed the three parts matching across modes to within
0.3 ms. Recorded because the temptation was to explain it, and a plausible
mechanism for a measurement artefact is worse than no explanation.

**The object was chosen by timestamp.** With one codegen unit that is
harmless. A *binary* crate gets a second unit for the allocator shim, and
whole-crate codegen of a large crate gets several — so "the newest `.o`"
picked the allocator shim for the `small` fixture and reported the hot root
missing from a compilation that had just produced it. The object is now chosen
by **what it defines**: the one containing the root symbol. This is findings
230 and 241 again, in a third setting — a name and a position are both
incidental, and identity is what the thing *is*.

That fix is what made `small` work end to end, which is why §27 has three
fixtures and one of them is a `bin`.

## 29. Two invariants, so that the narrow universe is a check and not a hope

Closure-only codegen made the patch object's contents *predictable*. A
prediction nobody compares against the outcome is a comment, so both are now
asserted on every patch.

**Set equality on definitions.** The object's external definitions must equal
the set the codegen universe was told to emit, plus an explicit
`RUNTIME_SUPPORT` allowlist that is currently empty and exists so the first
entry has to be argued for. Containment would not do: "every closure member is
present" passes on the 922-symbol whole-crate object exactly as happily as on
the 5-symbol one. Both directions have teeth — an unexpected definition means
the backend emitted something Path D did not account for; a missing one means
the patch is incomplete. Measured on all three latency fixtures: **4 defined,
4 expected, 0 unexpected, 0 missing.**

**Every relocation explained.** A target must be inside the closure, a symbol
the base image defines, or an allowlisted runtime helper. Anything else
rejects the patch.

Writing that down found a defect in code that had been passing its tests.
Section-relative relocations — a constant pool, a jump table, a string literal
— were `return None` inside a `filter_map`, which does not refuse them. It
**drops** them: the patch was published with that field unpatched and the code
read whatever cg_clif had left there. Refusing to lift a function is a rejected
patch. Silently skipping one of its relocations is a wrong answer at full
speed. A relocation against a symbol that could not be named was being dropped
the same way. Both now reject.

The one capability added is a GOT. Reading a static compiles to an
`adrp`/`ldr` pair through a Global Offset Table, so the arena now carries an
eight-byte slot per externally-referenced symbol and relocates `Page21`,
`PageOff12`, `GotLoadPage21` and `GotLoadPageOff12` into it. That is case two
of the invariant — an existing base-image symbol — rather than a widening of
the semantic class.

## 30. The runtime differential

§22 checked one value on one edit. That is a start and it is not a safety case.

```
                   same starting revision
                            │
               ┌────────────┴────────────┐
           LIVE PATH                 CLEAN PATH
               │                         │
    validate / classify           apply the same edit
      Path D closure                     │
       cg_clif patch            rustc, LLVM, real linker
         publish                         │
               │                         │
          run probes                run probes
               └────────────┬────────────┘
                            ↓
                    compare observations
```

The two sides share **no code below the source text**. The clean path is
ordinary rustc with the default LLVM backend, producing a `cdylib` that the
dynamic loader loads. Nothing in the live path's lifting, relocation or
publication logic runs on the clean side, so a bug there cannot cancel out.

### It has a base image, and that is the whole design

Every error the classifier exists to prevent is an error about code that was
*not* patched — a caller holding an inlined copy of an old body, a constant
folded into a neighbour, a layout some other function still assumes. A
differential that only ever calls the patched function cannot see any of them,
and would be green for exactly the reasons it should be red.

So the live path loads the previous revision as a real base image and drives
its probes through `diff_entry`, which lives in that image and reaches the
patch through a gate. Everything the entry point touches other than the patch
closure is old compiled code. That is the situation a live patch actually
creates, and it is what makes §32 findable.

Nine probe inputs, chosen so that `branch_cold`'s cold path is taken by some
and not others and `loop_edit`'s trip count varies across its clamp. Return
values and the memory the callee writes are both compared.

### The suite

| mutation | exercises | verdict | agrees with a clean rebuild |
|---|---|---|---|
| `body_arith` | arithmetic in the root | DIRECT | **yes** |
| `call_existing` | a second call to a member the closure had | DIRECT | **yes** |
| `new_local_helper` | a function that did not exist before | DIRECT | **yes** |
| `new_generic` | a generic instantiated at a new type | DIRECT | **yes** |
| `read_static` | reading a static the base image holds | DIRECT | **yes** |
| `multi_function` | two members of one closure changed | DIRECT | **yes** |
| `branch_cold` | a branch most probes never take | DIRECT | **yes** |
| `loop_edit` | a loop whose trip count is an input | DIRECT | **yes** |
| `edit_outside_closure` | a function outside the closure changed | FALLBACK(changed_outside_closure) | refused |
| `new_static` | a static introduced | FALLBACK(new_static_required) | refused |

Every variant is generated from `pristine.rs` by a stated substitution rather
than written by hand, because "the same starting revision, edited two ways" is
the premise, and a hand-written variant that had drifted in some second,
unnoticed respect would make a passing comparison mean less than it looks like.

### What is not observed, and why

`stdout`, `stderr`, panics and callbacks are fields on `Observation` and are
empty for every mutation. The reason is one reason: all four need the patch to
reference constant data — a format string, a `&Location`, a vtable — which
arrives as a section-relative relocation, which §29 refuses outright. A patch
that can panic is outside this DIRECT class today. The fixture is compiled with
`-Cdebug-assertions=off` for the same reason and not for convenience: at
`-Copt-level=0` an ordinary `+` emits an overflow check that calls
`core::panicking::panic_const_add_overflow(&Location)`.

## 31. Three negative controls

A suite that has never failed is a suite nobody has shown *can* fail.

| control | injected defect | required outcome | result |
|---|---|---|---|
| 1 | omit one member of the patch closure | caught while building the artifact, never as a wrong answer | **8/8 rejected**, base image intact |
| 2 | point a relocation at a symbol that does not exist | candidate rejected, current generation unchanged | **8/8 rejected**, base image intact |
| 3 | publish an edit the classifier refused | the suite must catch what the classifier would have | **2/2 caught** |

Control 3 is the one that decides whether a green run means anything, and it
demonstrates *both* detection mechanisms:

- `new_static` forced through is caught by §29's relocation rule — the patch
  references storage the base image does not have.
- `edit_outside_closure` forced through **publishes cleanly and is caught by
  the behaviour comparison**: `probe 0 (0, 0): live returned 7 wrote 43693,
  clean returned 8 wrote 43693`.

The transactional property is checked on every trial and not only the failing
ones, because an *accepted* patch that corrupted the previous generation would
otherwise go unnoticed. Across all four modes and every mutation, the base
image answers exactly as it did before the attempt.

Control 1 was wrong the first time and said so. It dropped Path D's *last*
symbol, which for two mutations nothing referred to — so the patch published,
agreed with the clean rebuild, and the control reported a failure to fail. An
omission that changes no observable behaviour is not an incomplete closure; it
is a closure that was larger than it needed to be. The victim is now a member
that another member calls, and if no such member exists the control reports
that it does not apply rather than passing.

## 32. What the differential found: the classifier was reasoning about the wrong thing

`edit_outside_closure` changes `diff_entry` — a function that is not the hot
root and is not in its closure. Every field S0c compared was bit-identical
across the two revisions, because every field S0c compared is *about the hot
root*: signature, ABI, symbol name, attributes, generics, predicates, MIR
observability. All unchanged, correctly, because the root did not change.

The patch published. The base image kept its old `diff_entry`. The program
returned **7 where a clean rebuild returned 8**.

Nothing in the compile-time oracle could have found this either — no
dependent's code changes, because the edit is inside one crate. It needed a
behavioural comparison against a clean rebuild, which is precisely the argument
for having one.

The missing premise is not about the root at all:

> **A live patch replaces the closure, so nothing outside the closure may have
> changed.**

The contract now carries a fingerprint of every function the crate defines, and
`classify` refuses when any body outside the closure changed — or was removed.
The fingerprints are rustc's own: it already hashes each HIR owner including
bodies for its incremental machinery, so this reads numbers that have been
computed rather than computing them. Added-then-deleted functions are covered
in both directions. It fails closed: an unavailable fingerprint refuses rather
than comparing equal to another unavailable fingerprint.

`edit_outside_closure` is now `FALLBACK(changed_outside_closure)`, naming
`::diff_entry`. All eight DIRECT mutations still agree, all 14 S0c adversarial
cases still pass, and the downstream oracle still passes.

**A DefId is not an identity, either.** Fixing that hole surfaced a second one
underneath it. The contract rendered types with `{:?}`, which prints
`DefId(0:7 ~ diff_fixture[b607]::diff_root)`. The `0:7` is a *definition
index*, assigned by the order items appear in the crate — so adding any
function above `diff_root` renumbered it, and the contract reported that
`diff_root`'s type had changed when nothing about it had. `new_local_helper`
and `new_generic` were both refused with `field: "type_of"`, quoting two
spellings of the same function.

That is the linker's finding 230 and finding 241 for a third time. A local's
name was not an identity; an archive member's name was not an identity; a
definition's index is not one either. The contract now compares def *paths*.
A consequence worth stating: this bug was making the classifier *look* safer
than it was, by refusing edits for a reason that had nothing to do with safety.

**Two smaller ones, both found the same way.** Path D walked into intrinsics
and asked for their MIR, which is an ICE rather than an error — a mutation
calling `rotate_left` took the compiler down. But skipping all intrinsics is
wrong in the more dangerous direction: one that is not `must_be_overridden` has
a fallback body, and cg_clif emits an ordinary call to it, so the object was
left calling a function nobody had generated. rustc's own collector resolves
such an intrinsic to its fallback `Item`, and now so does this.

And `def_path_str` cost **3.7 ms** — flat across a 40-function crate and a
900-function one, which is the shape of one crate-wide query and not of
per-item work. It renders the *visible* path, which needs `visible_parent_map`
over the whole crate graph. `def_path` is local data and costs 0.15 ms. When
the two renderings disagreed, every changed body looked like it lay outside the
closure, every edit was refused, and — because the summary table had no verdict
column — that read as a silent `None` rather than as a refusal. The column is
back.

## 33. Where the numbers stand

`body_arith`, closure-only, p50 over 12 iterations after 4 warm-ups, with §32's
check in place:

| fixture | expand | analysis | hot mir | classify | closure | codegen | extract | **publish** | **total** | cargo debug |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| small (bin) | 8.61 | 4.08 | 0.05 | 0.228 | 0.033 | 3.76 | 0.795 | **0.0020** | **17.5** | — |
| rg-lib | 9.75 | 6.65 | 0.07 | 0.165 | 0.031 | 7.11 | 0.103 | **0.0018** | **25.0** | 762 ms → **30.5×** |
| blinker-lib | 15.51 | 7.20 | 0.09 | 0.145 | 0.032 | 7.65 | 0.111 | **0.0019** | **31.2** | 434 ms → **13.9×** |

72 revisions, no errors, every one DIRECT and returning 106 / 167.

**A correction to §27.** That section put the closure's own backend cost at
0.33–0.68 ms. Repeating the empty-universe subtraction on a quiet machine gives
0.53, 0.25 and **−0.23** ms. A negative figure is not a faster-than-nothing
compile; it is the subtraction reaching its noise floor against a ~7 ms fixed
session cost. The defensible statement across both runs and all three fixtures
is:

> Lowering and compiling a real four-instance Rust patch closure through
> cg_clif costs **under 1 ms**, and on the largest fixture it is not
> distinguishable from zero by this method.

The conclusion §27 drew is unchanged and slightly strengthened: the backend is
not the thing to optimize. Resolving it further would need a timer inside
cg_clif rather than a subtraction outside it.

What remains, for `blinker-lib`'s 31 ms: `expand` 15.5, `analysis` 7.2,
fixed session finalization 7.9. Everything unique to Blinker Live — classify,
Path D, the backend, extraction, publication — is **0.5 ms of the 31**.

## 34. Objectless codegen — the backend's own timers

§33 established the closure's backend cost by subtracting an empty-universe run
from a closure-only one, and on the largest fixture that produced −0.23 ms. A
subtraction at its noise floor is not a measurement. The timers are now inside
cg_clif, around the work they name.

### The seam

§27 narrowed the codegen *universe* without touching the backend, because the
universe is a query and queries can be overridden from outside. The codegen
*output* is not a query. `CodegenBackend` exposes `codegen_crate` and
`join_codegen`, and the object file is written between them, inside cg_clif's
AOT driver. There is no seam there reachable from a driver, so this one is a
backend patch.

It is narrow on purpose: **356 added lines, 300 of them one new file and its
documentation.** The rest is four hooks in `driver/aot.rs`, `pub(crate)` on two
existing fields, and one `mod` declaration. Nothing in codegen changes; with no
callback installed the backend is an unmodified drop-in. The pristine source
stays in the toolchain (`rustc-src` ships it) and only the diff is in this
repository — `experiments/live-sink/`, with a `build.sh` that reproduces the
dylib in about 40 seconds.

```
MonoInstance → MIR→CLIF → Context::compile → code bytes
                                             relocations   → callback → arena
                                             alignment
                                                  ↓
                                          object file (still written,
                                          after the callback, timed)
```

### The numbers, with no subtraction

Four-instance patch closure, ~130 bytes of code, measured inside the backend:

| | small | rg-lib | blinker-lib |
|---|---:|---:|---:|
| MIR → CLIF | 0.036 | 0.103 | 0.044 |
| Cranelift `Context::compile` | 0.091 | 0.161 | 0.148 |
| extract code + relocations | 0.001 | 0.001 | 0.001 |
| **backend total** | **0.13** | **0.27** | **0.19** |

That is the number §27 and §33 were circling. It is under a third of a
millisecond, it was taken around the work rather than derived from a
difference, and it settles the question: **the backend is not what to optimize.**

### And the object file costs 0.2 ms, not 7

The object path still runs, after the callback, timed separately — because
deleting it before measuring it would repeat S0's mistake of removing a step to
save time and silently invalidating the measurement.

| | small | rg-lib | blinker-lib |
|---|---:|---:|---:|
| object define | 0.051 | 0.067 | 0.143 |
| object write | 0.095 | 0.133 | 0.132 |
| **total** | **0.15** | **0.20** | **0.28** |

I had assumed the ~7 ms of post-analysis residual §33 reported was largely
object emission. It is not: **removing the object file entirely saves about
0.2 ms.** The 7 ms is rustc finishing a session — dependency graph, incremental
bookkeeping, teardown — and none of it is the object.

That is a correction to the reading in §33, and it changes where the remaining
work is.

## 35. Successive revisions, no cold reset

Every earlier end-to-end run applied one edit to a pristine crate and then
started over. That cannot see anything which only goes wrong the *second* time.

`--sequence` applies revisions back to back in one process, each classified
against the revision before it rather than against pristine, with the exact
value asserted at every step. With `value = 3, scale = 5` the hot root's
`total()` is 15, so the four revisions must return 106, 167, 54 and 349.

| fixture | | G0 → G1 | G1 → G2 | G2 → G3 |
|---|---|---:|---:|---:|
| small | **edit → active** | 17.2 | 16.4 | 14.2 |
| | edit → committed | 24.3 | 19.7 | 17.2 |
| rg-lib | **edit → active** | 20.4 | 21.9 | 22.7 |
| | edit → committed | 24.2 | 26.0 | 26.7 |
| blinker-lib | **edit → active** | 27.3 | 26.3 | 26.5 |
| | edit → committed | 31.6 | 31.6 | 31.1 |

**Every value exact, on all three fixtures, four generations retained.** The
cost is flat across revisions: incremental state survived publication moving
ahead of finalization, def-path-hash identities survived successive source
shapes, body fingerprints advanced, Path D held no stale instance, and the
generation table did not depend on being the first one.

The two columns answer different questions. `edit → active` is what a developer
waits for. `edit → committed` is when the compiler is ready for the next
revision, and the gap — **3–7 ms** — is what moving publication ahead of
finalization bought.

### The harness was measuring its own extra session

The first version of this ran a separate contract-only session between
revisions, and it showed G1 and G2 at 54 and 50 ms against G0's 20. That was
not the second edit being slower. The interposed session had no `--emit=obj`,
so it left the codegen cache in a different state, and the next revision paid
for it — and on the `small` fixture it also tried to *link* a binary whose
`main` the closure-only universe had deliberately not codegened.

The product would never run that session: a revision's contract comes from its
own compilation. Taking it from there removed the second session, the artefact
and the link error together.

## 36. What the sink found, and one it could not have found any other way

The runtime differential was pointed at the new artifact path, and all four
modes pass: 7 of 8 DIRECT mutations agree with a clean rebuild, all three
negative controls behave as required.

**Slot 0 was the wrong function.** cg_clif delivers a codegen unit's items in
deterministic order, which is by symbol name — not Path D's order, which puts
the root first. For two fixtures the root's mangled name sorted first and
everything worked. `diff_fixture`'s root is `#[no_mangle]`, so `_diff_root`
sorted *after* `_RNv…blend`, and the published entry point was `blend`. It
returned 7 for probe `(0, 0)`: a plausible number, from the wrong function. The
consumer now finds the root by name instead of trusting a position.

**The suite had been testing the wrong backend.** rustc loads a codegen backend
once per process and caches it, so the first session decides for every session
after it. The differential's warm session ran first without
`-Zcodegen-backend`, which meant every later session used LLVM no matter what
it asked for. The sink was never reached — and before the sink existed, the
suite was validating LLVM's objects while believing they were cg_clif's.

**A cached codegen unit is not a compilation.** When a crate's code has not
changed rustc reuses its cached work product and codegens nothing. The sink
notices, because it only ever sees code that was actually generated. The object
path did not: it read a file off disk that a previous compilation had left
there and published it. Here that file happened to hold the right code — right
by luck, not by construction.

**The artifact class is backend-sensitive.** `new_local_helper` calls
`rotate_left`. Under LLVM that needs no constant data; under cg_clif it
materialises through an anonymous data object, which §29's relocation rule
refuses. So it is *refused* under the sink and *agrees* under the object path.
A refusal is a safety-preserving outcome and a coverage gap, and the suite now
counts the two separately — conflating them in either direction makes it
useless, one by hiding breakage and the other by making an honest refusal look
like some.

The DIRECT semantic envelope is backend-independent. The *artifact* envelope is
not, and that had not been written down before.

## 37. Identity, permanently

The sets §32 compares are keyed by `DefPathHash`, which rustc defines as stable
across crate and compilation-session boundaries. Not a definition index, which
§32 found is not an identity; not a rendered path, which is a string this code
invents rather than the compiler's own answer, and which cost 3.7 ms flat when
rendered the readable way (`def_path_str` walks the whole crate graph for the
*visible* path). A readable path travels beside the hash so that a refusal can
name the function rather than a hash.

The invariant, as it now stands:

```
Δbody = bodies added, removed or changed, by DefPathHash
C     = Path D's patch closure, by DefPathHash

DIRECT iff
  1.  Δbody ⊆ C                          — nothing changed outside the closure
  2.  every member of C satisfies the downstream-interface contract
  3.  no new base-image state is required — statics, TLS, exported data
  4.  every relocation target ∈ C ∪ base-image symbols ∪ runtime allowlist
  5.  emitted definitions = C ∪ allowlist  (equality, both directions)
  otherwise FALLBACK
```

Subset in (1) and equality in (5), and the difference matters: a closure may
legitimately contain unchanged functions, but an object may not contain a
definition nobody predicted.

## 38. The resident coordinator, and a 100-revision soak

One process, a fresh compiler session per revision. The process keeps what is
genuinely process-scoped — the rustc and cg_clif dylibs, the sysroot lookup,
the captured invocation, the incremental directory, the arena, the generation
table, and the previous revision's contract. It does not try to keep a
`TyCtxt`: rustc's `Compiler` is one session by construction.

Sessions are strictly sequential. Overlapping revision N's finalization with
N+1's analysis would have them share incremental state, and S0's `s-…-working`
finding is warning enough about what that costs, for a few milliseconds of
throughput.

### One process, one backend — as an invariant

rustc loads a codegen backend once per process and caches it, so the first
session decides for every session after it. §36 found this by accident. It is
now checked: a session that would switch backends aborts with the reason.

It fired immediately, on the object-path differential, which passed no backend
flag on its warm session and `-Zcodegen-backend=cranelift` afterwards — so it
had been running LLVM throughout. Fixed, and with cranelift actually selected
that suite now refuses `new_local_helper` exactly as the sink path does. The
two paths agree because they are finally compiling with the same compiler.

### 100 revisions per fixture, deterministic cycle

`body_arith` → `body_arith2` → `body_arith3` → `fallback_inline`, repeating.
Three DIRECT revisions with distinct answers and one the classifier must
refuse, so recovery is exercised as often as success. The value the *running
program* answers is read after every revision, including refused ones — where
it must be unchanged.

| fixture | revisions 0–9 | 40–49 | 90–99 | drift | peak RSS |
|---|---:|---:|---:|---:|---|
| small | 14.4 | 17.6 | 15.0 | **1.08×** | 108 → 114 MB |
| rg-lib | 29.7 | 27.7 | 20.8 | **0.77×** | 140 → 143 MB |
| blinker-lib | 29.3 | 24.7 | 24.5 | **0.92×** | 135 → 136 MB |

`edit → active`, p50 per bucket, milliseconds. `blinker-lib` is flat to within
0.5 ms from revision 10 onward.

**300 revisions across three fixtures: every value exact, every verdict as
required, no latency drift, peak RSS growth of 1.2–6.0 MB over the last 75
revisions.** 101 generations retained per run, none reclaimed — R1 deferred
reclamation and at this scale it is not what limits anything.

### The two latencies

| fixture | edit → active | active → ready | edit → ready |
|---|---:|---:|---:|
| small | 14.4 | 3.2 | 17.6 |
| rg-lib | 21.8 | 4.3 | 26.2 |
| blinker-lib | 24.5 | 4.1 | 28.6 |

Steady-state medians. The first is what a developer waits for. The second is
when the compiler is ready for the next revision, and it is the cost of a
coordinator that accepts edits faster than it can finalize them.

### Residency did not remove expansion, as predicted

| blinker-lib, 24.5 ms | |
|---|---:|
| expand | 14.2 |
| analysis | 6.8 |
| everything Blinker Live | **0.5** |
| — of which the backend | 0.18 |

Process residency removed startup and dylib loading. It did not remove
parsing and expansion, and it was never going to: each revision is a new
compiler session, and `TyCtxt`, HIR arenas and query state are session-scoped.
An earlier note in this file suggested a resident compiler "would not repeat"
whole-crate parsing. That was wrong, and the soak is what would have caught it
had it been acted on.

**Expansion is now the largest single component of the product's latency, and
it is a compiler-persistence problem rather than a Blinker Live one.**

## 39. Three more the soak found

**A published revision that nothing had classified.** The sink publishes from
inside codegen, before any verdict exists — it has to, because that is where
the artifact is. Every path that does not accept the patch must therefore undo
it, and the first version only undid the FALLBACK one. On the revision after
every refusal, the contract comparison had no baseline, the code returned
early, and the program went on running a patch that nothing had approved while
the harness recorded that nothing was published. There is now one acceptance
point and one rollback for everything else.

**A refusal that could be optimized away.** The soak's refusable revision
introduced a `static` and read it, on the theory that the base image has
nowhere to put the storage. At `-Copt-level=0` that is refused, and the
differential fixture confirms it. At `-Copt-level=3` — which the captured cargo
invocation for these fixtures uses — rustc folds the read of an immutable
static, the patch closure genuinely never references it, and DIRECT is the
*correct* verdict. The classifier was right and the fixture was wrong: it
reasons about the closure that exists, not the source text that produced it.
The refusable revision is now an `#[inline]` one, which cannot be folded
because inlinability is the thing being asked about.

**Recovery from an inlinability refusal takes two revisions.** `fallback_inline`
leaves a body rustc encodes into the crate's rmeta, so a downstream crate may
hold a copy. The revision after it has a clean `#[inline(never)]` body of its
own — but its *predecessor* is still observable downstream, and replacing one
copy does not reach the others. `classify` refuses on `before.mir_observable`
as well as `after.mir_observable`, which is correct and which no earlier test
had exercised, because no earlier test ran two revisions in a row. A real
coordinator answers a FALLBACK with an ordinary rebuild, which re-establishes
the baseline; this harness has none to rebuild, so it models the conservative
path and checks that the program stays on its last good generation throughout.
Across 300 revisions it did.

## 40. Candidate and commit

§39 found the sink publishing a revision that nothing had classified, and the
fix at the time was to roll it back on every non-accepting path. That is the
wrong fix. It is only safe because the soak is sequential: the sequence

```
build artifact → publish → classify → FALLBACK → roll back
```

has a window in which a generation nothing accepted is globally reachable, and
a request arriving inside it runs code that was refused.

The architecture now forbids it. cg_clif's sink may only build a **candidate**:
its code is in the arena, fully relocated, i-cache flushed, slot table built —
and `Runtime::current` has never heard of it.

```
rustc / cg_clif
      ↓
  Candidate            code bytes · relocations · gates
      │                NOT reachable
      ↓
  classifier  →  artifact rules  →  DIRECT?
      ├── no  → discard
      └── yes → Staged::commit()      one atomic store
```

`Staged::commit` is the only thing on the forward path that changes the current
generation, and `Patch` hands the caller a candidate rather than a fait
accompli. A refused candidate is *discarded*, not rolled back: its arena slab
stays allocated until reclamation exists, but nothing ever pointed at it, so
there is no interleaving in which it could have run.

The soak shows the change directly: **52 generations retained across 100
revisions**, one per DIRECT commit. It was 101 before — one per revision,
because every refusal published and then retreated.

## 41. Generation semantics, against independent rebuilds

Three scenarios and a control, with barriers rather than sleeps — a sleep makes
an interleaving *likely*, a barrier makes it the only one that can happen.
Every assertion is against a clean LLVM rebuild loaded by the dynamic loader.

The fixture gained a second `#[no_mangle] extern "C"` function inside the patch
closure, and the revisions compared change *both*. A probe that captures a
generation and calls one function through it proves very little: the pointer
was read once and could not have changed. Two gates, read on either side of a
barrier, is a claim with something to fail. The harness refuses to run if the
two references agree on any field the probe reads.

```
clean G1   returned 585    wrote 43235   second 34
clean G2   returned 7624   wrote 46946   second 325
```

| scenario | what it forces | result |
|---|---|---|
| 1 | a scope enters on G1; another thread commits G2 while it is blocked; it then re-reads **both** gates from the generation it holds | reads G1, both gates, both times |
| | a scope entered after the commit | reads G2 |
| 2 | a refused revision is built while a scope is open, and discarded | **no observation of it from either thread** |
| 3 | a scope holds G2; the runtime rolls back to G1; a new scope starts | holder still reads G2, new scope reads G1 |
| control | the same interleaving, with the probe deliberately re-entering the runtime after the barrier | reads the newly committed generation |

**10 observations, all exact.**

The control is the part that makes the rest mean anything. Scenario 1 asserts
that a captured generation still reads G1 after a concurrent commit — which a
runtime where the commit never happened would also satisfy. So the same
interleaving runs again with the probe reading `current` instead of what it
holds, and that one must see the new generation. It does.

Building the control found its own bug: the control commits, and it was
originally placed before scenarios 2 and 3, which then measured a state they
had not established. It runs last now, after the rollback, which also makes it
a second check that rollback left the runtime in a state a later commit can
still move.

Rollback is still called *code* rollback. It restores implementations. Whatever
the retired generation wrote to globals, files or sockets is still written.

## 42. `needs_rebase`, named

A refusal makes two notions of "the previous revision" diverge: the **compiler**
predecessor, which is whatever was last compiled, and the **committed**
predecessor, which is the generation the program is actually running.

This compares against the compiler predecessor, and that is why recovery from
an inlinability refusal takes two revisions rather than one. `fallback_inline`
leaves a body rustc encodes into the crate's rmeta, so a downstream crate may
hold a copy; `classify` refuses on `before.mir_observable` as well as
`after.mir_observable`; and the revision after it — clean `#[inline(never)]`
body of its own — is refused because its *predecessor* is still observable
downstream and replacing one copy does not reach the others.

Comparing against the *committed* predecessor instead would permit
DIRECT → FALLBACK → DIRECT, on the argument that no downstream crate ever
received the refused revision. That is probably sound. It is not what this
does, because it makes source history and runtime history diverge and each
divergence needs its own oracle.

The rule, stated rather than emergent:

> **A FALLBACK puts the Live session into `needs_rebase`. Revisions stay
> non-DIRECT until a rebase re-establishes the baseline.**

The soak checks the *policy*, and the classifier independently arrives at the
same verdict from `before.mir_observable`. Two derivations of one conclusion,
rather than one asserted twice. Over 300 revisions they never disagreed.

## 43. Where this stands

| property | status |
|---|---|
| Rust semantic validation | real compiler sessions, every revision |
| exact changed closure | Path D, 4–12 instances, set-equality checked |
| DIRECT safety classifier | 14 adversarial cases, closed-world invariant |
| downstream rebuild skipping | oracle, 50 and 32 dependents, discriminating control |
| MIR → Cranelift patch codegen | **0.13–0.27 ms**, timed inside the backend |
| publication | **~2 µs** |
| runtime vs independent LLVM rebuild | 9 mutations, 3 negative controls |
| 100-revision residency | flat: drift 0.93×, 1.03×, 1.01× |
| memory | +5.7 to +14.4 MB over 75 revisions |
| concurrent generation consistency | 3 scenarios + control, 10 observations |
| rollback consistency | scenario 3, against clean rebuilds |

Steady-state `edit → active`, p50: **small 15 ms, rg-lib 23 ms, blinker-lib
29 ms**, against cargo debug rebuilds of 434 and 762 ms.

The defensible claim:

> Blinker Live activates eligible Rust body edits in roughly 15–30 ms on the
> measured workloads, 14–30× faster than Cargo. The changed Rust code compiles
> in well under a millisecond and publishes in about 2 µs; the remaining
> latency is rustc's own semantic validation, of which expansion is the largest
> part and is a compiler-persistence problem rather than a Blinker Live one.

## 44. The agent API (M1)

M0 is the runtime. This is the surface, and the surface is the product decision.

An agent fixing a Rust bug with conventional tools spends its wall clock on
three things: reading files to find out what the program *is*, running cargo to
find out whether a change compiles, and running tests to find out what the
program *does*. The first is textual archaeology over a language whose structure
the resident compiler already knows exactly. The second and third are the same
434–762 ms rebuild, paid once per hypothesis.

So the API is not a shell. It is seven verbs:

| verb | question |
|---|---|
| `inspect` | what is this function — signature, body, callees, where it lives |
| `callers` | who calls it |
| `replace_body` | make this the new body; is that publishable |
| `probe` | what does this function *actually* return for these arguments |
| `run_affected` | which tests reach the change, and do they pass |
| `commit` | make the candidate the program |
| `rollback` | put the previous generation back |

JSON lines in, one flat observation out:

```
{"op":"probe","symbol":"scan","args":[0,5],"bytes":"12,18"}
{"status":"ok","latency_ms":0.009,"symbol":"::scan","returned":12,"source":"image"}
```

Three decisions inside that are worth naming.

**`replace_body` never publishes.** It compiles, classifies, and stages — §40's
candidate, exposed rather than hidden. `probe` and `run_affected` then call
*into the candidate* by address. An agent can therefore find out what a change
does before any of it becomes the program, which is what makes this an
experimentation surface rather than a fast deploy button, and is the thing M5's
speculative branching needs in order to exist at all.

**`source` is never omitted.** Every probe says whether it was answered by the
`candidate`, the current `generation`, or the base `image`. A probe that hits
the image after a patch is staged reports the *old* behaviour — which is true,
and is what every unpatched caller still sees — but an agent that mistook it for
the new one would conclude its edit had done nothing.

**A test reports two numbers, not a boolean.** `extern "C" fn(*mut u64) -> u64`:
write what you expected, return what you got. `{"expected":30,"actual":12}` is
actionable; `false` sends the agent looking for the difference somewhere this
API cannot answer.

### The semantic graph

`inspect` and `callers` come from `optimized_mir`'s call terminators and HIR
spans, keyed by `DefPathHash` throughout — §37's identity, because this index
has to survive exactly the session boundary that identity was designed for.

It is **not** built on the edit → active path. Building it forces `optimized_mir`
for every function in the crate, which the revision path does not otherwise do,
and in `after_analysis` it would land in front of the sink. So `replace_body`
does not ask for one: the workspace knows the byte delta of the splice it just
made and shifts every span in the same file arithmetically, which is exact.
What it cannot know is whether the edit changed the call graph, so it marks the
edited function's edges stale and the next question that needs them pays for a
refresh and says so.

| fixture | session | graph | functions |
|---|---:|---:|---:|
| agent | 15.0 ms | **0.05 ms** | 12 |
| small | 73.9 | **1.39** | 307 |
| rg-lib | 26.4 | **4.29** | 129 |
| blinker-lib | 28.9 | **4.19** | 63 |

The graph is 0.05–4.3 ms. The compiler session around it is 15–74. Which is the
same shape as every other number here: the part this project wrote is not the
expensive part.

### Affected-test selection, and the root that is not the patch

A test lives in the base image and calls the function it tests with an ordinary
`bl`. Publishing a patch does not change that call — the base image's test would
keep exercising the base image's code, and a suite that passes against the old
body while reporting on the new one is worse than no suite.

So the tests become **additional codegen roots**. The union of their Path D
closures is the codegen universe, and the arena gets a copy of each test whose
call to the patched function is an intra-closure relocation. `EXTRA_ROOTS` is
empty unless an agent session asks, so every measurement above compiles the
universe it always did.

Two things that had to be got right:

- `hot_root_closure` grew a shared `seen` set. Per-call dedup emitted each
  shared member once per root — a test reaches the function it tests — and §29's
  equality check expects a set, not a multiset.
- The contract, and therefore the classifier, still sees **the hot root's own
  closure**. Widening the universe widens what is generated; it must not widen
  what a DIRECT verdict is a claim about.

Selection is over the `DefId` graph, which over-approximates Path D: one generic
function stands for all its instantiations. That is the safe direction — a
superset runs tests that did not need to run, a subset silently skips the one
that would have failed. Generation, by contrast, is *complete*: every test is a
root, not only the ones the pre-edit graph calls affected, because an edit whose
purpose is to change what a function calls is exactly the case where the
pre-edit graph is wrong. It costs codegen proportional to the suite's reachable
set, and that is the honest limit to carry into M2.

### The gate

`agent_gate.py` is a transcript, not a unit test: every step is a question
somebody debugging would actually ask, and each is checked for the answer that
lets them take the next step.

```
  the program, as it is
  ok    the suite reports three failures, with both numbers
  what the failing path is made of
  ok    callers of `scan` come from the call graph, not from a search
  the hypothesis, asked of the running program
  ok    probe scan('12,18')  = 12   [image]
  ok    probe scan('12,18,') = 30   [image]
  ok    probe scan('7')      = 0    [image]
  ok    probe scan('7,')     = 7    [image]
  the fix, compiled and classified, published to nothing
  ok    the edit is DIRECT and staged            (9.5 ms)
  ok    the candidate answers 30 where the image answered 12
  ok    and nothing has been published: the generation is still 0
  the tests that reach the change, and only those
  ok    four of six selected — `test_count` and `test_classify_only`
        cannot reach `scan`
  ok    and every selected test passes
  commit / rollback
  ok    the retired implementation is the one running again

  21 calls, 3 compiler sessions, 339 ms total
  cargo, invoked: 0
```

The four probes are the whole argument. Two of them *are* the diagnosis —
`12,18` gives 12 and `12,18,` gives 30, so the machine loses whatever follows
the last separator — and none of them needed a rebuild, a print statement or a
debugger. Three compiler sessions in the entire session, one of them the open.

`test_classify_only` exists to be the discriminating control for selection:
without a test that cannot reach the change, "the affected tests were selected"
is a claim a selector returning everything would also satisfy.

## 45. What the agent workload found that nothing else could

Two findings, and both were invisible to every suite before it for the same
reason: the existing mutations are generated by *substitution*. `body_arith`
replaces `wrapping_mul(7)` with `wrapping_mul(11)`. An agent rewrites a body.

### The body fingerprint moved when the source moved

The first real `replace_body` was refused: `FALLBACK(changed_outside_closure)`,
naming `::count_numbers` — a function the edit did not touch.

The discriminator took three runs to build and one line to state:

| edit to `scan`'s body | verdict |
|---|---|
| byte-identical replacement | DIRECT |
| `let mut acc = 0` → `= 1` (same length) | DIRECT |
| three extra **spaces** before the closing brace | DIRECT |
| three extra **newlines** before the closing brace | **FALLBACK, `::count_numbers`** |

§32's fingerprints are `tcx.hir_owner_nodes(owner).opt_hash`, and rustc hashes
source *positions* into them along with content. An edit that changes the number
of lines in one body changes the hash of an unrelated body further down the
file. Nothing about the second function's meaning changed; only where it is.

It fails **closed** — it refused a safe edit rather than accepting an unsafe
one, so nothing published was ever wrong. But it makes `replace_body` nearly
useless, because almost every real edit changes a line count.

rustc names the fix itself. Its hashing context reads
`hash_spans = !incremental_ignore_spans`, so `-Zincremental-ignore-spans` turns
exactly this off, and the fingerprint stays the compiler's answer rather than
becoming one this harness invented.

Sound for what a live patch claims, and the claim is narrow enough to write
down: **a patch replaces `__text` and ships no debug information at all**, so a
function whose only difference between two revisions is its line number has
byte-identical machine code and the base image's copy of it is correct. What is
not fixed up either way is DWARF — after a live patch the base image still
describes the source positions it was built from, so a debugger reads stale line
numbers for patched and unpatched code alike. That was already true and this
does not make it worse.

Two controls:

- `SPIKE_HASH_SPANS=1` puts the flag back. The gate then fails at the fix and
  names `::count_numbers`. A fix whose absence cannot be demonstrated is a fix
  nobody can check.
- It costs nothing. Three **interleaved** 60-revision `rg-lib` soaks gave
  19.84/19.83, 19.29/19.92 and 26.36/19.78 ms with the flag off and on. No
  consistent difference, and the outlier is on the faster side of its pair.
  Worth recording because a single first pair read as a 13% win and it was
  noise — the same mistake as §33, caught the same way.

Every M0 suite re-run after the change: differential 4 modes × 11 mutations
(8 agreed, 1 refused as out of artifact class, the classifier control still
answering live 8 / clean 9), generations 10 observations across 3 scenarios and
a control, soaks of `small`, `rg-lib` and `blinker-lib` at 100 revisions each —
drift 0.88×, 1.04×, 0.96×, every value exact, every verdict as required.

### `ptr::add` is constant data at `-Copt-level=0`

The fixture then refused to publish: `cannot resolve .Ldata0 for the GOT`.

The error named a symbol nobody wrote, so the first thing built was an error
that names the *function* wanting it. Working it out by bisecting the fixture
took longer than the line that reports it.

Compiling five shapes and reading `__text`'s relocations:

| shape | needs constant data |
|---|---|
| `unsafe { *out = 7 }` | no |
| `unsafe { *p.add(i) }` | **yes** |
| `unsafe { *((p as usize + i) as *const u8) }` | no |
| `(packed >> (i * 8)) & 0xff` | no |
| `b - b'0'` | **yes** (overflow check) |
| `b.wrapping_sub(b'0')` | no |

`ptr::add` carries a UB precondition check whose call takes a
`core::panic::Location`. At `-Copt-level=0` nothing folds it away — not
`-Cdebug-assertions=off`, not `-Zub-checks=no` — so the call survives and the
patch references a `Location` it cannot carry. Integer offset arithmetic
compiles to a single `ldrb` and carries nothing.

The refusal was right and the fixture was wrong, for the third time in this
document (§31, §35, here). But the finding is bigger than the fixture: the
constant-data artifact class is not an exotic corner, it is `ptr::add`,
arithmetic overflow checks, array literals, bounds checks, `panic!`, and every
string in the program. It is **the** limiting factor on what fraction of real
Rust edits can go DIRECT, and M2's task set has to be built knowing that rather
than around it.

## 46. Constant data — the artifact class, widened

§45 ended on a claim about scope: the constant-data refusal is not an exotic
corner, it is `ptr::add`, overflow checks, array literals, bounds checks,
`panic!`, and every string in the program. That makes it *the* limit on how much
real Rust can go DIRECT, and it had to move before M2's task set could be built
against a stable envelope.

### What a patch could not carry

A relocation naming `.Ldata0` had no answer. The bytes exist — cg_clif puts them
in `__DATA,__const` — but the sink delivered only functions, so the patch
referenced an address nobody had provided and `stage` refused. §43 called this
"constant data" and left it.

The backend now delivers the constants too. One hook, in
`UnwindModule::define_data`, which every constant a module defines passes
through: anonymous allocations, statics, the `_rust_extern_with_linkage_` shims.
A `DataDescription` is consumed by `define_data` and the `ObjectModule` keeps no
readable copy, so it has to be recorded there or not at all.

Delivery carries what the code **reaches**, transitively, not everything the
module defined. Two reasons, and the second is the one that matters: a patch
should carry what it references and nothing else, and a crate compiles more than
one codegen unit, so a global record holds constants belonging to units this
patch has nothing to do with. Following references from the delivered code
reaches exactly the right set and cannot cross into another unit, because a
function's relocations only name symbols its own module declared.

### Why copying a constant is sound, and exactly when

A patch that carries a constant creates a **second copy**. The base image has
its own, and the unpatched functions there go on using it. For read-only bytes
that nothing compares by address, two identical copies are indistinguishable
from one — and that sentence is the entire argument, so the conditions are
exactly the ones it needs:

| condition | why |
|---|---|
| `Linkage::Local` | nothing outside the object can name it, so no other translation unit holds a reference to keep consistent. An exported constant is a `static`, and a `static` is state |
| not writable | duplicating mutable storage is the one thing a live patch must never do: two copies of a counter is two counters |
| not thread-local | a second TLS key is a second variable |
| no function addresses in the bytes | a vtable. `ptr::eq` on two `dyn Trait` references compares vtable pointers, so two vtables for one type is observable — the single case where duplicating read-only bytes is not equivalent to sharing them |

The backend reports these as *facts* and decides nothing; the rule lives in the
consumer beside the other artifact invariants, so there is one place to look
when it is wrong. Four unit tests, one per condition.

An unnameable relocation target is counted as a function relocation rather than
dropped. A target that cannot be explained is exactly as unjustifiable as a
known-bad one.

### Three things that had to be fixed to get one mutation to pass

**`resolve` was eating the Rust mangling prefix.** It stripped a leading `_`
because Mach-O prefixes symbols with one — but a name delivered by the sink is
cg_clif's *linkage* name, which has no such prefix, and Rust's v0 mangling
begins `_R`. So `_RNvNt…panic_const_div_by_zero` became `RNvNt…` and resolved
nowhere. Invisible until now because every base-image reference a patch made
was to a `#[no_mangle]` symbol, where the two spellings are the same string.

**A `cdylib` hides the code it contains.** `panic_const_div_by_zero` was linked
into the base image at a `t` symbol `dlsym` cannot see; `-Wl,-export_dynamic`
does not help, because the hiding happens in codegen. The image is now a Rust
`dylib`. A stand-in, and named as one: in the product the base image is the
developer's binary and Blinker *is* its linker, so it holds the address of every
symbol whether or not the dynamic table does.

**`LibCall` relocations were deliberately unnamed.** Reading a constant array at
a computed index compiles to a `memcpy` at `-Copt-level=0`. These are the
entries §29's empty `RUNTIME_SUPPORT` was reserved for, and the argument for
each is the same: a function that already exists in the process, which the patch
calls rather than defines, and of which there is exactly one. Named by
cranelift-module's own mapping rather than by a table written here, so the name
is the one the object path would have emitted and not a second opinion.

### The hole the third fix opened

Switching the base image to a `dylib` broke the omitted-closure-member control.
Six mutations that had been caught now **agreed** with the clean rebuild.

The control was passing for a reason it did not claim. Dropping a member left a
relocation nothing could resolve, so the patch was refused — but once every
symbol is exported, that member resolves *silently* against the base image's
older copy of it. Which is precisely the failure §29's closed-world invariant
exists to prevent, and it was one crate-type flag away the whole time.

The cause: **set equality was only ever enforced on the object path.** Both sink
paths — `patch`'s early return and `sink_candidate` — returned a staged
candidate without ever comparing what the artifact defines against what the
codegen universe was told to emit. Both check it now, and the control is caught
deterministically by the invariant rather than by a behavioural difference that
only appears when the dropped member happens to be one this revision changed.

This is the second time in two sections that a green control turned out to be
green for the wrong reason. Both were found by changing something else.

### Where the differential stands

Thirteen mutations, six modes.

| | before §46 | after |
|---|---:|---:|
| agreed with a clean rebuild | 8 | **11** |
| refused as out of artifact class | 1 | **0** |
| refused by the classifier | 2 | 2 |

`checked_div` (a divide-by-zero check, hence a `&Location`), `const_table` (a
constant array read through a bounds check) and `new_local_helper` (`rotate_left`,
whose fallback carries `panic_const_rem_by_zero`) all publish and agree.

Two new controls:

- **omit a carried constant** — caught while building. `.Ldata0 addresses
  .Ldata1, which the backend did not deliver`.
- **corrupt a carried constant** — *not* caught while building, and that is the
  point. Every relocation still resolves, nothing is missing, the patch
  publishes; only the behaviour comparison can see it. `const_table` disagrees
  (`live returned 18446744073709551584, clean returned 24`) while `checked_div`
  correctly does not, because its constants are panic locations on a path the
  probes never take.

  The first version of this control flipped one byte of the first constant and
  passed while proving nothing — it was hitting a cold-path `Location` rather
  than the table anyone reads. It now flips every byte of every constant, and
  the requirement is at suite level: at least one mutation must notice. A
  control that can only fail by luck is not a control.

### What it cost

Nothing measurable. Constants are copied into the same slab as the code, in one
pass, before relocation. `small` sequence: MIR→CLIF 0.037 ms, Cranelift 0.126
ms, extract 0.002 ms — unchanged. Three 100-revision soaks after the change:

| fixture | drift | steady state | verdicts |
|---|---:|---:|---|
| small | 0.95× | 12.45 → 11.86 ms | 51 DIRECT, 49 refused, all exact |
| rg-lib | 0.97× | 19.34 → 18.80 ms | 51 DIRECT, 49 refused, all exact |
| blinker-lib | 0.98× | 23.79 → 23.29 ms | 51 DIRECT, 49 refused, all exact |

Generations: 10 observations across 3 scenarios and a control, all exact.

They also sit in the same `MAP_JIT` mapping as the code, and are therefore
executable. That is not a weakening, because every byte in this arena already
is, but it is not where they belong: a read-only region for constants is the
obvious next piece of hygiene and is not done.

### The fixture says what it means now

`fixtures/agent/variants/pristine.rs` was written around the limit: an offset
helper instead of `ptr::add`, `wrapping_sub` instead of `-`, bytes assembled
into a `u64` instead of an array literal. Every one of those was a place where
the fixture avoided the question rather than answering it.

They are gone. The M1 gate is green on ordinary Rust — `*data.add(i as usize)`,
`b - b'0'` — and that is the result: 21 calls, 3 compiler sessions, no cargo,
against a crate nobody had to write carefully.

What is still out of class: trait methods, generics as roots, `const fn`,
`async fn`, RPIT, anything a vtable reaches, and any constant that is not
private and read-only. Arena reclamation is still not done.

## 47. Agent Bench (M2), and what it cost to make it measure anything

M1 built the surface. M2 is the instrument that has to decide whether the
surface is worth anything, and the first thing it had to do was prove it can
measure — the same rule §31 imposed on the differential, turned on the
benchmark itself.

### The task set

49 tasks, generated rather than scraped. A benchmark has to know the right
answer and has to be re-runnable; a task mined from a repository gives neither
for free. So each is a known-good crate plus a seeded defect, which makes ground
truth exact — the fix *is* the pristine text — and lets the DIRECT/FALLBACK mix
be chosen rather than discovered. The cost is external validity, and it is real:
these are bugs of a realistic *shape* in crates smaller than real ones. Tasks
from real repositories are the obvious next thing and are not here.

Four domains — a decimal scanner, a run-length decoder, a bracket matcher, a
rolling checksum — and six families:

| family | n | |
|---|---:|---|
| local-bug | 13 | a wrong constant, an inverted guard |
| multi-function | 13 | two defects, so fixing one function is not enough |
| fallback | 8 | the defective function is `#[inline]`, so the fix cannot go DIRECT |
| state-machine | 6 | a missing end-of-input flush, a dropped underflow guard |
| off-by-one | 5 | a bound, a stride |
| feature | 4 | the body is a stub and has to be written |

Every task is verified: the suite must **fail on broken and pass on fixed**. The
first draft dropped 16 of 49 — nine because the *fixed* source failed its own
suite, since I had written the checksum expectations by hand and my arithmetic
was wrong, and three because the seeded defect was invisible to the inputs the
tests happened to use.

Both were generator bugs, and both are now designed out. Expectations are
**measured off the pristine crate** rather than asserted, which is also the
right definition for a mutation benchmark: the task is to restore the intended
behaviour, and pristine is what intended means. The invisible defects got test
inputs that see them.

That took the rejection rate to zero, which is exactly when a filter stops being
watched — so two candidates are now injected on every run that must **never**
survive: a comment reword (invisible), and a real defect checked against a bent
expectation (so pristine fails its own suite). If either is emitted, generation
fails.

### The two environments, and the three agents

`cargo` edits the file and runs `cargo test`. `blinker` uses the seven verbs.
Same tasks, same oracle, same wall clock, both warmed first — a cold `cargo
build` is dominated by linking a test harness, which is real but is not what an
agent pays per hypothesis, and timing it would flatter Blinker for reasons that
have nothing to do with Blinker.

Grading is always a separate `cargo test`, including for the Blinker runs. An
environment that graded its own work would be reporting its own opinion, and the
Blinker path publishes into an arena — so "solved" has to mean the *source on
disk* is right, not that some generation answered well.

| agent | what it is for |
|---|---|
| `null` | does nothing. Must score 0, or the oracle is not discriminating |
| `oracle` | knows the fix. Must score ~100, or a solved task is not reachable |
| `searcher` | a fixed policy, identical in both environments, that has to find the defect |

`searcher` is where the environments diverge, and holding the policy constant is
the point: what comes out is the *environment's* difference, which is the one
thing M2 can honestly measure. What a model adds is M3 and M4, and no model runs
here.

### What it found in the agent API, twice

**A second `replace_body` on the same function spliced at the wrong offset.**
The index shifts every span at or after the splice point, and the edited
function's own `body_end` *is* that point — so it moved with the rest, and was
then moved again by an explicit adjustment. Five bytes into the following item.
The M1 gate never saw it because it edits each function once; Agent Bench hit it
on its second candidate repair, and the only thing that noticed was rustc
(`the compiler session failed`). There is a regression test now.

**`run_affected` selected against the most recent edit, not all of them.** For a
revision that changed two functions it ran only the tests reaching the *second*.
The consequence is worse than under-testing: it reported zero failures while
tests reaching the first function were still failing, so the agent committed and
stopped, and the suite on disk disagreed with the API that had just said yes.
`editing` is a set now, cleared on commit and rollback. That was worth two of
the three tasks Blinker failed.

### The numbers

Controls, on the full set — 98 attempts of `null`, and `oracle` reaching
everything it was given:

| agent | env | n | solved | median |
|---|---|---:|---:|---:|
| null | cargo | 49 | **0%** | — |
| null | blinker | 49 | **0%** | — |
| oracle | cargo | 49 | 100% | 476 ms |
| oracle | blinker | 30 | 97% | **25 ms** |

`searcher`, the same fixed policy in both environments, all 49 tasks, one run:

| | cargo | blinker | |
|---|---:|---:|---:|
| solved | **100%** | 94% | |
| median | 559 ms | **37 ms** | **15×** |
| solved within 1 s | 65% | **92%** | |
| builds | 76 | **17** | 4.5× |
| bytes across the interface | 1,228,571 | **231,623** | 5.3× |
| Blinker revisions | 0 | 100 | |
| FALLBACKs | — | 17 | |

Bytes are the honest stand-in for tokens: no model runs in M2, and a byte count
is what can be measured without pretending otherwise.

All 17 of Blinker's builds are FALLBACKs — the 8 `#[inline]` tasks, where the
classifier refuses and the agent has to reach for cargo. That is the escalation
path working, and it is why the `fallback` family exists.

**Blinker solved fewer.** Both `run_affected` findings above account for two of
the three, and fixing them took the sweep to 96%. The last one was the fix
changing `const MODULUS`, which `replace_body` cannot express — and chasing
that turned out to be worth much more than one task. §48.

### What the wall clock cost, and why it is not a result

Three contention sources, each found by measuring rather than guessing, and all
three were mine:

- A candidate repair made `decoded_len` wrong, which turned `decoded_sum` into a
  non-terminating loop — with a ten-minute subprocess timeout inside a
  two-minute budget. One hang cost 39 minutes. `cargo test` is now bounded by
  its own timeout *and* the attempt's remaining budget, the domain caps its own
  loop, and the run has a `--max-total` ceiling that prints `STOPPED EARLY`
  rather than presenting a partial sweep as a whole one.
- The task set was generated **inside the working tree**, so an editor's Rust
  indexer found 49 new crates and took 327% CPU analysing the benchmark while it
  ran. Rates fell from 24 attempts a minute to two. Tasks now generate outside
  the repository; they are build output and `domains.py` reproduces them.
- Resetting `src/lib.rs` between attempts wrote identical bytes, which still
  moves the mtime, so cargo rebuilt every time. It writes only on a real
  difference now.

What remains is XProtect, which scans each freshly written dylib — and the
Blinker path writes one per session. A warm is 0.65 s in isolation and much more
under that load. It inflates *Blinker's* wall clock rather than cargo's, so it
does not favour the result above; it only made the sweep slow to obtain.

### What M2 does not claim

Nothing about models. `searcher`'s absolute success rate is a fact about its
repair table, not about anything intelligent — it can only fix bugs whose repair
it already knows. What it measures honestly is the **cost per hypothesis**, and
that is the quantity M3 and M4 turn into a claim about capability.


## 48. What the const gap actually was

M2 ended with two tasks Blinker could not solve, both wanting to change
`const MODULUS`. That looks like a missing verb. Before adding one, the
question worth asking is whether changing a `const` is *safe to publish at
all* — and it is not, and the classifier was letting it through.

### The hole

A `const` is folded into every use site at compile time. Changing one changes
the machine code of every function that reads it. §32's check compares the HIR
hashes of *functions*, and a caller's HIR does not change when a constant it
reads does — so nothing saw it.

The differential fixture had no `const` in it, which is why this had never been
tested. Adding one read in two places — inside the patch closure and by
`diff_entry`, which lives in the base image where no patch reaches it — and
changing its value:

```
  const_changed   DIRECT   6   NO   intact   FAIL
      probe 0 (0, 0): live returned 56 wrote 43666, clean returned 392 wrote 43666
```

DIRECT, published, and a factor of seven wrong. The patched `diff_root` uses the
new constant; the base image's `diff_entry` still uses the old one. Exactly
§32's failure — a change outside the closure — in the one shape §32's check
cannot see.

### The rule

Every `const` the crate defines is now fingerprinted into the contract, by
`DefPathHash` and its HIR owner hash, and a revision in which one **changed**
is refused: `FALLBACK(const_changed)`, naming it.

Deliberately blunt. A narrower rule would need the set of functions that read
the constant, and there is no cheap reliable way to get one — the reference is
folded away before MIR, which is the whole reason this is dangerous.

But only *changes*. The first version also refused additions and removals, and
that was wrong in a way §46's own suite caught within minutes: `const_table`
adds a constant and reads it, which is exactly as safe as adding a helper —
nothing in the base image can have folded a constant that did not exist. It
turned that mutation into a FALLBACK and took the corrupted-constant control
down with it, because a refused patch carries no constants to corrupt. A removal
is likewise safe: a base-image function that folded the old value holds one that
was correct when it was compiled, and anything still *referring* to the constant
fails to compile, so no verdict is reached.

### Two more things the same afternoon found

**A missing variant counted as a pass.** The differential reported `as expected`
for `const_changed` on a run where the generator had never written the file —
the trial was recorded as an ordinary refusal. A suite that cannot tell
"refused" from "never ran" can be silently emptied. A missing variant now fails
the run and says which script to run.

**The `edit_outside_closure` mutation had stopped applying.** Its anchor was
`diff_entry`'s body, which this section changed, and the generator halted there
— so every mutation after it silently stopped being regenerated. Found because
`const_changed` was one of them.

### And then the verb was not needed

With the classifier correct, `replace_body` still cannot change a constant — and
should not. What was missing was the *agent* noticing. The searcher tried the
right repair, `replace_body` left the source untouched because the change was
not in any body, and the loop moved on. Two of the three failures were that.

The policy now compares the candidate against the current source with every
target's body blanked out. A difference in what is left is a change the body
verbs cannot reach, and it escalates — which is what a person would do, and what
the FALLBACK path is for.

| `searcher`, 49 tasks | cargo | blinker |
|---|---:|---:|
| solved | 100% | **100%** |
| median | 559 ms | **40 ms** |
| within 1 s | 65% | **92%** |
| builds | 76 | **19** |
| bytes across the interface | 1,228,571 | **235,760** |

All six families, 49 of 49, in both. Blinker's 19 builds are its escalations: 17
for the `#[inline]` family, 2 for the constant changes. That is the shape the
whole design wants — the fast path takes what it can prove, and the slow path
takes the rest, with the classifier deciding rather than the agent guessing.

Every M0 suite re-run: 6 differential modes over 14 mutations, generations 10/10
exact, 22 unit tests, the M1 gate, the linker gate.
