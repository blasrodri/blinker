# Implementation Plan

## `blinker` — Incremental Mach-O Linker for Rust on Apple Silicon

This document translates `PRODUCT_SPEC.md` into a sequenced, testable, benchmarkable
delivery plan. It does not restate the spec's rationale — read that first. This is the
"how we build it, in what order, and how we know each step actually worked" document.

Repo state at time of writing: empty stub (`src/main.rs` prints "Hello, world!"; single-package
`Cargo.toml`, no workspace, no dependencies).

---

## 1. The one decision that shapes everything else

The spec's own staging (Section 8, Stages A–G) and milestone list (Section 36) already
encode the right order. The one thing worth making explicit before writing any code:

**For an agentic edit–build–test loop, the highest-ROI work is Milestone 4 (persistent
parsed-input cache), not deep incremental output patching (Milestones 6–7).**

Reasoning: `cargo build` already does incremental *compilation* — a small edit typically
recompiles one or a handful of crates and leaves hundreds or thousands of `.o`/`.rlib`
inputs unchanged. Today, every one of those unchanged inputs is still re-read and
re-parsed by `ld64` on every single link. Milestone 4 alone (cache parsed object/archive/tbd
metadata, skip re-parsing unchanged inputs) plausibly captures most of the realistic win
for a "change one function, relink, rerun test" cycle — *before* any dirty-range output
patching exists. Milestones 5–7 (incremental graph, stable layout, dirty-range writes)
compound on top of that, but they are higher-risk, higher-effort, and address a smaller
remaining slice of latency (output generation, not input parsing).

**Implication for sequencing:** treat Milestones 0–4 as the "prove the product thesis"
arc and ship/benchmark them as a coherent unit before investing in 5–7. If the Milestone-4
benchmark doesn't show a meaningful agent-session win, that's a signal to stop and
re-diagnose before building the much more complex incremental-graph/layout machinery.
This matches Rule 9 in the spec ("Measure before optimizing") and Rule 10 ("Separate
semantic correctness from performance work").

Milestone 8 (daemon) is explicitly deferred (spec Rule 8: "Avoid premature daemon
complexity") — it's a multiplier on top of a working cache/incremental story, not a
prerequisite.

---

## 2. Delivery strategy

### 2.1 Workspace evolution, not big-bang scaffolding

Do not create all 16 crates from Section 9 up front as empty stubs — that's dead weight
until a milestone actually needs them. Convert to a workspace at Milestone 0 with only the
four crates the spec's own Section 41 assignment calls for:

```
Cargo.toml (workspace)
crates/
  cli/
  arguments/
  diagnostics/
  test-support/
```

Each subsequent milestone adds exactly the crates it needs (table in Section 7 below).
This keeps `cargo check`/`cargo test` fast and keeps the crate graph legible — an agent
(or human) reading the repo at any point sees only the components that actually exist and
do something.

### 2.2 Trunk-based, always-compiling, flag-gated

- One milestone in flight at a time (spec Rule 1). No long-lived feature branches per
  milestone — land incrementally behind `--incremental <auto|on|off>` and internal mode
  selection so `main` is always shippable and the default behavior is always the safest
  one available (cold link or fallback).
- `--incremental auto` is the default from the moment incremental mode exists at all;
  `off` and the fallback-linker path must work from Milestone 0 onward and never regress.
- Every milestone's acceptance criteria (spec Section 36) becomes the merge gate for that
  milestone's work, checked by a local gate script (`cargo xtask check` or equivalent —
  see Section 3.2), not just asserted by the implementer.

### 2.3 Correctness-before-performance gate

Per spec Section 35, no performance claims or benchmark publication before the
correctness gate: 100% pass on supported fixtures, no known silent mislinks, incremental
≡ cold on every fixture. This is enforced structurally: the differential-equivalence test
suite (Section 4.4 below) is a required, non-skippable step in the local gate script
starting at Milestone 2, and the benchmark report (Section 5) is only generated from runs
where that suite is green.

---

## 3. Testability strategy

Six tiers, introduced progressively as the code they exercise comes into existence.
Every milestone's deliverables list (Section 6) states which tiers apply.

| Tier | What | Tooling | First needed |
|---|---|---|---|
| 1. Unit | Per-function/module correctness | `cargo test`, in-crate | M0 |
| 2. Property | Arithmetic, alignment, serialization round-trips, ordering invariants | `proptest` | M1 |
| 3. Fuzz | Parsers only (Mach-O, archive, tbd, response-file, cache deserializer) | `cargo-fuzz` (libFuzzer), corpus committed under `fuzz/corpus/` | M1 |
| 4. Integration fixtures | Real small/medium Rust crates linked end-to-end through the binary | `test-support` harness + fixture repo | M0 (delegating mode) → M2 (internal linker) |
| 5. Differential | This linker vs. `ld64`/`lld` on structural + runtime properties | `test-support` + `otool`/`nm`/`dyld_info`/`codesign` wrappers, process-spawn comparison | M2 |
| 6. Mutation scenarios | Spec §33.5 edit list applied programmatically to fixtures, asserting both correctness *and* incremental-mode selection | `test-support` scenario runner | M5 (needs incremental graph to be meaningful) |

### 3.1 Fixture corpus plan

Three fixed tiers, checked in as `fixtures/` (small/medium) plus a documented external
target (large):

1. **Small** (checked in, run on every local gate-script invocation): minimal `fn main`,
   multi-module binary, thread-local statics, weak symbols, panic-unwind smoke test.
   These are the fixtures from spec §33.4's first half.
2. **Medium** (checked in, run on every local gate-script invocation): multi-crate
   workspace, a build-script crate linking a small C dependency, a `cargo test` harness
   binary, a crate with proc-macros used at compile time. Covers spec §33.4's second half
   minus "hundreds or thousands of object files."
3. **Large** (not checked in — fetched by the benchmark script from a pinned commit of a
   real external Rust project, e.g. a workspace with hundreds+ crates). Used for benchmarking
   and for the "hundreds or thousands of object files" fixture requirement. Pin the
   commit so results are reproducible; document the choice and rationale in
   `fixtures/LARGE.md` once selected (open question, Section 9).

### 3.2 Test environment: local-only for now

All testing runs entirely on your own Apple Silicon Mac for the foreseeable future — no
hosted or self-hosted CI runner is being stood up at this stage. This actually matches the
target platform exactly (spec §3: `aarch64-apple-darwin` only), so there's no
cross-environment gap to worry about.

Practical consequence: "CI job" throughout this plan means a local gate script, not a
hosted pipeline. Introduce it at M0 as a single entry point (e.g. `cargo xtask check` or a
plain `./scripts/check.sh`) that runs, in order: unit + property tests, the integration
fixture suite, and (once it exists) the differential suite — so there is one command that
represents "safe to consider this milestone's work done," run by hand before every commit
that claims to close out a deliverable. Tiers 1/2/4/5 run as part of that script on every
milestone checkpoint; tier 3 (fuzzing) runs as a separate, manually-invoked longer session
(e.g. a fixed-duration `cargo fuzz run` before closing out M1 and periodically thereafter)
rather than on a nightly schedule, since there's no scheduler to hang it on yet.

Revisit hosted CI later if/when the project needs protection against regressions between
sessions where you're not the one running the checks (e.g. accepting outside
contributions, or wanting a machine-independent historical benchmark record) — it's not a
blocker for any milestone in Section 5.

