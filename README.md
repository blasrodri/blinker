# blinker

An incremental Mach-O linker for repeated Rust development builds on Apple
Silicon, aimed at cutting the link latency in edit–build–test loops run by
humans, IDEs, and coding agents.

**Status: it links.** blinker produces Mach-O executables from real object
files and archives, signs them itself, and the results run.

| | |
|---|---|
| C programs | work, results match the system linker |
| Rust, `-C panic=abort` | works, including panic messages and `SIGABRT` |
| Rust, default (`panic=unwind`) | links and runs until something panics |
| Speed | ~1.2× `ld-prime`, Apple's default linker |
| Output size | 2.25× larger — no dead-stripping yet |

Delegation to the system linker remains the default; pass `--blinker-internal`
to link internally.

See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) for the product definition,
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for the milestone sequence,
and **[FINDINGS.md](FINDINGS.md)** for the 41 places reality contradicted the
plan — several of them contradicting earlier entries in the same file.

## What is not done

- **Unwinding.** `panic=unwind` faults after printing the panic message. The
  `__unwind_info` table is built and matches the system linker's values; the
  fault is elsewhere and has not been located.
- **Dead-stripping.** Every archive member pulled in is emitted whole, which is
  where the 2.25× size comes from.
- **The cache.** The whole point of the project, and not started. The parse
  cache originally planned was measured and struck (finding 41): parsing is
  faster than any deserialiser. The addressable cost is `resolve` and
  `relocate`, so the cache must store relocated output keyed by codegen unit.

## Measuring it

```bash
# capture a real link's arguments with a shim linker, then:
scripts/bench.py <captured-args>            # blinker vs the system linker
scripts/bench.py <captured-args> --profile  # blinker's own stage breakdown
```

The harness interleaves both linkers, discards warmup, verifies every run
succeeded, and reports spread. Each of those exists because an earlier version
without it produced a wrong number that was believed — see the header of
`scripts/bench.py`.

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
