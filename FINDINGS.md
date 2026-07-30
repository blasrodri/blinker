# Findings

What building this actually taught us, recorded against the assumptions in
[PRODUCT_SPEC.md](PRODUCT_SPEC.md). The findings that *contradict* the spec are
the valuable output. Design choices and their evidence live in
[DECISIONS.md](DECISIONS.md).

Findings 1–7 are from M0 (the workload recorder); 8 onward are from M1 (object
parsing).

Environment these were observed on:

```
macOS 26.5.2 (25F84), Apple Silicon (arm64)
Xcode 26.6 (17F113), macOS SDK 26.5
rustc 1.97.1 (8bab26f4f 2026-07-14), LLVM 22.1.6
cargo 1.97.1 (c980f4866 2026-06-30)
```

---

## 1. rustc invokes `cc`, not `ld` — the spec's argument model was wrong

**Spec assumption.** §10 and §38 describe parsing `ld64` options and delegating
to `/usr/bin/ld`, with §30's example showing `--fallback-linker /usr/bin/ld`.

**Observed.** `rustc` does not invoke `ld` at all. It invokes the *C compiler
driver* and lets the driver call `ld64`. The `linker = …` key in
`.cargo/config.toml` names a program in the **cc driver** position. A minimal
`cargo build` for `aarch64-apple-darwin` produced 37 arguments:

```
<symbols.o> <7 × *.rcgu.o> <20 × *.rlib>
-lSystem -lc -lm
-arch arm64
-mmacosx-version-min=11.0.0
-o <output>
-Wl,-dead_strip
-nodefaultlibs
```

Note `-arch arm64` (not `-arch_multiple`/`ld64` spelling),
`-mmacosx-version-min=` (a driver flag; `ld64` spells it `-platform_version`),
and `-Wl,-dead_strip` — the only real `ld64` option present, tunnelled through
the driver's `-Wl,` mechanism.

**Consequences, applied.**

- The `arguments` crate parses the **driver** surface and splits `-Wl,a,b`
  payloads into individual `ld64` options.
- The default fallback is `/usr/bin/cc`, not `/usr/bin/ld`. Forwarding this
  vector to `ld` would fail immediately — it does not accept these spellings.
- Every blinker option uses a `--blinker-` prefix. §38 raised namespacing as a
  contingency ("if collisions with `ld64` options become possible"); with the
  driver surface the collision risk is immediate, not hypothetical.

**Open question for M2.** When blinker starts linking internally it must decide
whether to *emulate the driver* (interpreting `-arch`, `-mmacosx-version-min=`,
`-lSystem`, and driver-injected default libraries itself) or to *become* an
`ld64` replacement invoked by a driver. These are materially different scopes.
The recorded corpus should settle which arguments must be interpreted before
that decision is made.

---

## 2. Recorded invocations are not replayable without archiving inputs

**Spec assumption.** §36 M0 acceptance: "Recorded invocations can be replayed."
§41 item 11: "Supports replaying a recorded invocation." Both read as though
recording the argument vector is sufficient.

**Observed.** It is not. `rustc` writes the link's object files — `symbols.o`
and one `.rcgu.o` per codegen unit — into a temporary directory
(`target/…/deps/rustc<random>/`) that it **deletes as soon as the linker
returns**. Replaying a recorded argument vector even seconds later fails:

```
clang: error: no such file or directory: '…/deps/rustcvrSigF/symbols.o'
```

This was found by writing the replay test, not by reading the spec.

**Consequence, applied.** Recording now snapshots every input file next to the
JSON record, and stores a second `replay_argv` pointing at the archived copies.
Replay prefers `replay_argv` and redirects `-o` into a scratch directory so it
can never overwrite a real build artifact.

**Cost.** Archiving copies the full input set, including ~20 rlibs (libstd and
friends). Recording is opt-in per invocation, so this is only paid when building
a corpus — but a corpus of several projects will be hundreds of MB. If that
becomes a problem, content-addressed dedup across recordings is the obvious fix
(the rlibs are identical across every link against the same toolchain), and M4's
cache will need content addressing anyway.