### 3.3 What "done" means per parser/relocation/rule (spec Rule 3, Rule 4)

No relocation kind, argument category, or resolution rule is considered supported until:
a unit test exists, it's exercised by at least one fixture, and — once tier 5 exists — a
differential test confirms behavioral equivalence against `ld64`. "Recognized in code" and
"supported" are different claims; the compatibility matrix (Section 8) tracks this
distinction explicitly rather than letting it live only in test names.

---

## 4. Performance strategy

### 4.1 Metrics are a first-class deliverable from M0

The JSON diagnostics schema from spec §7 (`mode`, `elapsed_ms`, `changed_inputs`,
`reused_inputs`, `dirty_symbols`, `dirty_relocations`, `bytes_read`, `bytes_written`,
`fallback_reason`, plus the phase-timing breakdown from §31) is defined once in
`diagnostics` at M0 and extended (never restructured) as later milestones populate more
fields. Every integration test can assert on these fields directly instead of parsing
human-readable output — this is what makes "reused_inputs > 0 after a no-op rebuild" a
one-line test rather than a benchmark-only observation.

### 4.2 Benchmark harness

Build a `benchmarks` crate (introduced at M0, populated at M4 once there's something
non-trivial to measure) that is a scenario runner, not a microbenchmark tool — end-to-end
`cargo build`/`cargo test` timing isn't well-suited to `criterion`'s in-process model.
It:

- Drives the fixture corpus through the scenario list in spec §34.2 (clean build, no-op
  rebuild, function-body edit, signature edit, static-data edit, module add, dependency
  add, feature change, worktree switch, 50-edit session).
- Repeats each scenario enough times for stable p50/p95/p99 (spec §34.3), discarding
  first-run filesystem-cache warmup noise.
- Records machine identity (chip, core count, RAM, macOS/Xcode/SDK/Rust/Cargo versions,
  filesystem, power mode) alongside every result, per spec §34 — never publish a bare
  number without this.
- Writes results as JSON to a committed `benchmarks/results/` history so regressions are
  diffable over time, not just eyeballed once.

`criterion` is still the right tool *inside* this harness for the pure-function pieces
that don't involve spawning `cargo` (argument parsing, Mach-O parsing throughput,
relocation application, dirty-range merging) — use it there, not for the end-to-end
scenarios.

