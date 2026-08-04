# blinker

A resident, incremental Mach-O linker for Rust on Apple Silicon.

**It makes a `cargo build`'s linking 2.7× faster, out of the box.**

```
a full build of this workspace — 15 links, 23 inputs at the median

  ld64 (cc)            765, 859, 857 ms
  blinker              304, 296, 296 ms      2.7×
```

That is the whole claim, and it is deliberately the *build*'s number rather
than a link's. Set `linker = "…/blinker"` and nothing else; it links internally
by default and starts a resident linker for the next link if none is running.

## Why this is the number

A linker is not slow. A *build* is slow, and it is slow because it links the
same programs over and over with almost nothing changed between them — and a
linker that exits after every link has to be told the whole program again each
time.

So the thing worth attacking is not the cost of one link. It is the cost of the
hundredth link of a program the linker has already seen ninety-nine times. That
is why blinker is resident: staying alive is worth **1.6×** on its own here
(296 ms against 484 for the same links one-shot), before any incremental
machinery does anything at all.

It is also why the median matters more than the maximum. A real build's links
have 23 inputs at the median and 132 at the largest. Optimising the tail is
optimising the case that happens once.

The thing standing in front of this number is measured in finding 204. A cold
build of this workspace submits **eleven links within 129 ms of each other**,
and the daemon serves one at a time: their round trips climb 35.7 → 205.5 ms in
a staircase, 301 ms of wall clock for about 40 ms of linking. `build-links.py`
replays links serially, so the 2.7× above does not include it.

The incremental build does not hit it — after touching one crate this workspace
issues exactly one link — so concurrency buys the cold and near-cold build and
buys the edit loop nothing. `BLINKER_TRACE_WAIT=<file>` writes the trace that
shows either.

## Where it stands

| | |
|---|---|
| **A whole `cargo build`'s links** | **2.7×** the system toolchain |
| **Edit relink, resident** | **20.6 ms** wall against `ld-prime`'s 34.3 ms |
| A body edit, debug self-link (1,099 objects) | 41.9 ms wall, 31.9 ms linking |
| Cold link, 238 objects | 1.03× `ld-prime` — inside the spread |
| Output size | 0.85× `ld-prime`'s on a large link, with dead-stripping |
| Cold link, 5,637 objects | **1.70× slower** |
| Edit relink, 5,637 objects | level — ~390 ms against 367 ms |

The last two rows are the honest cost of the design. A resident linker's first
link pays for state that no link has reused yet, and on a very large program
that bill arrives all at once. blinker wins on repetition and on the common
case; it does not yet win on a single cold link of a huge program.

`cargo test` binaries, `panic=abort` and `panic=unwind`, caught panics,
destructors, symbolized backtraces and a `dsymutil`-readable debug map all work.
Output kinds blinker cannot produce — `-dynamiclib`, so every proc-macro crate —
are delegated automatically with the reason recorded.

## What the large link spends its time on

Kept because it is where the remaining work is, not because it is the headline.
On a relink where 1 object in 5,637 has a reachability projection that moved and
197 of 506,405 addresses changed:

```
  read_and_parse   71 ms      of which ~29 ms is the extraction frontier
  relocate         66 ms      98% of relocations reused; the rest is global
  symbols          35 ms      1.7M entries; 21 ms with retained runs (200)
  emit             40 ms      was 62 before findings 198-200
  layout           33 ms
  dead_strip       32 ms      of which 21 ms traversing a graph that did not move
  write            14 ms
```

Every one is proportional to the whole program rather than to the edit. That is
the work of findings 191 onward — and the largest single item found so far was
not a stage at all but a 1.7-million-element clone sitting in the gap *between*
two measured stages (199).

Three results of that work are built, verified and switched off, each behind a
flag and each for a reason recorded in the findings rather than a plan to get to
it: `BLINKER_DELTA_LIVENESS` (2 ms) and `BLINKER_RETAIN_STRINGS` (14 ms, and it
costs the warm-equals-cold byte comparison the test suite leans on).
`BLINKER_MEMORY_BUDGET` bounds the per-target state in megabytes, default 1024.

`--blinker-delegate` delegates everything, `--blinker-no-daemon` or
`BLINKER_NO_DAEMON=1` links in-process, and `--blinker-daemon-stop` stops a
resident linker.

See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) for the product definition,
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for the milestone sequence,
and **[FINDINGS.md](FINDINGS.md)** for the 211 places reality contradicted the
plan — several of them contradicting earlier entries in the same file.

## What is not done

