<img src="assets/blinker-banner.svg" width="620" alt="blinker — a resident, incremental Mach-O linker for Apple Silicon">

A resident, incremental Mach-O linker for Rust on Apple Silicon.

**It makes a `cargo build`'s linking 3.7× faster, out of the box.**

From the project you want to link faster:

```bash
curl -fsSL https://raw.githubusercontent.com/blasrodri/blinker/master/install.sh | sh -s -- --use
```

That is the whole of it. `cargo build` now links with blinker.

```
a full build of this workspace — 16 links, 23 inputs at the median

  ld64 (cc)            794, 839, 840, 841 ms
  blinker              213, 215, 216, 217 ms      3.7×
```

Every one of those 16 links is blinker's, including the proc-macro dylib that
used to be handed to `ld`.

Set it as your linker and change nothing else. It links internally by default
and keeps a resident linker alive for the next link.

---

## Quick start

You need an Apple Silicon Mac, a stable Rust toolchain, and Xcode command line
tools (`/usr/bin/cc` must exist). `aarch64-apple-darwin` is the only target.

```bash
curl -fsSL https://raw.githubusercontent.com/blasrodri/blinker/master/install.sh | sh -s -- --use
```

That one line downloads the latest release, checks its SHA-256, puts `blinker` in
`~/.cargo/bin`, and writes the linker setting into this project's
`.cargo/config.toml`. Set `BLINKER_INSTALL_DIR` to put the binary elsewhere.
`blinker --blinker-uninstall` puts the project back exactly as it was.

Drop the `--use` and it installs the binary and configures nothing, which is
the right order if you would rather try it first — see step 1 below.

Or build it yourself:

```bash
git clone https://github.com/blasrodri/blinker && cd blinker
cargo build --release                    # the binary lands at target/release/blinker
```

**1. Try it on your project, changing nothing.** From the project's directory:

```bash
/path/to/blinker/target/release/blinker --blinker-try build
```

This builds into `target/blinker-try/` with blinker as the linker. It writes no
configuration, and it does not disturb your normal `target/`, so the next
ordinary `cargo build` rebuilds nothing.

**2. Keep it.** From the same directory:

```bash
/path/to/blinker/target/release/blinker --blinker-install
```

That writes the absolute path of the binary that ran into
`.cargo/config.toml`:

```toml
[target.aarch64-apple-darwin]
linker = "/path/to/blinker/target/release/blinker"
```

An existing config is *edited*, not replaced — comments, ordering and every
other setting survive. A `linker` already pointing at something that is not
blinker is reported rather than overwritten. Running it twice does nothing the
second time.

Now build as normal:

```bash
cargo build
cargo test
```

**3. Undo it.**

```bash
blinker --blinker-uninstall     # removes the key, and the file if it held nothing else
blinker --blinker-daemon-stop   # stops the resident linker
```

(`blinker` below is that same binary — copy it somewhere on your `PATH`, or keep
spelling out the path to it.)

The resident linker is the only state a build leaves behind, and it exits on its
own after twenty minutes idle regardless.

## Everyday commands

| I want to… | Command |
|---|---|
| Try blinker without configuring anything | `blinker --blinker-try build` (or `test`, `run`, …) |
| Turn it on for this project | `blinker --blinker-install` |
| Turn it off again | `blinker --blinker-uninstall` |
| See what a link cost | add `-C link-arg=--blinker-print-stats` to `RUSTFLAGS` |
| Rule blinker out as the cause of a bug | `BLINKER_NO_DAEMON=1`, or `--blinker-delegate` to hand every link to the system linker |
| Stop the resident linker | `blinker --blinker-daemon-stop` |
| Check the version | `blinker --blinker-version` |

## What works

- **Executables and test binaries**, `panic=abort` and `panic=unwind`, caught
  panics, destructors, symbolized backtraces, and a `dsymutil`-readable debug
  map.