---

## 3. Response files do not appear in practice — and now we know why

**Spec assumption.** §41 item 3 lists safe response-file expansion as an M0
deliverable, implying they are routinely present.

**Observed.** Not one `@file` argument appeared across 13 recorded links,
including four real third-party projects. The reason is measurable:

```
longest command line observed:  71,306 bytes  (tokei, 353 arguments, 342 inputs)
macOS ARG_MAX:               1,048,576 bytes
headroom:                              15x
```

A response file is an argument-length mitigation. rustc only reaches for one
when the command would exceed the limit, and a typical Rust link is 15x below
it. Triggering one needs a project roughly fifteen times larger than ripgrep or
tokei — plausible for a very large monorepo, not for ordinary work.

**Consequence.** Expansion stays implemented and unit-tested (nested files,
cycles, quoting, escapes) because the failure mode if we are wrong is a link
that cannot start at all. But it is now understood as a rare path rather than a
routine one, and the absence of an integration test for it is a measured
judgement rather than an untested gap. If M4's large benchmark project ever
produces one, that is the moment to add the end-to-end test.

---

## 4. Arity, not existence, is the dangerous unknown — so the option table is generated, not discovered

**How this was found.** The `-L` bug below (finding 4a) was found by building a
build-script fixture and watching classification fail. That works, but it scales
terribly: each such bug waits for a project that happens to trigger it, and the
failure mode is silent.

The realisation: knowing an option *exists* is the easy half. The half that
corrupts a link is knowing how many arguments it **consumes**. Get that wrong
and the linker's own arguments are read as input files — nothing errors, the
link is just wrong.

**What replaced the guessing.** Two authoritative sources encode arity directly:

- **Apple's `man ld`** on the host toolchain documents each option with its
  argument names (`-alias symbol_name alternate_symbol_name`), so arity is
  mechanically extractable — 209 options.
- **LLD's `lld/MachO/Options.td`** declares `Flag` / `Separate` / `Joined` /
  `MultiArg`, covering options Apple's page omits and disambiguating the
  dual-spelling ones.

Merged, that is **238 options with known arity**, now in
`crates/arguments/src/reference.rs` and driving classification.

**What this caught that fixtures never would have.** 16 options take *more than
one* argument:

```
-rename_section(4)
-platform_version(3)  -sectcreate(3)  -sectalign(3)  -segprot(3)  -sectorder(3)  -segcreate(3)
-alias(2)  -add_empty_section(2)  -rename_segment(2)  -section_order(2)  -segaddr(2)
-move_to_ro_segment(2)  -move_to_rw_segment(2)  -seg_page_size(2)  -sectobjectsymbols(2)
```

`-sectcreate __TEXT __info_plist plist.xml` would have contributed three phantom
input files. `-platform_version` is the modern replacement for
`-macosx_version_min` and takes three. None of these appear in any fixture we
would have thought to write.

**A second arity bug the table exposed.** Inside a `-Wl,` payload the same rules
apply, but values are the following *comma elements*:
`-Wl,-exported_symbol,_main` is one option consuming one value, not two flags.
The original code split blindly on commas.

**The corpus's real job, corrected.** After all nine fixture shapes, the corpus
exercises **3 of 238** options (`-arch`, `-dead_strip`, `-framework`). Iterative
discovery would have modelled 3. The table models 238, so an option appearing
for the first time in someone else's project is already parsed correctly. The
corpus now answers *which options matter in practice*, not *which exist* —
which is what it is actually good for.

Regenerate after a toolchain update with `scripts/extract-ld-options.sh`.

### 4a. `-L` arrives in two spellings

`man ld` documents only the attached form (`-Ldir`), so the separate form is not
derivable from it — LLD's table is the source that declares both. rustc emits
both: a build script's `cargo:rustc-link-search=` arrives as `-L` followed by
the path as a separate argument, everything else arrives attached. Handling only
the attached form silently drops the search path, and the native library the
build script just compiled fails to resolve. Same applies to `-F` and `-l`.