- **Speed at scale.** 1.70× the system linker on a large cold link, against
  1.03× on a small one, and the resident relink of a large program is level
  with a cold `ld-prime` — where it was 51% slower at the start of the work
  recorded in findings 179-186. The reason is that the stages left are
  proportional to the whole program rather than to the edit: dead stripping
  rebuilds the reachability graph even when one object in 5,637 moved, and the
  image is assembled, hashed and written whole even when 98% of relocations
  were reused.

  Not the reason, though it was believed to be for a long time and this file
  said so: materialising every symbol name into an owned `String`. Removing that
  allocation was measured at about 4 ms of a 780 ms link, because the parse has
  been on every core since finding 161 and 976,000 allocations spread over
  fifteen cores are not 976,000 allocations (finding 168).
- **Dynamic library output.** Proc-macro crates and `cdylib`s are delegated
  rather than linked. Correct, but it means a workspace is only partly linked
  by blinker.
- **A memory budget aimed at the wrong four hundred megabytes.** Per-target
  state is now bounded in bytes and evicted least-recently-used —
  `BLINKER_MEMORY_BUDGET`, default 1024 MB — which replaced three counts that
  each counted a different unit. It reports itself: 291 MB held on an ordinary
  large relink, 400 MB with the retained symbol table on.

  The measurement that made possible then said the budget covers 400 MB of a
  3.0 GB process. The rest is parsed inputs, still bounded by a window of four
  links rather than by bytes, and about a third is memory the allocator has
  freed and not returned — `malloc_zone_pressure_relief` does not move it
  (finding 201).
- **Incremental output.** The image is rebuilt and rewritten whole. The layout
  machinery for stable addresses across edits exists, is tested, and holds
  (9,719 of 9,722 contributions keep their address on an ordinary edit) — but
  unchanged bytes are still copied and re-emitted rather than left where they
  are.

  Worth less than it sounds, and measured rather than assumed: 59% of the
  output is `__LINKEDIT` and 46% of it is symbol-name text, and one symbol
  added near the front shifts every string offset after it. The ceiling on
  never touching an unchanged byte is about 9 ms of a large link — see
  finding 187, and finding 186 for the version of it that measured slower than
  writing the file whole.
- **`x86_64`, universal binaries, LTO.**

## A note on the numbers

Every performance figure above is from an interleaved A/B against the system
linker on real captured link arguments, not a synthetic benchmark. They moved a
lot: blinker was measured at 0.92× on a 47-object fixture and turned out to be
**7.44×** on a real binary, because a linear scan that was invisible at small
scale was quadratic at large. See finding 77 — and treat any single-fixture
number here, including these, as a claim about one workload.

They also go stale. This table said 2.92× and "the daemon is not implemented"
for long enough that a review of the project reasoned from it and recommended
work that had already been done. The numbers here are re-measured when they
change; if they disagree with FINDINGS.md, the finding with the higher number
is the one that was measured last.

## Measuring it

Build a workload first. It is rebuilt from the repository rather than found
lying around, because every workload this project measured before finding 93
was archived into a temporary directory and is gone:

```bash
scripts/workload.py self                     # blinker linking itself, release
scripts/workload.py self-debug --profile debug
scripts/workload.py rg --project ~/src/ripgrep
```

Each lands in `target/workloads/<name>/` with an `argv.txt`, a copy of every
input, and a manifest. Then:

```bash
scripts/bench.py  target/workloads/self/argv.txt            # vs the system linker
scripts/bench.py  target/workloads/self/argv.txt --profile  # stage breakdown
scripts/ab.py     target/workloads/self/argv.txt A B        # two blinker builds
scripts/ab.py     target/workloads/self/argv.txt A A        # the noise floor
```

The harnesses interleave the arms, discard warmup, verify every run succeeded,
and report spread. Each of those exists because an earlier version without it
produced a wrong number that was believed — see the header of
`scripts/bench.py`. `scripts/ab.py` reports the link and the process around it
separately, because on a 60 ms run 20 ms of it is spawn and page cache, and
measuring that as though it were linking spreads the result by 42%.

## Requirements

- Apple Silicon Mac (`aarch64-apple-darwin` is the only supported target)
- Rust stable toolchain
- Xcode command line tools (`/usr/bin/cc` must exist)

## Build

```bash
cargo build --release
```

The binary lands at `target/release/blinker`.

## Use it as your linker

Try it first, from the project you want to build. This writes no configuration
and builds into `target/blinker-try`, so nothing about the project changes and
the next ordinary `cargo build` is not made to rebuild anything:

```bash
/absolute/path/to/blinker --blinker-try build
```

Then, from the same directory:

```bash
/absolute/path/to/blinker --blinker-install
```

which writes what you would have written by hand, with the running binary's own
absolute path:

```toml
# .cargo/config.toml
[target.aarch64-apple-darwin]
linker = "/absolute/path/to/blinker"
```