- **Dynamic libraries** — proc-macro crates and `cdylib`s. rustc `dlopen`s a
  proc-macro dylib inside its own process to expand macros, and does so with
  the ones blinker links.
- **Dead stripping** (`-dead_strip`), from the entry point for a program and
  from every exported symbol for a library.
- **Anything else** — `-bundle`, `-r`, `-static`, `-shared`, and inputs that
  are LLVM bitcode rather than Mach-O (what `-flto` produces) — is handed to
  the system linker automatically, with the reason recorded, so a workspace
  that contains one never fails to build.

Blinker's output is compared byte-for-byte against a cold link of the same
inputs by the test suite: a warm, resident, incremental link must produce
exactly the file a from-scratch link would.

---

## Benchmarks

Every number here is from an interleaved A/B against the system linker on real
captured link arguments, not a synthetic benchmark. See
[measuring it](#measuring-it) to reproduce any of them.

### The headline

| | blinker | system linker | |
|---|---|---|---|
| **A whole `cargo build`'s links** (16 links) | **213 ms** | 794 ms `cc`/ld64 | **3.7×** |
| **Edit relink, resident** | **15.3 ms** | 31.4 ms `ld64` | **2.1×** |

The relink row holds 68 of 70 inputs in memory, replays the archive extraction
order, holds the resolved imports, and reuses 235 of 236 objects and 99% of
relocations. Its two halves come from two harnesses — `relink.py` for blinker's
resident number and `bench.py` for the system linker, which has no resident mode
to measure — so read it as "same link, same machine, same minute" rather than as
one interleaved A/B.

### By workload

| Workload | Result |
|---|---|
| Body edit, debug self-link (1,099 objects) | 41.9 ms wall, 31.9 ms linking |
| Cold link, 238 objects | 1.03× `ld-prime` — inside the spread |
| Output size, large link with dead-stripping | 0.85× `ld-prime`'s |
| **Cold link, 5,637 objects** | **1.70× slower** |
| Edit relink, 5,637 objects | level — ~390 ms against 367 ms |
| **Five C++ programs, 552 inputs each** | **~2× slower** cold; residency is free rather than costly |

The last three rows are the honest cost of the design. A resident linker's first
link pays for state no link has reused yet, and on a very large program that
bill arrives all at once. blinker wins on repetition and on the common case; it
does not yet win on a single cold link of a huge program.

### Why the build's number, not a link's

A linker is not slow. A *build* is slow, because it links the same programs over
and over with almost nothing changed between them — and a linker that exits
after every link has to be told the whole program again each time.

So the thing worth attacking is not the cost of one link. It is the cost of the
hundredth link of a program the linker has already seen ninety-nine times. That
is why blinker is resident: staying alive is worth **1.46×** on its own here
(213 ms against 312 for the same links one-shot), before any incremental
machinery does anything.

That number was zero until finding 214. A session forgot an input after four
links, a workspace with five or more targets therefore got no reuse at all, and
the harness meant to catch it was routing all sixteen links to one worker of
four. Both are fixed, and the eight-program rotation test is there so the window
cannot quietly become a cliff again.

It is also why the median matters more than the maximum. A real build's links
have 23 inputs at the median and 132 at the largest. Optimising the tail is
optimising the case that happens once — and measuring only the tail is how a
change worth 25% of the median link measures as 1% (finding 210).

### Concurrency

A cold build of this workspace submits **eleven links within 129 ms of each
other**. Blinker serves links from a pool of four worker processes routed by
output path, which took the total client wait from 705.8 ms to 517.9 ms (−27%)
and the median wait per link from 44.6 ms to 28.5 ms (−36%) on this workspace.

The incremental build does not hit the queue at all — after touching one crate
this workspace issues exactly one link — so concurrency buys the cold and
near-cold build and buys the edit loop nothing. `BLINKER_TRACE_WAIT=<file>`
writes a trace of every link's client-side wait; `scripts/link-burst.py` reports
the burst in it.

### Where the time goes on a cold link

The first link of a program, which is what a newcomer's first build is made of.
This workspace's own binary — 70 inputs, 724 objects, 2.9 MB out:

```
  read+parse      8.0 ms      objects 1.3, archive members ~6, stubs 3.2 alongside
  relocate        2.9 ms
  dead-strip      1.8 ms
  emit+sign       1.3 ms
  layout          0.8 ms
  total          17.8 ms      was 26.4 before findings 209, 210 and 213
```

Reading the 70 files on the command line costs 1.3 ms of it. Nearly everything
else in that stage is pulling 654 members out of rlibs and parsing them, and
that is where the next cold work is.

`BLINKER_GAP_PARTS=0.05` prints every part of a link that took longer than the
threshold you give it, which is how that table was produced.

### Where the time goes on a large relink

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

Every one is proportional to the whole program rather than to the edit. The
largest single item found so far was not a stage at all but a 1.7-million-element
clone sitting in the gap *between* two measured stages (finding 199).

### A note on the numbers

They move a lot. blinker was measured at 0.92× on a 47-object fixture and turned
out to be **7.44×** on a real binary, because a linear scan that was invisible at
small scale was quadratic at large (finding 77). Treat any single-fixture
number here, including these, as a claim about one workload.

They also go stale. This file said 2.92× and "the daemon is not implemented" for
long enough that a review of the project reasoned from it and recommended work
that had already been done. If a number here disagrees with
[FINDINGS.md](FINDINGS.md), the finding with the higher number was measured
last.

---

## What is not done

- **Speed at scale.** 1.70× the system linker on a large cold link, against
  1.03× on a small one, and the resident relink of a large program is level
  with a cold `ld-prime` — where it was 51% slower at the start of the work
  recorded in findings 179-186. The stages that are left are proportional to
  the whole program rather than to the edit: dead stripping rebuilds the
  reachability graph even when one object in 5,637 moved, and the image is
  assembled, hashed and written whole even when 98% of relocations were reused.

  Not the reason, though this file said so for a long time: materialising every
  symbol name into an owned `String`. Removing that allocation was worth about
  4 ms of a 780 ms link, because the parse has been on every core since finding
  161 and 976,000 allocations spread over fifteen cores are not 976,000
  allocations (finding 168).
- **Incremental output.** The image is rebuilt and rewritten whole. The layout
  machinery for stable addresses across edits exists, is tested, and holds
  (9,719 of 9,722 contributions keep their address on an ordinary edit) — but
  unchanged bytes are still copied and re-emitted rather than left where they
  are.

  Worth less than it sounds, and measured rather than assumed: 59% of the output
  is `__LINKEDIT` and 46% of that is symbol-name text, and one symbol added near
  the front shifts every string offset after it. The ceiling on never touching
  an unchanged byte is about 9 ms of a large link — see finding 187, and finding
  186 for the version of it that measured *slower* than writing the file whole.
- **Memory.** Per-target state is bounded in bytes and evicted
  least-recently-used (`BLINKER_MEMORY_BUDGET`, default 1024 MB): 291 MB held on
  an ordinary large relink. That is 400 MB of a 3.0 GB process, though — the
  rest is parsed inputs, still bounded by a window of four links rather than by
  bytes, and about a third is memory the allocator has freed and not returned
  (finding 201).
- **`x86_64`, universal binaries, LTO.**

---

## Blinker Live — an experiment, not a product

Linking is not the only thing a build repeats. Cargo rebuilds every crate
downstream of an edit because an rlib changed, and for a change to one function
body that work is almost entirely unnecessary. **Blinker Live** asks what it
costs to skip it: validate the edit with a real compiler, prove it is safe to
replace in place, generate only the code that actually changed, and publish it
into a running process.

It is in [`experiments/`](experiments/), it is not installed by anything above,
and it is not on the path of any `cargo build`. It uses a pinned nightly and
`rustc_private`; a failure to build it cannot affect the linker. It does share
one thing with the linker — `blinker-macho`, whose symbol and relocation model
turned out to be exactly what lifting a function out of a compiler's output
needs.

**Measured, on real crates** (`grep_matcher` from ripgrep, and this
workspace's own `blinker_diagnostics`):

| | Cargo debug rebuild | Blinker Live | |
|---|---:|---:|---:|
| edit `grep_matcher` | 762 ms | **23 ms** | **33×** |
| edit `blinker_diagnostics` | 434 ms | **29 ms** | **15×** |

Of that 29 ms, everything Blinker Live does is **0.5 ms**: the changed Rust
closure compiles through Cranelift in 0.13–0.27 ms and the new machine code
becomes callable in about 2 µs. The rest is rustc validating the edit, which is
work a correct system has to do.

The interesting result is not the speed. It is that **Cargo's downstream
rebuild graph is not the execution dependency graph** for a body-only edit —
and that this can be decided rather than hoped:

- a classifier that must *prove* an edit is safe, refusing anything it cannot
  (14 adversarial cases; a compile-time oracle rebuilds 50 and 32 downstream
  crates and checks their code is byte-identical);
- the exact set of functions an edit changed, discovered from the edited
  function outward rather than by compiling the crate;
- a runtime differential that runs the patched program against an
  independently rebuilt one — ordinary rustc, LLVM, a real linker, the system
  dynamic loader — across a mutation suite, with three negative controls that
  break it deliberately to show it can fail;
- immutable generations with one atomic commit, checked under concurrency and
  rollback against those same clean rebuilds;
- 300 revisions in one resident process with flat latency and bounded memory.

What it does not do: trait methods, generics, `const fn`, `async fn`, or any
edit whose generated code needs constant data — string literals, panics, vtables
— are all refused rather than attempted. Nothing is reclaimed from the code
arena. It replaces code, not state.

The record is [`experiments/live-frontend-spike/RESULTS.md`](experiments/live-frontend-spike/RESULTS.md),
which is written the same way as FINDINGS: every number with the harness that
produced it, and every wrong turn that produced a convincing number first.

---

## Reference

### Options

blinker occupies the position `rustc` invokes as the C compiler driver, so its
argument vector is full of driver flags (`-o`, `-L`, `-l`, `-arch`, `-Wl,…`).
Every blinker option therefore carries a `--blinker-` prefix that cannot collide
with a driver or `ld64` flag, and is stripped before the remaining arguments are
forwarded.

| Option | Meaning |
|---|---|
| `--blinker-try [CARGO ARGS]` | Build through blinker with no configuration, into `target/blinker-try` |
| `--blinker-install` | Set this binary as the linker for the project in the current directory |
| `--blinker-uninstall` | Undo that, leaving the project as it was found |
| `--blinker-daemon-stop` | Stop the resident linker, if any |
| `--blinker-no-daemon` | Link in this process, and start no daemon |
| `--blinker-delegate` | Hand every link to the system linker |
| `--blinker-cache` | Replay an unchanged image from a previous link |
| `--blinker-print-stats` | Print the human-readable summary |
| `--blinker-json-diagnostics <PATH>` | Write the machine-readable record to `PATH` |
| `--blinker-diagnostics <LEVEL>` | `quiet` \| `normal` \| `verbose` |
| `--blinker-fallback-linker <PATH>` | Linker to delegate to (default: discovered) |
| `--blinker-record-invocation <DIR>` | Record this invocation, with inputs, into `DIR` |
| `--blinker-replay-invocation <FILE>` | Replay a recorded invocation |
| `--blinker-strict-fingerprints` | BLAKE3-hash every input rather than trusting metadata |
| `--blinker-version`, `--blinker-help` | Version / help |

Options that have to travel through `rustc` to reach the linker are passed as
`-C link-arg=--blinker-…`. Use the inline `=` form so `rustc` cannot separate an
option from its value. `--blinker-try`, `--blinker-install`, `--blinker-uninstall`
and `--blinker-daemon-stop` are run directly, not through `rustc`.

### Environment

| Variable | Meaning |
|---|---|
| `BLINKER_NO_DAEMON=1` | Link in-process, start no daemon |
| `BLINKER_MEMORY_BUDGET` | Per-target state bound, in MB (default 1024, divided among the four workers) |
| `BLINKER_FALLBACK_LINKER` | Linker to delegate to |
| `BLINKER_TRACE_WAIT=<file>` | Append one line per link with its client-side wait |
| `BLINKER_GAP_PARTS=<ms>` | Print every part of a link slower than this threshold |
| `BLINKER_DELTA_LIVENESS`, `BLINKER_RETAIN_STRINGS` | Built, verified, off — each worth a few ms and each with a recorded reason in FINDINGS |

### Fallback linker discovery

Highest precedence first:

1. `--blinker-fallback-linker <PATH>`
2. the `BLINKER_FALLBACK_LINKER` environment variable
3. `/usr/bin/cc`, then `/usr/bin/clang`

An explicitly configured path that does not exist is an error rather than a
silent fall-through to a default — quietly substituting a different linker would
change link semantics without saying so.

The default is `cc` rather than `ld` because that is what `rustc` itself
invokes; see [FINDINGS.md](FINDINGS.md).

---

## Measuring it

Build a workload first. It is rebuilt from the repository rather than found
lying around, because every workload this project measured before finding 93 was
archived into a temporary directory and is gone:

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

For a whole build rather than one link:

```bash
BLINKER_TRACE_WAIT=/tmp/trace cargo build && scripts/link-burst.py /tmp/trace
```

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

### Recording a corpus

Recording captures real linker invocations from real projects, which is how the
arity table in `blinker-arguments` and most of FINDINGS were derived:

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

---

## Development

```bash
./scripts/check.sh          # full gate: fmt, clippy, unit + end-to-end tests
./scripts/check.sh --fast   # skip the slow real-cargo-build tests
```

The same gate runs in CI on an Apple Silicon runner
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) — the same script, not
a weaker second definition of "passing". It runs locally too, as a pre-commit
hook (`scripts/install-hooks.sh`), because the fast feedback is the point.

Pushing a `v*` tag builds a release, runs the gate against that exact commit,
and publishes `blinker-aarch64-apple-darwin.tar.gz` with its checksum — which is
what `install.sh` downloads.

### Layout

| Crate | Role |
|---|---|
| `cli` | Entry point, driver, daemon, setup commands, record/replay |
| `arguments` | Argument classification and response-file expansion |
| `macho` | Object parsing |
| `link` | Resolution, dead stripping, relocation, the session |
| `layout` | Output addresses, and keeping them stable across edits |
| `output` | Mach-O emission, `__LINKEDIT`, export trie, code signing |
| `relocations` | Relocation kinds and how each is applied |
| `symbols` | Symbol resolution structures |
| `cache` | The on-disk incremental cache |
| `tbd` | `.tbd` stub libraries — what the system dylibs export |
| `archive` | `.a` and `.rlib` member extraction |
| `hashing` | The fast hashers the hot maps use |
| `diagnostics` | JSON record schema, timings, input fingerprints |
| `differential` | Output compared against `ld64`'s |
| `corpus` | Gathering and reporting real link invocations |
| `test-support` | Fixture generation and the end-to-end harness |

### Documents

- [PRODUCT_SPEC.md](PRODUCT_SPEC.md) — what the product is
- [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) — the milestone sequence
- **[FINDINGS.md](FINDINGS.md)** — the 241 places reality contradicted the plan,
  several of them contradicting earlier entries in the same file
- [experiments/live-frontend-spike/RESULTS.md](experiments/live-frontend-spike/RESULTS.md)
  — Blinker Live, measured
- [experiments/live-sink/](experiments/live-sink/) — the 356-line cg_clif patch
  that lets a backend hand out machine code instead of an object file