### 4.3 Regression gate

Once M4 lands (first milestone with a real perf story), the local gate script runs the
small/medium fixture subset of the scenario suite and flags if p50 regresses past a
documented threshold vs. the last committed baseline — run by hand before closing out any
milestone's work, same as the correctness checks. The large-project full benchmark stays a
separate, manually-invoked run (too slow to run on every checkpoint) but should run at
least once per milestone.

### 4.4 Targets recap (from spec §35, restated as gates, not aspirations)

- Cached parsing: ≥80% of unchanged object parsing avoided on repeated links (M4 gate).
- Small-edit incremental: p50 < 250ms, p95 < 750ms on large fixtures (M5–M7 gate).
- Agent-session: ≥2× lower cumulative linker wall time over a 50-edit session vs. the
  fastest compatible stateless baseline (i.e., plain `ld64`) (M4 first checkpoint, M7
  final target).
- All targets are gated behind the correctness suite (Section 3) being green — a fast
  wrong answer is not progress.

---

## 5. Milestone plan

Each entry: goal, crates touched, concrete deliverables beyond the spec's own list (where
this plan adds detail), test tiers that must pass, perf instrumentation added, and exit
gate. Acceptance criteria are the spec's (§36) unless noted.

### M0 — Workload recorder

*Goal: stop guessing about what `rustc` actually asks for; get a replayable corpus.*

- **Crates:** `cli`, `arguments`, `diagnostics`, `test-support`. Convert root
  `Cargo.toml` to a workspace.