An existing `.cargo/config.toml` is edited rather than replaced — comments,
ordering and every other setting survive — and a `linker` already pointing at
something that is not blinker is reported rather than overwritten. Running it
twice does nothing the second time.

Then build as normal:

```bash
cargo build
cargo test
```

That is the whole of the setup. Blinker links internally, and the first link
starts a resident linker that the rest of the build reaches.

**To restore the default linker**, run `blinker --blinker-uninstall` from the
project — it removes the key it added, and the file and `.cargo` directory too
if they held nothing else — then `blinker --blinker-daemon-stop`. The daemon is
the one piece of state a build leaves behind, and it would go on its own twenty
minutes later regardless.

## Corpus tooling

`blinker-corpus` builds projects through blinker with recording on, then reports
what the links actually contained:

```bash
cargo build
./target/debug/blinker-corpus gather --out corpus          # nine built-in fixture shapes
./target/debug/blinker-corpus external --project ~/src/ripgrep --out corpus
./target/debug/blinker-corpus report --records corpus      # argument inventory
./target/debug/blinker-corpus baseline --repeat 3          # timing comparison
```

The inventory's most important line is the unmodelled-argument list: anything
there is a spelling the classifier does not understand yet. Before adding one,
check its arity in `crates/arguments/src/reference.rs` — assuming a value-taking
option takes none causes its values to be silently read as input files.

See [FINDINGS.md](FINDINGS.md) for what the corpus has established so far.

## Recording a corpus

Recording captures real linker invocations from real projects, which is how
the arity table in `blinker-arguments` and most of FINDINGS were derived:

```bash
cargo build --config 'target.aarch64-apple-darwin.rustflags = ["-C", "link-arg=--blinker-record-invocation=/tmp/corpus"]'
```

Each link writes `/tmp/corpus/<output-name>-<pid>.json` plus a
`<output-name>-<pid>.inputs/` directory holding a copy of every input file.

The archived inputs are what make a recording **replayable**. `rustc` writes the
object files for a link into a temporary directory it deletes the moment the
linker returns, so a recording that stored only paths would be dangling by the
time you opened it. Replay a recorded link with:

```bash
blinker --blinker-replay-invocation=/tmp/corpus/mycrate-12345.json
```

Replay rewrites the output path into a scratch directory, so it can never
overwrite a real build artifact.

## Options

blinker occupies the position `rustc` invokes as the C compiler driver, so its
argument vector is full of driver flags (`-o`, `-L`, `-l`, `-arch`, `-Wl,…`).
Every blinker option therefore carries a `--blinker-` prefix that cannot collide
with a driver or `ld64` flag, and is stripped before the remaining arguments are
forwarded.

| Option | Meaning |
|---|---|
| `--blinker-fallback-linker <PATH>` | Linker to delegate to (default: discovered) |
| `--blinker-record-invocation <DIR>` | Record this invocation, with inputs, into `DIR` |
| `--blinker-replay-invocation <FILE>` | Replay a recorded invocation |
| `--blinker-json-diagnostics <PATH>` | Write the machine-readable record to `PATH` |
| `--blinker-diagnostics <LEVEL>` | `quiet` \| `normal` \| `verbose` |
| `--blinker-print-stats` | Print the human-readable summary |
| `--blinker-strict-fingerprints` | BLAKE3-hash every input rather than trusting metadata |
| `--blinker-version`, `--blinker-help` | Version / help |

Because these must travel through `rustc` to reach the linker, pass them as
`-C link-arg=--blinker-…`. Use the inline `=` form so `rustc` cannot separate an
option from its value.

### Fallback linker discovery

Highest precedence first:

1. `--blinker-fallback-linker <PATH>`
2. the `BLINKER_FALLBACK_LINKER` environment variable
3. `/usr/bin/cc`, then `/usr/bin/clang`

An explicitly configured path that does not exist is an error rather than a
silent fall-through to a default — quietly substituting a different linker would
change link semantics without saying so.

The default is `cc` rather than `ld` because that is what `rustc` itself invokes;
see [FINDINGS.md](FINDINGS.md).

## Development

```bash
./scripts/check.sh          # full gate: fmt, clippy, unit + end-to-end tests
./scripts/check.sh --fast   # skip the slow real-cargo-build tests
```

There is no hosted CI. blinker is developed on the same Apple Silicon hardware
it targets, so the gate script runs locally and is the merge bar for every
milestone deliverable.

### Layout

| Crate | Role |
|---|---|
| `crates/cli` | Entry point, driver, fallback execution, record/replay |
| `crates/arguments` | Argument classification and response-file expansion |
| `crates/diagnostics` | JSON record schema, timings, input fingerprints |
| `crates/test-support` | Fixture generation and the end-to-end harness |

Crates are introduced as milestones need them rather than scaffolded up front;
the full target layout is in the implementation plan.