---

## 5. The argument inventory holds on real third-party projects

The nine built-in fixtures are synthetic — they contain the shapes we thought to
construct. Four real projects were recorded as the check on that:

| Project | Inputs | argv | Command line | Link | Frameworks |
|---|---:|---:|---:|---:|---|
| tokei | 342 | 353 | 71 KB | 135 ms | |
| fd | 327 | 343 | 67 KB | 142 ms | Foundation |
| hyperfine | 319 | 332 | 69 KB | 80 ms | Security |
| ripgrep | 310 | 321 | 65 KB | 114 ms | |

**Zero unmodelled arguments.** Real projects added `-liconv`, `-lobjc`, and
`-framework Foundation` / `-framework Security` — all already handled.

Across the whole corpus of 13 links: 1,669 inputs, 672 MB read, median link
81 ms.

**Scale gap worth noting.** Real projects link ~320 inputs; the synthetic
fixtures link 27–82. Anything sensitive to input count — the M4 cache, the M5
graph — should be exercised against the real projects rather than the fixtures,
which are an order of magnitude too small to be representative.

An integration test asserts the inventory stays clean and is designed to fail
loudly as the corpus grows:

```
rustc emitted arguments blinker does not model: [...]
Add them to the `arguments` crate — check `reference.rs` for the option's
arity before assuming it takes no value.
```

---

## 6. Anything the linker prints makes rustc emit a warning

**Spec assumption.** §7 shows a concise human-readable summary printed on every
link ("mode: incremental / elapsed: 82 ms / …"), with §31's `--print-stats`
controlling it.

**Observed.** rustc has a `linker_messages` lint, on by default. Any linker
output on stdout or stderr surfaces as a build warning:

```
warning: linker stderr output
  = note: `#[warn(linker_messages)]` on by default
warning: `probe` (bin "probe") generated 1 warning
```

So a summary printed on every successful link would add a warning to every
successful build — exactly the noise §7 says to avoid ("normal successful links
should not generate excessive output").

**Consequence.** The machine-readable record is the primary interface, not the
terminal. `--blinker-json-diagnostics` and `--blinker-record-invocation` are
side channels that produce no build warnings. `--blinker-print-stats` remains
available but is opt-in and inherently noisy under cargo.

**Open question.** A per-link human-readable summary that does *not* warn needs
a channel other than the linker's stdio — a file the user tails, or the eventual
daemon (§28). Worth settling before M4, since that is the milestone whose
results a developer will actually want to watch.

---

## 7. Whole-build wall time cannot measure linker overhead

**What was tried.** Time a full clean `cargo build` with the system linker and
with blinker, and compare. Median of repeated runs, per fixture:

```
FIXTURE         SYSTEM (ms) BLINKER (ms)     OVERHEAD
minimal                 543          476       -12.3%
multimod                280          317        13.3%
workspace               421          382         -9.1%
buildscript             569          534         -6.1%
cdep                    737          745          1.0%
procmacro               589          624          6.1%
testharness             219          241          9.8%
generics                264          218        -17.3%
deps                   4004         3619         -9.6%
```

**Why it says nothing.** The spread runs from −17% to +13%, including
impossible negatives — blinker cannot make a link faster while delegating to the
same linker. Compile time is an order of magnitude larger than link time, and
its variance alone exceeds the entire linker step. The instrument cannot resolve
the quantity.

**The instrument that works.** blinker's own recorded timings isolate the link
step. Across the corpus:

```
Median link time:  67 ms   (delegated to the system linker)
blinker overhead:   8.9 ms per link (13.2% of the link step)
  argument parsing  0.12 ms
  fingerprinting    0.30 ms
  remainder         ~8.5 ms — input archiving (27 files, 17 MB copied)