- **Deliverables (concrete breakdown of spec §41/§42):**
  1. Workspace scaffold + the local gate script (`cargo xtask check` or equivalent: build
     + test) — do this before any linker logic so every later commit is gated.
  2. `arguments`: tokenizer for `ld64`-style argument syntax, response-file (`@file`)
     expansion, a typed-but-permissive `RawInvocation` (ordered arg list preserved
     verbatim for fallback replay) plus best-effort extraction of the known categories
     from spec §10.
  3. `cli`: parses project-specific flags (spec §38 subset relevant to M0:
     `--fallback-linker`, `--record-invocation`, `--replay-invocation`, `--json-diagnostics`,
     `--print-stats`, `--version`), strips them before forwarding, discovers/validates the
     fallback linker path, spawns it, propagates stdout/stderr/exit code exactly.
  4. `diagnostics`: defines the JSON schema (§7) with only the fields M0 can populate
     (`mode: "delegated"`, `elapsed_ms`, input inventory, `fallback_reason: null`); records
     input fingerprints (path, size, mtime, optional BLAKE3) per §13's fast path only
     (no verification-path hashing logic needed yet — that's M4).
  5. `test-support`: fixture-driving harness — given a fixture crate dir, run
     `cargo build`/`test` with `RUSTFLAGS`/`.cargo/config.toml` pointed at the built
     binary, capture the recorded invocation JSON.
  6. Record/replay: `--record-invocation <dir>` writes one JSON file per link; a small
     replay tool re-executes a recorded invocation against the fallback linker for
     regression/debugging use.
  7. Unsupported-argument inventory: every argument not recognized by category is
     logged (not silently dropped) and aggregated into a report across a fixture run —
     this *is* the "list of unresolved questions" deliverable.
- **Test tiers:** 1 (unit: arg parsing, response files), 4 (integration: ≥5 real fixture
  projects link successfully through the wrapper — spec's own acceptance bar).
- **Perf:** none claimed yet; `elapsed_ms` is recorded so M0's own baseline benchmark
  report (pure `ld64` timing, no wrapper logic yet) becomes the first row in the
  benchmark history from day one.
- **Exit gate:** ≥5 representative Rust projects build through the wrapper; recorded
  invocations replay byte-identically; unknown-argument report exists and is non-empty
  (proves it's actually inspecting real output, not a fixture with zero surprises).

### M1 — Object and archive inspection

*Goal: safe, fuzzed, read-only understanding of every input format — no linking yet.*

- **Crates added:** `macho`, `archive`, `tbd`.
- **Deliverables:**
  - `macho`: bounds-checked parser for the subset in spec §14. Every offset/count/multiply
    validated before use (spec §40 Rule 11: unsafe minimized, confined, documented,
    fuzzed). Stable-ID output shape (`ParsedObject` per spec §14) — no self-referential
    structures, so this can later be cached/serialized directly.
  - `archive`: `.a`/`.rlib` header + long-filename + symbol-table parsing, lazy member
    access API (parses on demand, not eagerly — this is load-bearing for M1.5's "index
    only" perf story even though extraction *decisions* aren't wired to symbol resolution
    until M2/M5).
  - `tbd`: parser for the subset of Apple `.tbd` YAML/text actually observed in the M0
    corpus (not the full format) — install names, exports, re-exports, arch/platform
    filters.
  - A small CLI inspection subcommand (or standalone `test-support` tool) that dumps
    parsed symbol/relocation counts for comparison against `nm`/`otool` — this is what
    makes the acceptance criterion ("agree with trusted inspection tools") checkable
    rather than assumed.
- **Test tiers:** 1, 2 (proptest on bounds arithmetic), 3 (fuzz all three parsers from day
  one — corpus seeded from every object/archive/tbd file touched by the M0 fixture run).
- **Perf:** parsing throughput microbenchmarks via `criterion` (bytes/sec for `macho` and
  `archive` parsing) — establishes the baseline that M4's caching claims to avoid.
- **Exit gate:** every M0-corpus input parses without unsupported-silent-behavior;
  malformed-input fuzz corpus causes zero panics/UB; symbol/relocation counts match
  `nm`/`otool` on all fixtures.

### M2 — Minimal cold linker

*Goal: first internally-produced, runnable executable. Semantic correctness only.*

- **Crates added:** `symbols`, `layout` (initial/cold mode only), `relocations`
  (supported-subset only), `dyld`, `signing`, `output`, `validation`.
- **Deliverables:** deterministic symbol resolution (spec §16, strong/weak/undefined,
  duplicate/undefined diagnostics with provenance); cold layout (spec §19.1); the ARM64
  relocation subset actually observed in M0/M1 corpora (spec §20, each with an isolated
  test fixture); one dyld metadata strategy chosen and documented (spec §22, "choose one,
  don't support multiple encodings yet"); external ad-hoc signing (Strategy A, spec §25);
  transactional output (spec §26 — this discipline starts here and never gets weakened
  later); structural + optional execution validation (spec §32.4).
- **Test tiers:** 1, 2, 4 (small fixtures now link *internally*, not just delegate), 5
  (differential suite stands up here — this is the first milestone where "internal vs.
  ld64" is a meaningful comparison, and it becomes a permanent, non-skippable step in the
  local gate script from this point forward per Section 3.3).
- **Perf:** none claimed (spec: "no persistent optimization required yet"); latency is
  recorded for the record, expected to be *worse* than delegated mode at this stage — a
  correctness milestone, not a speed one. State this explicitly in the benchmark history
  so a later reader doesn't misread a regression.
- **Exit gate:** minimal fixtures run and exit-match system-linked equivalents;
  unsupported cases hit explicit, structured fallback (never silent).

### M3 — Broad Rust debug support

*Goal: real multi-crate projects and `cargo test` binaries work, with usable debugging.*

- **Crates touched:** `archive` (lazy extraction wired to symbol resolution), `symbols`
  (archive-provided definitions), `macho`/`output` (unwind + debug-section handling per
  spec §23–24), `tbd`/`dyld` (expanded SDK coverage).
- **Deliverables:** archive extraction decisions driven by unresolved-symbol closure (spec
  §15); the documented MVP debug-info strategy (preserve input sections, regenerate on
  cached full-output links, no fine-grained patching yet — spec §23); unwind-section
  inventory + preservation (spec §24) validated against real panic/backtrace behavior, not
  just "it launched."
- **Test tiers:** 1, 4, 5, plus explicit LLDB-behavior checks (breakpoint by name, stack
  trace, source-line display, panic backtrace, test-harness backtrace — spec §23's minimum
  list) as scripted integration tests, not manual spot checks.
- **Perf:** still not the focus; watch for regressions from expanded parsing scope.
- **Exit gate:** representative medium fixtures (multi-crate, build-script + C dep,
  proc-macro-using) build and test successfully; full differential suite green; no known
  silent symbol-resolution differences (this needs to be an actual checked property —
  e.g. compare full resolved-symbol sets between internal and `ld64` output, not just exit
  codes).

### M4 — Persistent parsed-input cache

*Goal: the first milestone that should show a real number. This is the priority checkpoint per Section 1.*

- **Crates added:** `cache`.
- **Deliverables:** versioned, checksummed, corruption-tolerant cache (spec §27) storing
  the immutable-reusable half (parsed object/archive-member/tbd metadata, interned
  symbol names, SDK metadata) content-addressed so it's safely shareable across
  worktrees per spec §29; the two-stage fingerprint strategy in full (spec §13) — fast
  metadata path plus content-hash verification path, with explicit triggers for when
  verification is required; cached full-output link mode (Stage C: reuse parsed metadata,
  still regenerate full output — deliberately *not* doing dirty-range writing yet, per
  spec Stage C's own scope limit).
- **Test tiers:** 1, 2 (encode/decode round-trips, corruption handling), 4, 5, plus the
  cache-specific correctness properties: deleting the cache must not change behavior;
  corrupted entries must degrade to a miss, never a panic or a wrong link (this is
  directly testable by fuzzing the cache deserializer — tier 3 extends here too).
- **Perf:** **this is where `benchmarks` crate gets populated and the regression gate
  (Section 4.3) turns on.** Run the full scenario suite; the ≥80%-unchanged-parsing-avoided
  target (spec §35) is the milestone's actual acceptance bar, not just its own §36 wording
  ("repeated builds avoid at least 80%...").
- **Exit gate:** spec's own M4 acceptance, plus: benchmark report published showing the
  agent-session metric (cumulative wall time over a 50-edit session) against the M0
  delegated-mode baseline. **Decision checkpoint:** if this number isn't meaningfully
  better, stop and re-diagnose (see Section 1) before starting M5.

### M5 — Incremental graph

*Goal: know precisely what changed and what it invalidates, conservatively.*

- **Crates added:** `graph`.
- **Deliverables:** the reverse-dependency graph (spec §17) and centralized change
  classification (spec §18) — explicitly *not* scattered ad hoc conditionals (spec §40
  Rule 7/Rule 20: keep invalidation conservative and centralized, maintain a living
  compatibility matrix). `--explain-incremental` (spec §31) becomes real here, backed by
  the graph's actual reasoning rather than a hand-written string.
- **Test tiers:** 1, 2 (dirty-range/invalidation-rule properties), 4, 5, and 6 — this is
  where the mutation-scenario suite (spec §33.5, all 20 scenarios) becomes meaningful,
  since it's the first milestone where "was this incremental, and why/why not" is an
  assertable property per scenario.
- **Perf:** scenario suite re-run; expect improvement over M4 on scenarios where full
  parsing was already avoided but symbol re-resolution previously wasn't.
- **Exit gate:** function-body edits touch only the affected graph regions (checkable via
  graph node counts, not just wall-clock); incremental ≡ cold on every fixture (spec
  §32.2's full procedure, now automated); every non-incremental case has a machine-checked
  explainable reason.

### M6 — Stable layout

- **Crates touched:** `layout` (extended with persisted placements + slack, spec §19.2).
- **Deliverables:** `RegionPlacement` persistence, configurable growth slack, local
  expansion → extension area → thunk → relayout → full-link fallback chain (spec §19.2),
  with each fallback tier individually testable (force each precondition in a fixture and
  assert the *correct* tier fires, not just that *some* fallback fires).
- **Test tiers:** 1, 2, 4, 5, 6.
- **Perf:** address-stability rate on the small-edit scenario subset.
- **Exit gate:** ≥95% unchanged-symbol-address preservation on selected benchmarks;
  layout overflow provably cannot corrupt output (this wants a dedicated stress/fuzz test
  that forces overflow repeatedly and checks output validity after every occurrence).

### M7 — Dirty-range output updates

- **Crates touched:** `output` (dirty-range tracking + partial rewrite), `signing`
  (measured separately per spec §25).
- **Deliverables:** dirty byte-range tracking + merging (spec §33.2's property-test
  target), safe base-output reuse/cloning, partial rewrite with correct global-metadata
  regeneration where still required, crash-safety re-verified under the transactional
  model (spec §26) now that writes are partial rather than whole-file.
- **Test tiers:** 1, 2, 4, 5, 6, plus interrupted-write fault-injection tests (kill the
  process mid-write in a test harness, assert previous output still valid — this is the
  concrete form of spec §36 M7's "interrupted writes leave the previous executable valid").
- **Perf:** bytes-written vs. output-size ratio on small edits becomes a tracked metric;
  this is the milestone that should close the gap to the p50 < 250ms / p95 < 750ms target.
- **Exit gate:** spec's own M7 bar, plus: full agent-session benchmark re-run, target ≥2×
  vs. stateless baseline checked as a hard number, not an estimate.

### M8 — Daemon and multi-agent support

*Deferred until M0–M7's single-shot story is solid — spec Rule 8.*

- **Crates added:** `daemon`.
- **Deliverables:** versioned local protocol (spec §28), in-memory hot state, worktree
  isolation (spec §29 — shared immutable cache, isolated mutable per-worktree state,
  verified by a test that mutates one worktree and asserts zero effect on a sibling's
  output or graph), cancellation, per-session memory bounds, crash recovery.
- **Test tiers:** 1, 4, 5, 6, plus concurrency-specific tests: concurrent requests to
  distinct worktrees succeed independently; concurrent requests to the *same* logical
  target serialize correctly (spec §28's "no two concurrent requests mutate the same
  output state") — this needs deliberate race-inducing test harnesses, not just
  sequential integration tests.
- **Perf:** cross-worktree cache-sharing win, measured as a distinct benchmark scenario
  (spec §34.2's "worktree switch").
- **Exit gate:** spec's own M8 acceptance criteria verbatim.

---

## 6. Crate introduction timeline

| Crate | Introduced | Notes |
|---|---|---|
| `cli` | M0 | Grows CLI surface per §38 as milestones add flags |
| `arguments` | M0 | |
| `diagnostics` | M0 | Schema fixed early, extended not restructured |
| `test-support` | M0 | Becomes the home for fixtures, differential tooling, scenario runner |
| `macho` | M1 | |
| `archive` | M1 | Extraction *decisions* wired at M3/M5 |
| `tbd` | M1 | |
| `symbols` | M2 | |
| `layout` | M2 (cold) → M6 (stable) | |
| `relocations` | M2 | Grows per-kind as corpus demands (never speculative) |
| `dyld` | M2 | |
| `signing` | M2 | External ad hoc only until spec explicitly revisits Strategy B |
| `output` | M2 (full rewrite) → M7 (dirty-range) | |
| `validation` | M2 | |
| `cache` | M4 | |
| `graph` | M5 | |
| `benchmarks` | M0 (scaffold) → M4 (real content) | |
| `daemon` | M8 | |

---

## 7. Tooling and dependency choices (proposed, not yet committed)

- **CLI parsing:** `clap`, but only for the project-specific flag namespace — raw linker
  arguments are hand-parsed in `arguments` since they don't follow clap's grammar.
- **Diagnostics/tracing:** `tracing` + `tracing-subscriber` for the phase-span timing
  spec §31 wants; the JSON diagnostics writer is a custom subscriber layer so field names
  stay pinned to the spec schema rather than tracing's own shape.
- **Hashing:** `blake3` crate, matching spec §13's explicit suggestion.
- **Serialization:** need a versioned, corruption-tolerant format for `cache` (spec §27).
  Leaning `bincode` + explicit schema-version prefix byte + checksum, over something like
  `rkyv`, for simplicity and easier corruption handling — **flagged as an open decision**,
  revisit at M4 kickoff once cache access-pattern (random vs. sequential) is clearer.
- **Property testing:** `proptest`.
- **Fuzzing:** `cargo-fuzz` (libFuzzer-based) — standard for parser fuzzing, matches spec's
  fuzz-target list directly.
- **Mach-O/archive/tbd parsing:** hand-rolled in `macho`/`archive`/`tbd`, not built on top
  of the `object` crate. The spec's own bar (§14: "every offset, count, multiplication, and
  range must be checked," stable IDs, minimized documented unsafe) is a *design* constraint
  as much as a correctness one — reusing a general-purpose parser would fight the
  stable-ID/serializable-output shape the caching and graph layers depend on. **Flagged as
  an open decision** — worth a short spike comparing hand-rolled M1 effort vs.
  wrapping `object` before committing, since it's the single biggest scope item in M1.
- **Benchmarking:** `criterion` for pure-function microbenchmarks; custom scenario runner
  (Section 4.2) for end-to-end timing — no dependency needed there beyond `std::process`
  and careful warmup/repetition handling.
- **External tool wrapping (differential tests):** shell out to `otool`, `nm`, `dyld_info`,
  `codesign` and parse structured output where possible; per spec §32.4, prefer a
  structured parser over fragile text-scraping wherever the tool supports it (e.g. some of
  these have machine-readable or at least stably-formatted output worth wrapping once,
  in `test-support`, rather than re-parsing ad hoc per test).

---

## 8. Compatibility matrix (mechanism, not content)

Per spec Rule 20 ("maintain a continuously updated compatibility matrix"), this plan
proposes the matrix live as a generated artifact, not a hand-maintained doc that drifts:
a `cargo xtask compat-matrix` (or `test-support` subcommand) that walks the argument
categories (§10), relocation kinds (§20), and Mach-O features (§14) each tagged in code
with their support state (`Unsupported`, `RecognizedNotLinked`, `Supported+Tested`,
`Supported+Differential`), and renders it to `COMPATIBILITY.md` as a step in the local
gate script (fails if the doc is stale relative to the tags). This directly prevents the
"claims support before an integration fixture proves it" failure mode (spec Rule 4) by
making the claim mechanically derived from test coverage rather than asserted in prose.

---

## 9. Risks and open questions

- **Large-fixture selection** (Section 3.1): which real external project to pin for the
  "hundreds/thousands of object files" and benchmark-scale fixture. Needs a concrete pick
  before M4 benchmarking can be meaningful. Candidates should be large Cargo workspaces
  with heavy dependency graphs and no `x86_64`-only or non-Silicon-relevant dependencies.
- **`object` crate build-vs-wrap decision** (Section 7): biggest single scope lever in M1;
  worth a short time-boxed spike before committing.
- **Cache serialization format** (Section 7): revisit with real access patterns at M4.
- **dyld metadata encoding choice** (spec §22): needs a concrete pick ("choose one
  supported strategy") backed by inspecting what `rustc`'s actual M0-corpus outputs use —
  this falls out of M0/M1 data rather than being decidable up front.
- **Debug-info and signing strategy** are both spec-mandated "choose and document one"
  decisions (§23, §25) — default to the spec's own "preferred initial strategy" /
  "Strategy A" unless M2/M3 data says otherwise; don't relitigate unless a fixture forces it.

---

## 10. Immediate next step

Start M0 exactly as scoped in Section 5 and spec §41–42. First concrete actions, in order:
convert `Cargo.toml` to a workspace; add the local gate script (even before any linker
code exists, so it's gating from commit one); then `arguments` (tokenizer + response
files) and `cli` (delegating wrapper) together, since neither is independently
testable against real `rustc` output without the other.

---

## M4 design, as of the corrected measurements

> **Struck by finding 41.** The premise below was tested and is false:
> deserialising a `ParsedObject` is 1.7-3.5x *slower* than parsing the object.
> Cache the relocated output keyed by CGU instead. Kept for the reasoning, not
> as a plan.

Recorded here rather than started, because the measurements that justify it are
now solid and the implementation is a clean unit of work.

### What it is worth

From finding 40 (25 iterations, warmup discarded, sd < 1 ms per stage), against
`ld-prime` at 32.3 ms and blinker at 39.7 ms on the same 27-input link:

| cached through | saving | blinker becomes |
|---|---|---|
| parse | 27% | ~29 ms — ahead of ld-prime |
| + resolve | 55% | ~18 ms |
| + relocate | 80% | ~8 ms |

A parse cache alone is sufficient to make blinker the faster linker. That is
the M4 target.

### The key

**Content hash, never path.** Finding 15: rustc's object filenames carry a
per-build session component that changes every build, so a path-keyed cache has
a 0% hit rate by construction. The CGU component of the name is stable, and the
file's BLAKE3 hash is stable and self-verifying; blinker already hashes inputs
when `--blinker-strict-fingerprints` is set.

The key must also include a **schema version**, bumped whenever `ParsedObject`
or any type it contains changes shape. A cache hit that deserializes a stale
layout is worse than a miss.

### The open question: is deserialising faster than parsing?

This is the premise the whole design rests on and it is **not yet measured**.
`ParsedObject` is mostly `Vec<InputSymbol>`, and symbol names are `String`s —
thousands of allocations either way. Parsing currently costs 8.1 ms for 17 MB
across 27 files. If deserialisation allocates the same strings, it may not win.

The experiment to run first, before writing a cache: serialise the parsed
objects, then time deserialisation against re-parsing. If the margin is thin,
the answer is to change what is stored — string interning with a single shared
buffer, or offsets into the original file rather than owned `String`s — rather
than to pick a faster codec.

`bincode` 3 is available (`cargo add bincode@3`, no features needed) and is the
obvious first codec to try; `ciborium` is already in the local registry cache
if working offline matters. Neither choice matters until the premise above is
tested.

### Where it lives

A `blinker-cache` crate between `blinker-macho` and `blinker-link`, with
`load_objects` consulting it. The cache directory should default under
`CARGO_TARGET_DIR` when set, so a `cargo clean` clears it and it never outlives
the build tree it describes.

## The cache design, as measured (supersedes the struck M4 above)

Three premises were tested before any of it was written, and two of them
reshaped it. Every number below is measured on a 56-input Rust debug link
(26.1 ms internal, 1.03 MB output).

| premise | measured | effect |
|---|---|---|
| deserialising a parse beats re-parsing | 0.43 ms parse vs 0.75–1.50 ms | **killed** the parse cache (41) |
| loading patched bytes beats relocating | 0.065 ms vs 7.3 ms — 112× | design confirmed (59) |
| content-hashing inputs is cheap | 7.28 ms vs 7.3 ms saved | **killed the key**, not the cache (60) |
| ...hashing only rustc's output | 0.16 ms — 45× | design restored (61) |

### What is stored

`crates/cache`, between `blinker-macho` and `blinker-link`. Per link:

- **patched output section bytes** — the artifact, and the only large thing;
- **per object**: its input key, the output ranges its bytes occupy, the
  symbols its relocations resolved against, and the binds and rebases its
  relocation pass produced;
- **every defined symbol's address**, sorted by name hash, for diffing.

Not the parse, not the layout, not the symbol table — those are inputs to the
computation, and finding 41's rule is that a cache only pays when the artifact
is flatter than the computation that made it.

### The three reuse conditions

An object's bytes are reused only if its input is unchanged, its contribution
has not moved, and nothing it references has moved. The third is what makes
this a graph: an untouched object holding a pointer to an edited one is stale
through no fault of its own. Validating it is a set probe per dependency
against the addresses that changed since the last link — not a lookup per
relocation, which would be the work being avoided.

### Two keys, because there are two populations

98.2% of the bytes are toolchain rlibs at content-addressed paths
(`libstd-4f24f0876fd27385.rlib`) and are keyed on metadata. 1.8% is rustc's own
codegen output, renamed every build (finding 15), and must be hashed. Anything
unrecognised is hashed: trusting a path that lied produces a wrong binary,
while hashing unnecessarily costs microseconds.

### Remaining work, in order

1. **Record during a cold link.** `apply_relocations` already resolves every
   target; have it accumulate per-object dependency hashes, and slice the
   binds and rebases it produces by the object that produced them. Write the
   cache after `assemble`.
2. **Reuse on rebuild.** After layout, diff the address tables, then copy
   cached bytes over the reusable objects' ranges and skip them in the
   relocation loop. Everything else runs as it does today.
3. **Fall back loudly.** Report reuse in `LinkRecord` (`reused_inputs` exists
   and has been `null` since M0), and take the cold path on any mismatch.

The ceiling for this shape is the relocate stage: 7.3 ms of 26.1, so ~28%,
taking blinker from ~1.15× ld-prime to roughly 0.85×. Going below that means
reusing `resolve` (6.3 ms) and skipping the read of unchanged inputs, which
needs the symbol table cached too — a later step, and one whose premise should
be measured the same way before it is built.

## Cache status: reuse works, and where the remaining time is

Steps 1 and 2 of the three above are done. A warm link on the 47-object Rust
fixture reuses **every** object and produces a byte-identical binary.

```
                        internal link   relocate
  no cache                 25.3 ms        7.4 ms
  warm cache               22.6 ms        4.5 ms
```

(15 iterations, warmup discarded, sd < 0.2 ms per stage.)

Relocate falls by 2.9 ms rather than by all 7.4: reuse still pays the byte copy
and the recording pass that keeps the cache current. Against `ld-prime` the
process-level comparison is currently too noisy to read — 49% spread on the
baseline over 12 interleaved runs — so it is not quoted here as a result.

### What the cache cannot reach

`read+parse` (6.9 ms) and `resolve` (6.2 ms) are now 58% of the link and are
untouched, because both run before the cache is consulted: the addresses have
to exist before anything can be checked against them. Getting under them means
caching the symbol table and the layout, so unchanged inputs need not be read
at all — and per finding 60, the premise to measure first is whether skipping
those reads survives the cost of proving the inputs unchanged.

### Still open

- **Step 3, fallback reporting.** `reused_objects` is on `LinkTimings` but not
  in `LinkRecord`, so a JSON consumer cannot see the hit rate. Finding 64 is
  the argument for surfacing it by default rather than on request.
- **A test that reproduces finding 64.** The C fixture written for it passes
  with the fix reverted; only the Rust link distinguishes them.
- **Tentative (common) symbols** (finding 65), unrelated to the cache but
  found by it.
- **Dead-stripping.** Output is 2.0x ld-prime's.

## State at the end of this stretch

blinker links C and Rust on Apple Silicon, signs its own output, handles
`panic=unwind` identically to the system linker, and caches across links.

```
  ld-prime                     28.4 ms   1.00x
  blinker, cold                31.3 ms   1.10x
  blinker, unchanged relink    10.4 ms   0.37x
```

Cold-link profile, 20 iterations, sd < 0.3 ms per stage:

```
  read+parse   5.0 ms   resolve  6.4 ms   layout 1.6 ms
  relocate     7.3 ms   emit+sign 2.2 ms
```

### The two things left, both milestones rather than tasks

**1. The partial fast path** — the edit-compile case.

The whole-image path (finding 67) fires only when *nothing* changed. When one
codegen unit changed, the link falls all the way back to the full pipeline at
~22 ms, even though 46 of 47 objects are provably untouched.

Closing that means relocating the changed object alone and patching its bytes
into the cached image. The blocker is not the relocation — it is that
`apply_relocations` needs the whole link state to run: the address map, the
GOT/stub/TLV tables, section addresses, personality fields, thread-local
offsets. Reaching it from cache means either caching that state (and finding 41
warns about what happens when the cached form is less flat than the computation)
or restructuring the pass to take a narrower context.

The check is already cheap enough — 0.18 ms proves which inputs changed — so
the payoff is the difference between 22 ms and something near the 10.4 ms the
unchanged case gets.

**2. Dead-stripping** — finding 70.

The output is 2.02x ld-prime's, and all of it is unreachable code plus the
unwind, exception and literal data that serves it. Every input object sets
`MH_SUBSECTIONS_VIA_SYMBOLS`, so cutting sections at symbol boundaries is
legal; the work is that atoms, not sections, become the unit of layout, which
touches placement, `AddressMap`, relocation, and the tables that index by
function address.

### Rules this project runs on, earned rather than assumed

- Measure the premise before writing the code. Three cache designs were killed
  or reshaped this way (41, 60, 68); each measurement cost minutes.
- A cache is worth building only when the artifact is flatter than the
  computation that produced it (59), and it should be checked *before* the
  expensive work rather than after (67).
- A null result needs the same proof of provenance as a positive one (58).
- An instrument needs its own negative control; a counter that reports success
  while nothing happens is the failure it was added to detect (66).
- A test proves something only when the failure would produce a different
  observable value, not merely a different internal state (63, 66, 69).
