# Spike S0 — Rust frontend lower bound

**Recommendation: B**, with one large caveat that S0 did not measure and that
should be measured before R1 begins (§8).

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

## 8. Known limitations — one of them matters a great deal

**S0 measured a single-crate edit.** In every fixture the edited file belongs to
the crate being compiled, so the measurement covers one compilation. A real edit
to a library crate forces recompilation of every crate downstream of it: this
repository's own relink harness reports a 13-input blast radius for a one-line
edit to `pulsevm`, and 70 inputs for `blinker`. **The 62 ms figure is a lower
bound for the easiest topology, not the expected case**, and the expected case
could be several times larger.

This is the single most important thing S0 did not answer, and it should be
answered before R1 is begun rather than after. It needs no new machinery — the
harness already replays a captured cargo invocation, and what it needs is to
replay the *sequence* of invocations cargo issues for a downstream edit.

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
