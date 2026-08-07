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
