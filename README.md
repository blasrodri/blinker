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
| Dylibs, bundles, partial links | delegated to the system linker, with a recorded reason |
| Output size | **0.73–0.79×** `ld-prime`'s, with dead-stripping |
| Speed, small link (47 objects) | **0.92×** `ld-prime` |
| Speed, large link (921 objects) | **2.92×** `ld-prime` |
| Unchanged relink | 10.4 ms — the whole image comes from cache |
| One-line edit | 16.1 ms, reusing 24 of 26 objects and 100% of relocations |

Delegation to the system linker remains the default; pass `--blinker-internal`
to link internally.

See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) for the product definition,
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for the milestone sequence,
and **[FINDINGS.md](FINDINGS.md)** for the 78 places reality contradicted the
plan — several of them contradicting earlier entries in the same file.

## What is not done

- **Debug information.** No `N_OSO` debug-map stabs and no local symbols in the
  output, so breakpoints by function name and source-line display do not work.
  Panic backtraces do, and match the system linker frame for frame. This is the
  largest missing feature.
- **Dynamic library output.** Proc-macro crates and `cdylib`s are delegated
  rather than linked. Correct, but it means a workspace is only partly linked
  by blinker.
- **Speed at scale.** 2.92× the system linker on a large link, against 0.92× on
  a small one. `read+parse` is the largest remaining stage, and the reason is
  structural: every symbol and relocation is materialised into owned `String`s,
  a representation chosen for a parse cache that was later measured and
  abandoned (finding 41).
- **The daemon, dirty-range output rewriting, stable addresses across edits.**
  The layout machinery for the last of these exists and is tested, but nothing
  calls it.
- **`x86_64`, universal binaries, LTO.**

## A note on the numbers

Every performance figure above is from an interleaved A/B against the system
linker on real captured link arguments, not a synthetic benchmark. They moved a
lot: blinker was measured at 0.92× on a 47-object fixture and turned out to be
**7.44×** on a real binary, because a linear scan that was invisible at small
scale was quadratic at large. See finding 77 — and treat any single-fixture
number here, including these, as a claim about one workload.

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

**To restore the default linker**, delete that `[target.aarch64-apple-darwin]`
section (or just the `linker` key). Nothing else is left behind — blinker keeps
no global state, and without `--blinker-internal` every link is performed by
the system linker anyway.

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