```

Archiving only happens when `--blinker-record-invocation` is set. Steady-state
overhead without recording is under a millisecond.

**Consequence for M4.** Benchmarking must measure the link step, not the build.
The implementation plan's scenario suite should be built on blinker's recorded
per-link timings, with whole-build wall time reported only as context. A
plan that compares `cargo build` durations will not be able to detect the effect
it is trying to measure.

---

## Remaining M0 work

None blocking. The milestone's deliverables are met:

- Cargo-compatible delegating linker, with argument parsing and normalization.
- Invocation recording with archived inputs, and working replay.
- JSON metrics on every link.
- Fixture corpus: nine synthetic shapes plus four real third-party projects.
- Baseline timing report (finding 7), with the caveat that whole-build wall
  time is the wrong instrument.
- Argument inventory: clean across all 13 links, with 238 ld64 options modelled
  by arity ahead of ever being seen.

Carried into M1 rather than left open:

1. **The driver-vs-ld64 scope question** (finding 1) — whether blinker emulates
   the compiler driver or replaces `ld64` under one. The corpus is now large
   enough to settle it.
2. **Response files** (finding 3) — rare, measured at 15x headroom, unit-tested
   only. Revisit if M4's large benchmark project produces one.

## Decisions taken during M0

| Decision | Rationale |
|---|---|
| Hand-rolled option parsing, no `clap` | Options must be scanned out of a foreign, driver-shaped argv and stripped in place; `clap` models a vector it owns. Also keeps the dependency set at 3. |
| `--blinker-` prefix for all project options | Collision with driver/`ld64` flags is immediate, not hypothetical (finding 1). |
| Fallback defaults to `cc`, then `clang` | Matches what rustc would have invoked (finding 1). |
| Explicit missing fallback path is an error | Silently substituting a different linker would change link semantics without saying so. |
| Logic in `blinker_cli` lib, thin `main.rs` | Lets the driver be tested directly rather than only through process spawning. |
| JSON key set is fixed; unpopulated fields are `null` | Consumers index these directly; a stable key set means no defensive guards, and a test asserts the contract. |
| Fingerprint fast path only (no hashing by default) | Spec §13's verification-path *policy* belongs to M4, where reuse decisions actually depend on it. The recorded shape already carries the fields M4 will need. |

---

## 8. The robustness harness found a real bug within minutes of existing

**Context.** Spec §14 requires malformed input to produce a structured error
rather than a panic. The obvious reading is "add bounds checks", and wrapping
`object` (D1) appeared to satisfy that for free — its parser is bounds-checked
and fuzzed upstream.

**What the mutation tests found.** Not an out-of-bounds read. A *successful
parse that produced an unusable structure*: a corrupted symbol whose section
reference named a section that did not exist.

Mach-O numbers sections from one, so the parser subtracts one to get an index.
A corrupted `n_sect` naming section 200 in a 14-section object subtracts
cleanly to 199 and was stored as a `SectionId(199)`. Nothing overflowed and
nothing read out of bounds — `object`'s checks were never going to catch it,
because the value is structurally valid and only semantically wrong.

Downstream, every ID is treated as an index. A dangling one is a latent panic
sitting inside a `ParsedObject` that claims to be fine.

**The general lesson.** Inheriting a bounds-checked parser buys memory safety,
not *semantic* validity. The invariants that matter here — every ID resolves —
belong to our representation, so they must be enforced at our conversion
boundary. Every index now passes through a range check against the table it
refers into, and the fuzz target asserts the same invariant.

**On the harness itself.** This was found by a deterministic mutation test that
runs on stable in the normal gate, not by a fuzzing session. That split is
deliberate: a regression here should fail in the gate rather than only in a
session nobody has run recently. `fuzz/` exists for depth, and drives the same
entry point with the same invariant assertion.

---

## 9. `lib.rmeta` is a real Mach-O object, so skipping it must be name-based

**Spec assumption.** §15 asks for "deliberate skipping of Rust metadata members
that are not linker inputs" — which reads as tidiness, an optimisation to avoid
handing a non-object to an object parser.

**Observed.** It is not tidiness. It is required for correctness.

`lib.rmeta` inside a Rust `.rlib` is a **genuine Mach-O 64-bit arm64 object**:

```
$ file lib.rmeta
lib.rmeta: Mach-O 64-bit object arm64

