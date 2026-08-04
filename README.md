# blinker

An incremental Mach-O linker for repeated Rust development builds on Apple
Silicon, aimed at cutting the link latency in edit–build–test loops run by
humans, IDEs, and coding agents.

**Status: it links Rust, and it links itself.** blinker builds its own 5.4 MB
binary from 921 objects, dead-strips it, signs it, and the result links other
programs.

| | |
|---|---|
| C programs | work; behaviour matches the system linker |
| Rust, `panic=abort` and `panic=unwind` | work, including caught panics, destructors and symbolized backtraces |
| `cargo test` binaries | work |
| Debug information | `SO`/`OSO`/`FUN` debug map emitted; `dsymutil` reads it back |
| Dylibs, bundles, partial links | delegated to the system linker, with a recorded reason |
| A whole `cargo build`'s links | **2.7×** the system toolchain, out of the box |
| Output size | **0.85×** `ld-prime`'s on a large link, with dead-stripping |
| Cold link, small (238 objects) | **1.09×** `ld-prime` — inside the spread |
| Cold link, large (5,637 objects) | **1.75×** `ld-prime` |
| Edit relink, small, resident | **11.8 ms** against `ld-prime`'s 29.7 ms cold |
| Edit relink, large, resident | **342 ms** against `ld-prime`'s 311 ms cold |

The per-link rows are where the two scales disagree: on a small link the
resident linker is comfortably faster than a cold `ld-prime`, and on a large one
it is still slower. Per-link figures are minima over ten alternating relinks
(`scripts/relink.py <workload> --daemon`), which is the statistic that survives
a loaded machine; medians run 5–15% higher.

The first row is the one a developer feels, and it took until finding 189 to
measure. `cargo build` on this workspace performs sixteen links with a median of
23 inputs — the 5,637-object link the rows below are taken from is the tail, not
the shape. Across a build's worth of links:

```
ld64 (cc)             732-812 ms
blinker              283-305 ms
blinker one-shot      470-498 ms
```

`scripts/build-links.py` produces that. The middle row is what
`linker = "…/blinker"` gets you, with no other configuration: blinker links
internally by default and engages a resident linker by default, starting one
for the next link if none is running. The third row is what that second default
is worth, measured by turning it off with `--blinker-no-daemon`.

Both of those were opt-in until finding 190, which means the documented setup
used to install a program that delegated every link to Apple's linker and never
started the daemon it was built around.

What the large relink spends its 342 ms on, when one object in 5,637 changed:

```
  emit             90 ms      __LINKEDIT is 59% of the image (187)
  relocate         67 ms      98% of relocations reused; the rest is global
  read_and_parse   46 ms
  dead_strip       29 ms      of which 17 ms traversing a graph that did not move
  layout           27 ms
  write            17 ms
```

Every one of those is proportional to the whole program rather than to the
edit. That is the work of findings 191 onward.

Output kinds blinker cannot produce — `-dynamiclib`, so every proc-macro crate
— are still delegated automatically, with the reason recorded.
`--blinker-delegate` delegates everything, and `--blinker-no-daemon` or
`BLINKER_NO_DAEMON=1` links in-process. `--blinker-daemon-stop` stops a
resident linker.

See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) for the product definition,
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for the milestone sequence,
and **[FINDINGS.md](FINDINGS.md)** for the 196 places reality contradicted the
plan — several of them contradicting earlier entries in the same file.

## What is not done

- **Speed at scale.** 1.75× the system linker on a large cold link, against
  1.09× on a small one, and the resident relink of a large program is still
  slower than a cold `ld-prime` — by 10%, where it was 51% at the start of the
  work recorded in findings 179-186. The reason is that the stages left are
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
- **Bounded, not unbounded, sharing across targets.** A session now holds an
  input for four links after the last one that mentioned it, and keeps the
  per-link answers for three targets and the finished images for three. That
  fixed alternating targets going cold (finding 188 — it was a 2x penalty, and
  no benchmark here linked more than one program so nothing said so), but the
  windows are constants rather than a memory budget, and a workspace with more
  concurrent targets than that still thrashes.
- **Incremental output.** The image is rebuilt and rewritten whole. The layout
  machinery for stable addresses across edits exists, is tested, and holds
  (9,719 of 9,722 contributions keep their address on an ordinary edit) — but
  unchanged bytes are still copied and re-emitted rather than left where they
  are.

  Worth less than it sounds, and measured rather than assumed: 59% of the
  output is `__LINKEDIT` and 46% of it is symbol-name text, and one symbol
  added near the front shifts every string offset after it. The ceiling on
  never touching an unchanged byte is about 9 ms of a 342 ms link — see
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

```toml
# .cargo/config.toml
[target.aarch64-apple-darwin]
linker = "/absolute/path/to/blinker"
```

Then build as normal:

```bash
cargo build
cargo test
```

That is the whole of the setup. Blinker links internally, and the first link
starts a resident linker that the rest of the build reaches.

**To restore the default linker**, delete that `[target.aarch64-apple-darwin]`
section (or just the `linker` key), then run `blinker --blinker-daemon-stop` —
the daemon is the one piece of state a build leaves behind, and it would go on
its own twenty minutes later regardless.

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