$ otool -l lib.rmeta | grep -A1 sectname
  sectname .rmeta
   segname __DWARF
```

rustc wraps crate metadata in an object container so `ar` and linkers handle it
like any other member. It parses cleanly. Worse, its single section is
`__DWARF,.rmeta`, which our own classifier files as `SectionKind::Debug` — so a
content-based filter would not merely accept it, it would accept it as
*plausible debug data* and link the entire metadata blob into the output.

The toolchain also ships a second such member, `lib.rmeta-link`, which older
descriptions of the rlib format do not mention.

**Consequence.** Member classification is by **name**, and the test that pins
this asserts the surprising half explicitly: metadata *does* parse as Mach-O,
carries no code, and must be excluded anyway. A future refactor that "improves"
classification by sniffing content would silently start linking metadata.

**Related.** The archive symbol table (`__.SYMDEF`) is listed by `ar t` as a
member but is not a member in the structural sense — it is an index. The reader
surfaces it as such, so member lists must be compared against `ar` minus that
entry.

---

## 10. A `.tbd` is a multi-document file, and reading one document finds 3 of 9,264 symbols

**Spec assumption.** §21 describes `.tbd` handling as extracting "install names,
exports, re-exports, architecture constraints, and platform constraints" — a
list of fields, implying one stub describes one library.

**Observed.** `libSystem.B.tbd` is 3,760 lines containing **40 YAML documents**:
libSystem itself, then every library it re-exports, inline in the same file.
Measured against the real SDK stub:

```
documents in the file:              40
symbols in the primary document:     3
symbols after following re-exports: 9,264
```

libSystem's own `exports` block lists three symbols
(`___crashreporter_info__`, `_libSystem_init_after_boot_tasks_4launchd`,
`_mach_init_routine`). `_malloc` lives in the re-exported
`/usr/lib/system/libsystem_malloc.dylib` document at line 2,822 of the same
file.

A reader that took the first document and stopped would report that libSystem
exports three symbols, and every link would fail with thousands of undefined
symbols.

**Consequence.** Resolution walks `reexported-libraries` breadth-first,
resolving each install name against the other documents in the same file. The
walk guards against cycles — the graph is declared data and nothing guarantees
it is acyclic — and skips names that live in other files, which is the library
resolver's job in M3.

---

## 11. Architecture matching cannot be exact, or every link fails

**Spec assumption.** §21 lists "architecture filtering" among the `.tbd`
resolver's jobs, which reads as: match the target, discard the rest.

**Observed.** `libSystem.B.tbd` declares:

```yaml
targets: [ x86_64-macos, x86_64-maccatalyst, arm64e-macos, arm64e-maccatalyst ]
```

**There is no `arm64-macos`.** Our target is `aarch64-apple-darwin` — plain
arm64. Exact filtering discards libSystem entirely.

Yet arm64 binaries link against it constantly. Confirmed empirically rather
than assumed:

```
$ cc -arch arm64 -o t t.c && lipo -archs t
arm64
$ otool -L t
	/usr/lib/libSystem.B.dylib
```

`arm64e` is arm64 plus pointer authentication; the two share a symbol set, and
the toolchain accepts an arm64e stub for an arm64 link.

**Consequence.** Architecture matching treats arm64 and arm64e as compatible,
and **nothing else** — the rule is deliberately narrow, since widening it would
begin accepting stubs that genuinely do not match. Platform still matches
exactly, so `maccatalyst` never satisfies a `macos` link even at the same
architecture. Both halves are pinned by tests, including one asserting that
libSystem's real declared target list yields exactly `arm64e-macos` for an
arm64 macOS link.

Of the 330 stubs in the SDK, 330 mention `arm64e-macos` and only 32 mention
`arm64-macos` — so this is the common case, not an edge case.
