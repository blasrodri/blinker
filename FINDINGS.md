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

---

## 12. 64-bit load commands must be 8-byte aligned, and only some tools say so

**How it surfaced.** The first assembled image passed `otool -h` and `otool -l`
cleanly — the header was right, and `otool` walked every load command without
complaint. `nm` on the same file:

```
truncated or malformed object (load command 7 cmdsize not a multiple of 8)
```

Load command 7 was `LC_LOAD_DYLINKER`. I had emitted `cmdsize` 28: twelve fixed
bytes plus `/usr/lib/dyld` (13 characters) plus a NUL, rounded up to a 4-byte
boundary. **64-bit Mach-O requires 8.**

The real toolchain agrees, and the evidence was already in the capture taken
before any of this was written — its `LC_LOAD_DYLINKER` is `cmdsize 32`, not 28.
I had read that number and not noticed what it implied.

**Why one tool caught it and the other did not.** `otool` walks the command
list permissively; `nm` validates alignment before reading. Both are Apple
tools, on the same file, disagreeing about whether it is well formed. Checking
against a single tool would have shipped this, and the eventual symptom would
have been dyld refusing the binary with no useful message.

**Consequence.** Path-carrying command sizes go through one function that
applies the 8-byte rule, used by both the emitter and the size predictor so the
two cannot drift apart. A test asserts the alignment across a range of path
lengths, and the `LC_LOAD_DYLINKER` test now pins 32 explicitly with a note
about why 28 is wrong.

**The general lesson**, and the reason the differential suite is worth the
effort: *our* assertions only check the format we believe in. Validation has to
come from the programs that were right first — and from more than one of them,
because they do not agree on what to check.

### Status

blinker now emits an image `file(1)` reports as `Mach-O 64-bit executable
arm64`, whose segments, sections, symbols and linked library `otool` and `nm`
read back correctly:

```
$ file /tmp/blinker-out
/tmp/blinker-out: Mach-O 64-bit executable arm64

$ nm -a /tmp/blinker-out
                 U _exit
0000000100004000 T _main
```

It is structurally valid, not yet runnable — the export trie, lazy binding and
synthesised stubs are still missing, and nothing has been through dyld.

## 13. A differential harness calibrated against the wrong target manufactures bugs

The differential suite's first run reported that blinker emitted the wrong dyld
metadata: it produced `LC_DYLD_INFO_ONLY` where the reference had
`LC_DYLD_CHAINED_FIXUPS` and `LC_DYLD_EXPORTS_TRIE`. That looked like a
straightforward, well-evidenced defect — a whole subsystem aimed at the wrong
mechanism.

blinker was right. The reference was wrong.

The macOS **deployment target** selects the strategy:

| deployment target | dyld metadata |
|---|---|
| ≤ 11.x | `LC_DYLD_INFO_ONLY` — classic rebase/bind opcode streams |
| ≥ 12.0 | `LC_DYLD_CHAINED_FIXUPS` + `LC_DYLD_EXPORTS_TRIE` |

Measured directly:

```
$ cc -arch arm64 -o out c.c                          # default: macos 26.0
  -> LC_DYLD_CHAINED_FIXUPS LC_DYLD_EXPORTS_TRIE
$ cc -arch arm64 -mmacosx-version-min=11.0 -o out c.c
  -> LC_DYLD_INFO_ONLY
$ rustc --print deployment-target
MACOSX_DEPLOYMENT_TARGET=11.0
```

`cc` defaults to the running OS version. `rustc` defaults to 11.0 — and
blinker links rustc's output. A real Rust binary confirms it:

```
$ otool -l target/debug/rstest | grep LC_DYLD
      cmd LC_DYLD_INFO_ONLY
```

So the reference link has to be pinned to *rustc's* target, not the driver's
default. It now is, and the pin is checked against `rustc --print
deployment-target` rather than hardcoded, so the two cannot drift apart.

### Why this is the important finding

A differential harness is trusted more than a hand-written assertion, because
it compares against something that was right first. That trust is exactly what
makes a miscalibrated one dangerous: it does not merely fail to catch bugs, it
**manufactures** them, and the manufactured ones carry the same authority as
the real ones. Acting on this report would have meant rewriting a correct
`dyld_info.rs` to target a mechanism blinker will never encounter.

The general rule this produces: **a differential test's calibration needs its
own tests.** Three now exist — the same program linked twice must compare
equal (nothing that legitimately varies leaked into the comparison), two
different programs must compare unequal (the comparison is not vacuous), and
the reference's deployment target must equal rustc's (it is measuring the
right thing). The first two were written from the start. The third was written
only after the miscalibration was caught, which is the honest order.

## 14. `MH_HAS_TLV_DESCRIPTORS` is conditional, and blinker hardcodes it

With the harness calibrated, one real discrepancy remains in the header:

```
Rust binary : 0x00a00085
C at 11.0   : 0x00200085
difference  : 0x00800000  = MH_HAS_TLV_DESCRIPTORS
```

blinker's header constant was cross-checked against a real *Rust* executable,
which uses thread-locals, so the bit is set there correctly. It is not a
constant, though: it belongs only in an image that actually has thread-local
variable descriptors. blinker sets it unconditionally, so it is wrong for any
image without TLS.

This is latent rather than active — blinker's inputs are Rust — but it is the
same shape as the `MH_APP_EXTENSION_SAFE` error in finding 12: a *conditional*
property read off one sample and frozen into a constant. Deriving it from the
presence of `__thread_vars`/`__thread_bss` in the layout is the fix.

### The rest of the gap

With calibration fixed, blinker matches ld64 on 9 of the 13 compared
properties for a trivial program — identity, load-command order, segment set,
segment placement, section sizes, dependencies, undefined symbols, local
symbol count and entry point. What remains is a to-do list rather than a
surprise: `LC_FUNCTION_STARTS`, `LC_DATA_IN_CODE`, `LC_CODE_SIGNATURE`, and
`__TEXT,__unwind_info`.

## 15. Object file paths are not stable across builds, so a path-keyed cache misses every time

Measuring rustc's incremental output required comparing object files between
builds. The first attempt reported that **every** object file changed on
**every** edit — 13 of 13, for a one-character change. That was the
measurement, not the compiler:

```
gen0: edittest-e3ba9c90af78615f.0iyf66cnpo8vwk1rztk4nxg1f.14oibsx.rcgu.o
gen1: edittest-e3ba9c90af78615f.0iyf66cnpo8vwk1rztk4nxg1f.0q0qm9y.rcgu.o
                                 └── CGU identity: stable ──┘ └ session ┘
```

The filename has three variable components. The second identifies the codegen
unit and is **stable across builds**. The third is a per-build session id that
changes every single time, even when nothing was edited.

**Consequence for M4:** a cache keyed on input path has a 0% hit rate. Not a
low rate — zero, always, by construction. The key must be the CGU identity or
the file's content hash. blinker already fingerprints by content, so the fix is
to key on that rather than on the path recorded in `argv`.

## 16. Monomorphization does not fan out across codegen units in debug builds

The concern: editing a generic function's body invalidates every
instantiation, so one source edit becomes N symbol changes, where N is the
number of concrete types it is used at. If true, it would be the single
largest threat to incremental patching of Rust.

Measured instead of assumed. A generic instantiated at four concrete types
across four separate modules:

```
$ nm -jU *.rcgu.o | grep generic_scale
__RINv…11lib_generic13generic_scaledEB4_    (f64)
__RINv…11lib_generic13generic_scalehEB4_    (u8)
__RINv…11lib_generic13generic_scalemEB4_    (u32)
__RINv…11lib_generic13generic_scalexEB4_    (i64)
```

All four live in **one** CGU — the one where the generic is *defined*, not
where it is used. Editing its body changed 1 CGU of 15, the same as editing a
non-generic function. Cross-crate, with a downstream crate instantiating at
four types the defining crate never uses, the answer was the same: 1 CGU.

The mechanism is `-Zshare-generics`, which is on by default in debug builds —
instantiations are shared and placed with the definition. Debug builds are
exactly the agentic-edit case, so this lands on the favourable side.

**Not yet measured:** release builds, where share-generics is off and the
fanout should reappear. Recorded as a known gap rather than assumed benign.

## 17. Symbol names cannot classify an edit, because Rust's mangling omits signatures

The natural way to bucket an edit — compare the old and new symbol tables — does
not work. Measured deltas in the one CGU each edit touched:

| edit | symbols added/removed | `__text` size |
|---|---|---|
| body edit, non-generic | +0 / −0 | 660 → 696 (**+36**) |
| additive (new function) | +2 / −0 | 660 → 792 (+132) |
| **signature change** `f(a,b)` → `f(a,b,c)` | **+0 / −0** | 660 → 712 (+52) |
| **struct layout change** (new field) | +0 / −1 (a temp label) | 660 → 660 (+0) |

The two cascading edits are the two that a symbol-name diff calls *identical*.
Rust's v0 mangling for a non-generic free function encodes its **path**, not
its type signature, so changing the signature leaves the mangled name
untouched. A struct gaining a field changes no exported name at all.

So tiers 1 and 3 are indistinguishable by symbol comparison. Classification has
to come from the set of changed CGUs plus reference analysis, not from names.

Also note the first row: a body edit that added `+ 0` grew `__text` by **36
bytes**. "Same signature, same or near-same size" is not automatic even for the
easiest case — the slop budget for in-place patching has to be measured against
real edits rather than assumed small.

## 18. rustc's incremental verdict is already on the filesystem

The appealing idea is to consume rustc's own per-item dependency fingerprints
rather than re-deriving them by diffing object files. The literal version means
reading `target/debug/incremental/**/dep-graph.bin`, which is an unstable
internal format with no compatibility guarantee.

It is not necessary. With incremental compilation on, rustc **reuses the object
file for any CGU it determined was unchanged**. So "which `.rcgu.o` files have
new content" *is* the compiler's own answer to "what changed", already
materialized, through a stable interface — files on disk. The one-CGU-changed
measurements above are that signal being read.

This is what makes finding 15 load-bearing: the signal is only usable once the
comparison is keyed on CGU identity instead of path.

## 19. Ad-hoc signing is part of linking on Apple Silicon, and two of its constants are counter-intuitive

On arm64 macOS the kernel does not warn about an unsigned image, it kills the
process before any of its code runs. Measured on blinker's own output:

```
unsigned          → exit 137   (128 + 9, SIGKILL)
ad-hoc signed     → exit 42    (what the program computes)
one byte flipped  → exit 137   "code or signature have been modified"
```

So a linker that cannot sign cannot produce a program here. This is not a
post-processing step that can be left to `codesign`; `ld64` does it inline and
so must blinker.

The structure was read out of a real signature rather than from headers, and
the page hashes recomputed independently to confirm what they cover:

```
SuperBlob (0xfade0cc0)
  ├─ slot 0x00000  CodeDirectory (0xfade0c02)   SHA-256 per page + special slots
  ├─ slot 0x00002  Requirements  (0xfade0c01)   an empty set, 12 bytes
  └─ slot 0x10000  CMS signature (0xfade0b01)   empty — that is what ad-hoc means
```

Two things would have been got wrong by reasoning from the format's shape:

- **The blobs are big-endian**, inside a file that is little-endian
  everywhere else.
- **The signing page is 16 KiB, not 4 KiB.** The `CodeDirectory` field is
  `pageSize = 14`, a log2. Assuming the familiar 4 KiB yields four times the
  slots and an image the kernel refuses, with no diagnostic pointing at the
  page size.

The special slots are also indexed *backwards* from the code hashes: the 32
bytes immediately before them are slot −1, the 32 before that slot −2. Slot −2
holds the requirements hash and slot −1 is zeroed because there is no
Info.plist — so having requirements at all forces an empty info slot to exist
as padding.

### The size has to be exact before any bytes exist

The signature covers the file up to where it begins, and `LC_CODE_SIGNATURE`
points at that offset — a load command *inside* the hashed region. So the
signature's size must be computed before the image is emitted, and a one-byte
error moves `code_limit`, changing every page hash. `signature_size` and `sign`
are therefore checked against each other for a range of image sizes, rather
than trusting that they agree.

## 20. Signing turned a latent layout bug into a corrupted header

Enabling signing broke the degenerate case: an image with no sections came out
with `ncmds = 3222068986` and `otool` reporting `load command 0 extends past
end of load commands`.

`__LINKEDIT`'s file offset was computed as the maximum end of the mapped
segments — and an image with no sections has none, so the fallback was zero.
Nothing had noticed, because the emitter only ever *padded* forward to that
offset, and padding to zero is a no-op. Signing truncates to it, so the same
wrong number that had been harmless now cut the header and load commands off
the front of the file.

Two things follow. `__LINKEDIT` now starts past the header and load commands
whether or not anything else was laid out. And the truncation is guarded by an
explicit error rather than an assumption, because the failure mode when it is
wrong is a file too corrupt to diagnose from.

The empty-image test that caught this looked like completeness padding when it
was written. It was the only test that ran the degenerate shape end to end.

## 21. Cross-object *data* is the case a test suite quietly misses

The first end-to-end link tests all passed: a single object ran, a cross-object
call ran, a global read ran. Linking two objects by hand, outside the harness,
failed immediately:

```
link failed: object 1: cannot apply GotLoadPageOff12:
  ARM64_RELOC_GOT_LOAD_PAGEOFF12 needs an indirect address that was not supplied
```

Three shapes look similar and are not:

| reference | relocation | needs a GOT |
|---|---|---|
| global read, same object | `PAGE21` + `PAGEOFF12` | no |
| call to another object | `BRANCH26` | no |
| **data in another object** | `GOT_LOAD_PAGE21` + `GOT_LOAD_PAGEOFF12` | **yes** |

The compiler cannot know whether an `extern` definition will end up in this
image or in a dylib, so it emits the indirect form unconditionally and leaves
the choice to the linker. Every test in the suite had picked one of the two
shapes that avoid it.

The lesson is not "write more tests" — the suite was reasonable. It is that a
passing suite says nothing about the cases it does not contain, and running the
thing by hand is how you find out which those are. This project has now had two
such catches: the differential harness's deployment target (finding 13) and
this one, both from doing the obvious thing manually and looking at the result.

### What synthesising `__got` requires

Four things, none of which the relocation engine can do alone:

1. Scan every relocation for GOT-needing kinds and collect the target symbols.
2. Place a synthesised section *before* layout runs, so it is addressed like
   any other rather than appended afterwards. Layout keys contributions by
   `(object, section)`, so the linker needs an object id that cannot collide
   with a real input's.
3. Patch the GOT-based relocations with the address of the **slot**, not of the
   symbol — the symbol's address is what the slot *contains*.
4. Emit a rebase entry per slot. The slots hold absolute addresses and the
   image is position independent, so dyld has to relocate every one at load.

Step 3 is the one that reads wrong until it clicks: `Context.target` and
`Context.got` are different addresses and the instruction needs the latter.

### Also found: the same-object assumption

The cross-object *call* test failed first for an unrelated reason. Symbol
addresses were resolved by searching the object holding the relocation, which
is correct for locals and for self-contained files, and wrong the moment one
object refers to another — the definition is elsewhere, so the lookup reported
a symbol undefined that resolution had already found. Two coordinate systems
that coincide in the single-object case.

`blinker-link`'s module documentation had predicted exactly this class of bug
("a symbol whose address is computed in one coordinate system and consumed in
another") before the code was written. Predicting it did not prevent writing it.

## 22. Imports must be resolved before undefined symbols are an error

Adding libSystem support, the first attempt reported:

```
link failed: undefined symbols:
  _printf
```

for a program that links against libSystem, whose `.tbd` stub blinker had just
successfully read 9,264 symbols from — `_printf` among them. The check simply
ran in the wrong order: symbol resolution validated completeness *before* the
dylibs were given their chance at the leftovers. An undefined reference is only
an error once every provider has declined it.

The same mistake has a second form, found immediately after. A GOT-based
relocation to an *imported* symbol asked for that symbol's own address — which
an import does not have, and never will; that is what importing means. The
instruction is patched from the **slot's** address, and the symbol's address is
what dyld writes into the slot at load time. The lookup was not merely
redundant, it was a category error, and it failed the link with the same
misleading "undefined symbol" wording.

Both are the same shape: a stage asking a question that is only meaningful for
symbols the image defines itself.

## 23. Non-lazy stubs are three instructions, and enough

ld64's default stubs jump through `__la_symbol_ptr` into `__stub_helper`, which
calls `dyld_stub_binder` on first use. Reproducing that means three more
synthesised sections and a second opcode stream, all to defer work.

Binding eagerly needs one section and this:

```asm
adrp x16, <got page>          ; page containing the slot
ldr  x16, [x16, <page off>]   ; the address dyld bound
br   x16
```

Twelve bytes, matching the shape ld64 emits for its non-lazy stubs — confirmed
against a real Rust binary's `__TEXT,__stubs`, where every entry decodes to
`ADRP x16` / `LDR x16,[x16,#n]` / `BR x16` (`0xd61f0200`).

A `BRANCH26` needs a stub only because it cannot reach an address that does not
exist until load time. Data references already go through the GOT and need
nothing extra — which is why only *called* imports get one.

Lazy binding is an optimisation to add when there is something to measure it
against. For a short-lived process it defers work that is never saved.

### What a realistic program actually needs

```
$ blinker-link real.o -o real && ./real
sorted: 1 3 7 19 42 88
strlen=22
$ echo $?                       # identical to ld64's output and status
89
```

`qsort`, `malloc`, `strcpy`, `strcat`, `snprintf`, `printf`, `strlen`, `free` —
all imported functions through stubs — plus `___stack_chk_guard`, an imported
*data* symbol through the GOT. The stack protector is what pulls in that last
case, and no smaller test reaches it.

## 24. Linking Rust is a queue of distinct walls, not one big one

Pointing the driver at a `println!`-and-exit Rust program produced five
failures in a row, each precise and each a different missing feature. Worth
recording because the *shape* is the useful part: none of them were subtle, and
none were visible from linking C.

1. **`.rlib` archives.** rustc passes the whole sysroot as archives.
   `Unsupported Mach-O header` — blinker was trying to parse an archive as an
   object. Archives are not simply expanded either: `libstd.rlib` holds
   hundreds of members, and pulling them all in would produce a binary tens of
   megabytes larger than it should be. A member enters the link only when it
   defines a symbol something already in the link needs, and pulling it in can
   create new undefined symbols — so extraction is a loop to a fixed point.

2. **`ARM64_RELOC_SUBTRACTOR`.** A *paired* relocation: SUBTRACTOR (the value
   subtracted) immediately followed by UNSIGNED (the value subtracted from).
   Meaningless alone. Relative pointers in unwind and exception tables are
   built this way, so Rust hits it immediately and simple C never does. The
   relocation loop had to become index-based to consume two entries at once.

3. **`ARM64_RELOC_TLVP_*`.** Thread-local access, structurally the GOT again —
   a pointer table the instruction loads from — but pointing at a TLV
   *descriptor* rather than the variable.

4. **A data reference to an imported symbol.** A TLV descriptor's first word
   points at `__tlv_bootstrap`, which lives in libdyld. A pointer-sized field
   whose target is an import cannot be patched at all: the address does not
   exist until load. The field stays zero and a bind entry tells dyld where to
   write. Until this, binds were only ever emitted for GOT slots.

5. **`SG_READ_ONLY`.** dyld *refuses to load* an image whose `__DATA_CONST`
   segment lacks the flag — "__DATA_CONST segment missing SG_READ_ONLY flag".
   The emitter had a comment deferring it as an optimisation. It is not an
   optimisation; it is a load requirement.

After all five, an 8.5 MB Rust executable links and dyld gets as far as
thread-local setup before rejecting it:

```
malformed thread-local, offset=0x1007F9ED0 is larger than total size=0x0
```

which is the next real piece of work: a TLV descriptor's third word is an
**offset into the thread-local block**, not an address, and the block's total
size has to be computed from `__thread_data` + `__thread_bss` and recorded.
blinker is currently writing absolute addresses into a region it has declared
to be zero bytes long.

### The useful part

Each wall was found by running the thing, took one measurement to identify, and
had an unambiguous fix. None would have been found by more unit tests of the
stages in isolation, because every stage was individually correct — what was
missing was a case none of them had ever been handed.

## 25. Every absolute pointer in data needs a rebase, and C never taught us that

With thread-local sections correctly typed, the Rust binary loaded and then
segfaulted:

```
EXC_BAD_ACCESS  KERN_INVALID_ADDRESS at 0x00000001000c8fa8
  rs2 +199596  std::rt::lang_start_internal
```

The fault address is the giveaway. The image had loaded at `0x102064000`, but
the address being dereferenced was `0x1000c8fa8` — in the **unslid** address
space. That is the signature of a pointer written at link time and never
rebased: correct arithmetic, applied to a base the process is not using.

A position-independent image is loaded at a random offset, so *every* absolute
address baked into data must be listed in the rebase stream for dyld to slide.
blinker was emitting rebases only for GOT slots.

C hid this completely. C globals are reached PC-relatively through
`ADRP`/`ADD`, so a small C program contains almost no absolute pointers in
data — the C test suite could never have caught it. Rust's vtables, statics
and panic metadata are full of them, and `lang_start_internal` is simply the
first code that walks one.

Two fixes, both the same rule applied twice:

1. Every pointer-sized `UNSIGNED` relocation writing into a writable segment
   gets a rebase entry — not just the GOT's.
2. **Synthesised pointer tables need them too.** `__thread_ptrs` holds absolute
   addresses of thread-local descriptors, and it was missed even after fix 1,
   because its contents are written by the linker rather than derived from a
   relocation. That omission alone reproduced the identical crash, in the
   identical function.

### Diagnosed without a debugger

`lldb` on macOS triggers the Developer Tools Access authorization prompt, which
wants a password; nothing else in this project needs one, and a build that
demands `sudo` is not one an agent can run unattended. The OS had already
written everything needed to
`~/Library/Logs/DiagnosticReports/*.ips` — fault address, signal, and a
symbolicated stack. Reading the crash report is both faster and unprivileged.

## 26. Thread-local descriptors store offsets, not addresses

Before the crash above, dyld rejected the image outright:

```
malformed thread-local, offset=0x1007F9ED0 is larger than total size=0x0
```

Both halves of that message were wrong for the same reason, and neither is
about the offset it names.

**`total size=0x0`** — dyld computes the per-thread block's size from the
sections *typed* as thread-local data. blinker was placing `__thread_data` and
`__thread_bss` in `__DATA` correctly but leaving them typed `S_REGULAR`, so
dyld found no thread-local storage at all. Measured from a real binary, the
types are `__thread_data` = `0x11`, `__thread_bss` = `0x12`, `__thread_vars` =
`0x13`, and the pointer table `0x14`.

**`offset=0x1007F9ED0`** — a TLV descriptor's three words are
`{thunk, key, offset}`, and the third is the variable's offset **within the
block**, not its address. The block is copied per thread, so an address there
is meaningless. Confirmed by reading a real descriptor, whose third word was
`0x38` where blinker was writing a full `0x1_0000_0000`-based address.

## 27. `__LINKEDIT` opcode streams must be 8-byte aligned, and dyld fails silently when they are not

`dyld_info` on a blinker-linked Rust binary refused to read it at all:

```
mis-aligned LINKEDIT content 'bind opcodes'
```

The rebase stream is an arbitrary number of bytes, and the bind stream was
written immediately after it, so it began wherever that happened to end. dyld
reads these through pointer-sized loads and rejects a misaligned stream —
**every bind in it is then simply never applied**, with no diagnostic at
runtime. The failure surfaces as a crash at the first use of anything that
needed binding, arbitrarily far from the linker.

This is the third alignment rule in this format to bite (after 8-byte load
commands in finding 12 and the symbol table's own alignment), and the second
to fail *silently* rather than loudly.

## 28. Section order is load-bearing for thread-locals

`__thread_bss` and `__thread_ptrs` were absent from the layout's section-order
table, so they sorted to `usize::MAX` — the end of `__DATA`, behind `__bss`,
`__common`, and every unrecognised section. The emitted order was:

```
__thread_vars __thread_data __bss __common __bitcode __cmdline __got __thread_bss
```

dyld treats `__thread_data` followed by `__thread_bss` as **one contiguous
block** that it copies per thread, and a descriptor's offset is relative to the
start of it. With unrelated sections in between, every offset into the block
was wrong by however much sat between them.

Two related defects fell out of the same inspection:

- `__LLVM,__bitcode` and `__LLVM,__cmdline` were being carried into the image.
  They are inputs to further tooling, not parts of a program; ld64 drops them.
  Here they were sitting in the middle of `__DATA`, between sections required
  to be adjacent.
- `__got` was landing in `__DATA` and, being unranked, sorted *after* the
  zero-filled sections — a section with file content placed after ones with
  none. It belongs in `__DATA_CONST` with `__const`: both hold pointers dyld
  writes once and never again.

### Still failing: panics

With all of the above fixed, a panicking Rust program still dies in
`std::panicking::panic_count::increase`, which is a thread-local access, before
any message is printed. `-C panic=abort` fails identically, which rules
unwinding *out* — this is not the missing `__unwind_info`. Something about the
thread-local path remains wrong, and the descriptors and fixup streams now look
structurally correct, so the next step is comparing the per-thread block byte
for byte against ld64's rather than reasoning about it.

Recorded rather than guessed at: non-panicking Rust programs link and run
correctly, and that is the honest boundary of what works.

## 29. A synthesised pointer table cannot be filled by a global symbol lookup

Several slots of `__thread_ptrs` were written as **zero**:

```
__thread_ptrs:
  00000000 00000000   ← null descriptor pointer
  000c8fd0 00000001
  00000000 00000000   ← null
  000c8fe8 00000001
```

A thread-local access loads the descriptor address from its slot and calls
through the descriptor's first word. A zero slot is a null dereference on first
use — which is exactly where `panic_count::increase` died.

The cause: the fill routine looked each symbol up as
`addresses.lookup(SYNTHETIC_OBJECT, name)`. Local symbols are keyed **per
object**, because two objects may legitimately define the same local name, so
a lookup under the linker's own synthetic id sees only globals. Thread-locals
inside `libstd` are frequently local, and every one of those got a zero slot.

The fix is to carry the *referencing object* alongside the name in the table,
and look up against that — so a local definition is visible from the object
that referenced it. The same defect was present in the GOT and fixed with it;
it had simply not been reached, because the C tests only ever put globals
there.

This is the third instance of one root confusion in this project: **a symbol
name alone is not a key.** Findings 21 and 25 were the same mistake in
different clothing.

### Result

The panic path now works end to end under `-C panic=abort`:

```
before
thread 'main' panicked at src/main.rs:1:33:
boom
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

terminating with `SIGABRT`, matching ld64 exactly. Reaching the abort means
the whole thread-local mechanism is correct: descriptors, the pointer table,
its rebases, and the block-relative offsets.

`-C panic=unwind` — the default — still crashes *after* printing the message,
in the unwinder. That is the genuine `__unwind_info` gap: blinker drops the
input objects' `__LD,__compact_unwind` and never synthesises
`__TEXT,__unwind_info`, so there are no tables for the unwinder to walk.
Known, recorded, and not hidden behind a passing test.

### A test that asserted the wrong thing

The first version of the regression test asserted `status.code() == Some(134)`
and failed against behaviour that was already correct. A process killed by a
signal has **no exit code** — `code()` returns `None`, and the 134 a shell
prints is the shell's own `128 + SIGABRT` encoding. The test now distinguishes
`Exited(n)` from `Signalled(n)`, which is a distinction the platform makes and
the test had quietly collapsed.

## 30. `__unwind_info`: the addend is stored in the field, not the relocation

`__TEXT,__unwind_info` is now synthesised from the input objects'
`__LD,__compact_unwind`. Two measurements shaped it, and one caught a bug that
would have been invisible from the output's structure.

**The table was structurally valid and described almost nothing.** First
working version produced 17 entries where ld64 produced 469. Every
compact-unwind record points at its function through a relocation whose
**addend is stored inline, in the eight bytes being patched** — not in the
relocation entry. Resolving the relocation target alone gives the *section
base*, so hundreds of records that all point into `__text` resolved to the same
handful of addresses and collapsed under de-duplication. Reading the inline
value and rebasing it out of the object's coordinate space took the count to
2186, with offsets that line up against `__text`:

```
__text  addr 0x100000900
entry   funcOff 0x900   enc 0x04000000
entry   funcOff 0x940   enc 0x04000000
```

This is the same coordinate-space confusion as findings 21, 25 and 29, in a
fourth costume: an address that is meaningful only relative to something, used
as though it were absolute.

**A size that must be known before the addresses exist.** The table's contents
depend on the layout, but its *size* has to be fixed before layout runs. It is
reserved from an upper bound computed from the record count — every record
assumed distinct, carrying an LSDA, with the maximum three personalities — and
a test asserts the bound is never exceeded for any shape of input. Over-
reserving wastes a few kilobytes; under-reserving fails the link.

### Still not working: `panic=unwind`

The unwinder now *consumes* the table — the failure changed from a clean
`abort` (no tables found) to a `SIGSEGV` (tables found, followed, wrong). The
remaining defect is visible in the header:

```
blinker: personalities n=0
ld64:    personalities n=1   ['0x44000']
```

Rust's landing pads are reached through `rust_eh_personality`, and blinker
records no personality at all, so the unwinder has nothing to call. ld64's
entry is an image-relative offset to the **GOT slot** holding the personality
pointer, not to the function — so the fix is to route the personality
relocation through the GOT the way a data reference to an import already is,
rather than resolving it to an address directly.

Recorded as the precise next step rather than left as "unwinding is broken".

## 31. Most Rust functions use DWARF-mode compact unwind, which is a pointer into `__eh_frame`

With `__unwind_info` built and personalities routed through the GOT, the table
still had zero personalities and zero LSDAs, and `panic=unwind` still crashed.
Counting the encoding modes explained why:

| mode | blinker | ld64 |
|---|---|---|
| `FRAME` | 160 | 17 |
| **`DWARF`** | **1408** | **375** |
| `FRAMELESS` | 618 | 74 |

`UNWIND_ARM64_MODE_DWARF` (`0x03000000`) does not describe the frame at all.
Its low 24 bits are an **offset into `__eh_frame`** where the function's FDE
lives, and the unwinder follows it to the real DWARF description. ld64's 375
DWARF entries all carry a non-zero offset; every one of blinker's 1408 carries
zero, so the unwinder dereferences the start of `__eh_frame` for every function
and reads whatever is there.

That also explains the two zero counts. In DWARF mode the personality and the
LSDA live in the CIE and FDE respectively, not in the compact record — so there
are no personality or LSDA *relocations* to find in `__compact_unwind`, and
looking for them was searching for something that was never there. Routing
personalities through the GOT was correct and is kept, but it was not the
blocker.

### What is actually required

An `__eh_frame` parser: walk the CIE/FDE records of each input object, map each
FDE to the function it describes, compute where that FDE lands in the output
`__eh_frame`, and write that offset into the encoding's low 24 bits. Until
then, DWARF-mode functions have no usable unwind description.

This is the honest boundary. `-C panic=abort` works completely; `panic=unwind`
does not, and the remaining work is one well-specified component rather than an
open question.

### The pattern across findings 21, 25, 29, 30 and 31

Five of the hardest bugs in this project were the same mistake: **an offset
used as though it were an address, or an address as though it were an offset.**
Cross-object symbol lookup, unrebased absolute pointers, null pointer-table
slots, inline compact-unwind addends, and now DWARF-mode encodings. A linker is
largely a program for translating between coordinate spaces, and every one of
these was a place where two spaces coincided in the simple case and diverged in
the real one.

## 32. The FDE's function comes from a relocation, not from decoding DWARF

Filling the DWARF-mode encodings needs, for each function, the offset of its
FDE within the output `__eh_frame`. Two things are required: where each record
begins, and which function it covers.

Boundaries are easy — every CFI record starts with its own length, and a zero
length terminates the section. A record whose second word is zero is a CIE;
anything else is an FDE, and that word is the distance back to its CIE.

The function is where it looks hard. An FDE's `PC begin` field is encoded
according to its CIE's augmentation string, so the textbook approach is to
parse the augmentation, learn the pointer encoding, and decode accordingly.
None of that is necessary here: in a **relocatable** object that field carries
a relocation, so the target is already available from the relocation list, in
the same form the rest of the linker consumes. Decoding the DWARF encoding
would be re-deriving something the object states outright.

The result matches ld64 value for value:

```
blinker  funcOff 0x96c  enc 0x03000014
blinker  funcOff 0x994  enc 0x03000048
ld64     funcOff 0x8f4  enc 0x03000014
ld64     funcOff 0x91c  enc 0x03000048
```

All 1408 DWARF-mode entries now carry a real offset, where every one of them
previously carried zero.

### Still crashing, and now the search has moved

`panic=unwind` still faults. What changed is *where the problem must be*: the
index into `__eh_frame` is now demonstrably correct, so the remaining fault is
in the `__eh_frame` **contents** — most likely the relocations applied to the
FDEs' own pointer fields, which are PC-relative under most CIE augmentations
and are being handled by the generic relocation path.

That is a narrower and better-posed question than the one this section started
with, and the evidence for it is that every layer above has been checked
against ld64 and agrees.

## 33. The `__eh_frame` hypothesis was wrong, and the measurement was cheap

The previous section ended with a hypothesis: that FDE pointer fields might
resolve to absolute addresses, which cannot be rebased because `__TEXT` is
read-only, leaving them stale after ASLR slide — the same shape as finding 25,
one layer down. It was plausible, it explained the symptom, and it was wrong.

Counting the relocations in a real Rust object's `__TEXT,__eh_frame`:

```
address  pcrel length extern type  symbolnum
0000001c 0     3      1      1     3          ← SUBTRACTOR
0000001c 0     3      1      0     4          ← UNSIGNED
```

Two relocations at the same offset: a **SUBTRACTOR pair**, computing a
difference rather than an address. Differences are slide-independent by
construction, so there is nothing for a rebase to fix and nothing stale. The
pair path already handles these (finding 24, item 2).

The same dump also confirms, incidentally, that the FDE's `PC begin` sits at
`FDE start + 8` — the CIE occupies `0x00..0x14`, the FDE begins at `0x14`, and
`0x1c` is exactly eight bytes in. The offset the parser assumes is the offset
the object uses.

So `__eh_frame`'s contents are relocated correctly, its index from
`__unwind_info` is correct and matches ld64 value for value, and
`panic=unwind` still faults. The cause is somewhere neither of those, and the
next investigation should start by *finding* it rather than proposing it.

**The point worth keeping:** the hypothesis cost one command to refute. Acting
on it — auditing rebase coverage for `__TEXT`, adding machinery to rebase
read-only pointers — would have cost hours and produced nothing, because the
premise was false. This project has now had two hypotheses that survived
reasoning and died to a single measurement (the other being the dyld strategy
in finding 13). Both times the measurement was available before the reasoning
started.

## 34. Cold-link measurement: blinker is 1.6× slower than ld64, and 2.25× larger

> **Superseded by findings 38 and 39.** The ratio here is wrong (see 38) and
> the linker called `ld64` throughout is really `ld-prime` (see 39).

The first real benchmark, on the argument vector `rustc` actually handed the
linker for a small Rust binary (8 objects, 19 rlibs, 17 MB of input):

| linker | time | output size |
|---|---|---|
| `ld64` invoked directly | **27.9 ms** | 468 KB |
| `blinker` (release) | **44.6 ms** | 1056 KB |
| `cc` → `ld64` (driver spawn included) | 44.6 ms | — |

Three things worth separating.

**blinker is 1.6× slower cold.** Not the order of magnitude that finding D3
feared, but not parity either, and parity was never the plan — a linker that
re-reads 17 MB and rebuilds everything cannot beat one that does the same thing
in optimised C++. This number matters because it is the floor the cache has to
beat: M4 has to save more than 44.6 ms of work to be worth anything, and it
starts from a 16.7 ms deficit.

**The `cc` driver costs as much as the link.** Spawning `ld` takes ld64's
27.9 ms to 44.6 ms. Any comparison that measures `cc` rather than `ld` is
measuring process creation as though it were linking — which is exactly the
mistake the first attempt at this benchmark made.

**The output is 2.25× larger** because blinker does not dead-strip. Every
member pulled out of an archive is emitted whole. That is a correctness-neutral
gap today and a real one for anything shipping.

### Two failed measurements before this one

The first run reported blinker at 35.8 ms against ld64's 64.9 ms — a win. Both
numbers were of **failing** linkers: the object list was passed as a single
quoted argument, both programs rejected it immediately, and output was
redirected to `/dev/null` so neither said so. A benchmark that does not assert
its subject succeeded is measuring startup cost.

The second attempt fixed the quoting and still failed, because the objects
alone do not link — the rlibs carry `rust_panic` and the allocator shims. Both
linkers agreed exactly on which symbols were missing, which is what made the
failure obvious.

The rule this leaves: **a benchmark must verify its output before it reports a
time**, and the verification belongs in the harness rather than in the
operator's memory. The run above checks exit status and file size for both
linkers, and it is the reason the third set of numbers can be trusted where the
first cannot.

## 35. No phase dominates, which changes what M4 can be worth

The plan assumed a persistent cache of *parse results* would be the win, on the
reasoning that parsing dominates a cold link. Profiling the link says
otherwise. Same 27-input Rust link as finding 34:

```
  read+parse      7.4 ms   29.6%
  resolve         6.9 ms   27.8%
  layout          1.6 ms    6.3%
  relocate        6.2 ms   24.8%
  emit+sign       2.1 ms    8.5%
  total          24.8 ms
```

Three phases are within a factor of 1.2 of each other and together are 82% of
the time. **Caching parses can save at most 7.4 ms of 24.8 ms**, and only if
the cache lookup itself were free — it is not, since it requires hashing the
same inputs the parse would have read.

That is an upper bound of about 30%, against a linker that is currently 1.6×
slower than ld64. A perfect parse cache would bring blinker from 44.6 ms to
roughly 37 ms, still behind ld64's 27.9 ms. **A parse cache alone cannot make
blinker faster than the thing it replaces.**

### What the numbers actually argue for

The win has to come from not redoing `resolve` and `relocate` either — which
means caching the *linked output* and patching it, not the parsed inputs. That
is M5's incremental relink, and this profile says it is not an optimisation to
layer on top of M4 but the point of the exercise.

Findings 15–18 remain the design for how to key that cache (CGU identity or
content hash, never path; rustc's own incremental verdict readable from which
`.rcgu.o` files changed). What changes is *what is stored under that key*: not
`ParsedObject`s, but enough of the finished layout and patched section content
to rebuild an image while touching only the CGUs that changed.

### Also visible: the CLI costs roughly twice the library link

The library links in 24.8 ms; the same link through the CLI reports 48 ms and
takes 46.5 ms of wall time. Where that difference goes is **not yet
established**.

My first explanation was that `fingerprint_input` reads every input to hash it
and the link then reads them all again. That is wrong: `InputFingerprint::probe`
only calls `metadata()` unless `hash_contents` is set, and the CLI leaves
`--blinker-strict-fingerprints` off by default, so fingerprinting 27 files is 27
`stat` calls. It is not the cost.

Recorded as an open question rather than an explanation, because a plausible
story that has not been measured is exactly what this project keeps punishing.
The next step is to time the CLI's phases directly rather than infer them —
the record already carries `argument_parsing_ms` and `input_fingerprinting_ms`,
and the internal link is currently lumped under `fallback_exec_ms`, which is
itself a naming bug now that the link is not a fallback.

## 36. A third of the link was in the gaps between the timers

> **Retracted by finding 40.** The 31% gap was the `xcrun` spawn being charged
> to untimed work, not repeated traversal. Measured properly it is 4.9%.

Instrumenting the CLI's internal link answered the open question from finding
35, and corrected finding 35 at the same time:

```
  link: 38.6 ms
    read+parse     8.5 ms  22.0%
    resolve        7.3 ms  18.8%
    layout         1.8 ms   4.7%
    relocate       6.8 ms  17.7%
    emit+sign      2.3 ms   6.1%
```

The named stages sum to **26.7 ms of 38.6 ms**. The other 11.9 ms — 31% of the
link — is work in the gaps *between* the timers, and it is not small:

- the pre-layout scans that decide what to synthesise (`got_symbols`,
  `personality_symbols`, `tlv_symbols`, `stub_symbols`, `unwind_table_size`),
  each of which walks every relocation of every object;
- `output_symbols`, which walks every symbol of every object and does a layout
  lookup per definition;
- the rebase and bind table construction.

Every one of those is a full pass over data the timed stages have already
walked. The link makes roughly half a dozen separate traversals of the same
symbol and relocation lists.

### What this does to the earlier conclusion

Finding 35 concluded that no phase dominates, and that a parse cache could save
at most ~30%. That conclusion survives — it is if anything stronger, since
read+parse is 22% of the link rather than 29.6%. But the *reason* has shifted:
the largest single category is not any named phase, it is **repeated traversal**,
and that is a cost a cache does not address at all. It is addressed by making
one pass collect what six passes currently collect separately.

That is a cheaper and more certain win than the cache, and it comes first: it
reduces the work the cache would otherwise have to memoise, and it does not
depend on any of the open questions about what to store or how to key it.

### On the instrumentation itself

The gap only became visible because the stage timings were reported *next to*
the total rather than on their own. Percentages that sum to 69% are obvious;
five numbers in isolation are not. The summary now prints both, so the next
person to read a profile sees the discrepancy without having to add the column
up by hand.

## 37. Two optimisations, no measurable win, and a benchmark too noisy to tell

Finding 36 attributed 31% of the link to repeated traversal and called it the
first thing to fix. Two changes followed, and neither is defensible as a win.

**Collapsing four relocation walks into one.** `got_symbols`,
`personality_symbols`, `tlv_symbols` and `stub_symbols` each walked every
relocation of every object; they are now one pass. Result: 46.3 ms against a
46.5 ms baseline. No change.

**Caching the SDK lookup.** `LinkRequest::new` calls `default_stub_library()`,
which spawns `xcrun --show-sdk-path` — measured at **14 ms**, a third of a
40 ms link. That looked like the whole missing gap. Result: 44.7 ms, and
setting `SDKROOT` (which skips the spawn entirely) gave 47.1 ms — *slower*.

Two things were wrong with the second one:

- A `OnceLock` caches within **one process**. blinker is spawned once per link,
  so an in-process cache can never be hit twice. The change is correct in the
  library, and worth nothing to the CLI.
- The numbers do not separate a 14 ms effect from noise, which means they were
  never going to confirm or refute the hypothesis.

### The real finding is about the benchmark

Across four runs of the same binary on the same inputs: 44.7, 46.3, 46.5,
47.1 ms — a spread of about 5%. A 14 ms effect on a 46 ms link should be
impossible to miss, and it was missed, so the harness is not measuring what it
claims to. Seven iterations of a subprocess with no warmup control, no
interleaving, and no variance reported is not an instrument; it is an anecdote
with a decimal point.

**Nothing further should be optimised until the benchmark reports variance and
interleaves the two linkers.** Every number in finding 34 and 36 carries this
caveat, including the 27.9 ms vs 44.6 ms comparison that the whole M3 conclusion
rests on — the *ratio* there is large enough to survive 5% noise, but the phase
percentages are not.

The two code changes are kept: one pass over the relocations is simpler than
four regardless of speed, and honouring `SDKROOT` before spawning `xcrun` is
correct because the compiler driver already sets it. Neither is claimed as a
performance improvement.

## 38. The benchmark was wrong, and blinker is near parity with ld64

> **Naming corrected by finding 39:** the baseline called `ld64` here is
> Apple's `ld-prime`. The measurements stand; the name does not.

Rebuilding the harness (interleaved runs, controlled warmup, verified output,
reported spread) changed the headline number of this project.

```
inputs: 27 files, 16.9 MB — 40 iterations after 5 warmup

  ld64         28.3 ms  (min 25.0, max 29.5, sd 1.1, spread 16%)  output  458 KB
  blinker      31.0 ms  (min 29.4, max 33.9, sd 0.9, spread 14%)  output 1031 KB

  blinker/ld64: 1.10x   output ratio: 2.25x
```

**Finding 34 reported 1.6×. The real figure is about 1.10×.** That earlier
number compared blinker's 44.6 ms against an ld64 sample of 27.9 ms — and 27.9
was near the *fast end* of ld64's distribution while 44.6 was near the slow end
of blinker's. Two unlucky samples, one ratio, no variance reported, and a
strategic conclusion drawn from it.

It also shows how much the harness itself mattered: at 15 iterations the spread
was 27–39% and the ratio read 1.17×; at 40 with more warmup the spread fell to
14–16% and the ratio settled at 1.10×. The measurement was dominated by its own
noise until it was built to say so.

### This reverses finding 35's conclusion

Finding 35 argued that a parse cache saving ~30% could not make blinker beat
ld64, because it would land at ~37 ms against 27.9 ms. On the corrected
baseline, blinker is at 31.0 ms against 28.3 ms — **2.7 ms behind, not 16.7**.
A cache saving even 15% would put blinker ahead of the linker it replaces, and
the M4/M5 work is viable rather than doomed.

The phase percentages in findings 35 and 36 should be re-measured through this
harness before anything is optimised against them, for the same reason: they
were produced by the instrument this finding discredits.

### What the harness now refuses to do

It aborts rather than timing a link that failed or produced an implausibly
small output — the failure mode that made attempt one report a 45% win for two
crashing programs. And it prints a note when the difference between the two
linkers is smaller than the observed spread, which it does for this result: at
1.10× with 14% spread, "blinker is slower" is a statement the data supports
only weakly.

The remaining honest claim is narrow and worth stating exactly: **blinker links
this workload in roughly the same time as ld64, while producing an image 2.25×
larger because it does not dead-strip.**

## 39. The baseline is ld-prime, not ld64, and not lld

Every comparison in findings 34 and 38 was labelled "ld64". That is wrong, and
the distinction matters because the three names are three different programs.

What `cc` actually invokes on this machine:

```
$ ld -v
@(#)PROGRAM:ld  PROJECT:ld-1267
```

That is **ld-prime**, Apple's linker rewrite shipped since Xcode 15 — parallel,
and the default for every `cc` and `rustc` link here. The classic `ld64` is
still installed, as a *separate binary* named `ld-classic`, and is no longer
what anything uses by default.

**lld is not installed at all** on this machine: `-fuse-ld=lld` is rejected as
an invalid linker name and neither `lld` nor `ld64.lld` is on `PATH`. No
measurement in this project has ever involved it.

Measured against all three, same 27-input link, 25 interleaved iterations:

```
  ld-prime      32.3 ms  sd 2.6   output  458 KB
  ld-classic    34.5 ms  sd 4.5   output  518 KB
  blinker       39.7 ms  sd 4.5   output 1031 KB
```

So blinker is being compared against the *faster* of Apple's two linkers, which
is the right baseline — it is what a real build uses. Choosing `ld-classic`
would have flattered blinker by about 7%.

The naming is corrected throughout rather than left as a harmless shorthand:
"we are 1.1× ld64" and "we are 1.2× ld-prime" are claims about different
programs, and the second is the one that is true.

### On lld

lld's Mach-O port is a plausible additional baseline and is often the fastest
linker on other platforms. It is absent here, so any claim about how blinker
compares to it would be unfounded. Installing it and adding it to
`scripts/bench.py` is a worthwhile next step; asserting anything about it now
would not be.

## 40. The profile, measured properly, and what it says about the cache

Findings 35 and 36 profiled the link from single runs. Re-measured through the
benchmark harness — 25 iterations, warmup discarded, standard deviations
reported:

```
blinker internal link: 30.1 ms median

  read+parse     8.1 ms   27.0%  sd 0.6
  resolve        8.3 ms   27.6%  sd 0.8
  layout         1.9 ms    6.5%  sd 0.2
  relocate       7.6 ms   25.1%  sd 0.9
  emit+sign      2.7 ms    8.9%  sd 0.3

  accounted     28.6 ms   95.1%
  unmeasured     1.5 ms    4.9%
```

Two corrections to what was previously recorded.

**Finding 36's "31% in untimed gaps" was an artifact.** `internal_link_ms` is
measured from before `LinkRequest::new`, which calls `default_stub_library()`,
which spawned `xcrun` — 14 ms charged to "work between the timers". The real
unmeasured remainder is 4.9%, which is process and allocator noise. The
repeated-traversal story built on that number was chasing something that was
not there, and the traversal collapse (finding 37) unsurprisingly measured no
improvement.

**The stage shares are stable and roughly equal.** Three phases —
`read+parse`, `resolve`, `relocate` — are within 9% of each other and are 80%
of the link between them. The standard deviations are under 1 ms, so these are
real distinctions rather than noise, which is more than could be said for any
earlier version of this table.

### What a cache is worth, on numbers that hold up

Against `ld-prime` at 32.3 ms and blinker at 39.7 ms on the same workload:

| cached through | saved | blinker becomes |
|---|---|---|
| parse | 27% | ~29 ms — **ahead of ld-prime** |
| parse + resolve | 55% | ~18 ms |
| parse + resolve + relocate | 80% | ~8 ms |

So finding 35's conclusion is now doubly wrong: it said a parse cache could not
close the gap, on a baseline that was itself mismeasured. A parse cache alone
is enough to pass ld-prime, and caching through relocation — which is what
findings 15–18's CGU-keyed design supports — is worth roughly 4×.

That is the first time this project has had a defensible estimate of what M4/M5
is worth, and it rests on an instrument whose failure modes are documented in
its own header.

## 41. The parse cache is dead: parsing is already faster than deserialising

M4 was specified as a persistent cache of parse results, and finding 40 put its
value at 27% — enough to make blinker the faster linker. Before building it,
the premise was tested: is deserialising a `ParsedObject` actually faster than
parsing the object again?

On a realistic input — a 9.4 MB member of `libstd.rlib`:

```
  parse Mach-O        0.43 ms
  deserialise JSON    7.51 ms   (17.4x parse)
```

JSON is a deliberately pessimistic codec, so the useful figure is the implied
one: a compact binary format runs 5–10× faster than JSON, which puts
deserialisation at **0.75–1.50 ms against 0.43 ms to parse**. A parse cache
would be **1.7–3.5× slower than the thing it replaces.**

### Why parsing is so cheap

blinker's parser is lazy by construction, and the reason is recorded in
`InputSection`'s own doc comment: section bytes are *addressed by offset, not
copied*, "so that `ParsedObject` stays small enough to cache". Parsing 9.4 MB
costs 0.43 ms because it walks load commands and the symbol table structurally,
over borrowed bytes, and copies almost nothing.

Deserialisation cannot do that. It must **allocate** every `String` and every
`Vec` the parser merely pointed at. The design decision that made parsing fast
is exactly the one that makes a parse cache pointless — and it was made, and
documented, long before anyone measured either.

### What this does to the 27%

`read+parse` is 27% of the link, but that measurement bundles two things.
Parsing is 0.43 ms per 9.4 MB; the rest is **reading 17 MB off disk**. A cache
cannot avoid the read unless it stores materially less than the original — and
a serialised `ParsedObject` is not smaller, it is 5.4 MB of JSON for a 9.4 MB
object, and a binary encoding would still carry every symbol name.

So the addressable cost is not parsing and not reading. It is `resolve` (28%)
and `relocate` (25%) — the stages that consume the parse rather than produce
it, and whose *outputs* are small: resolved addresses and patched section
bytes, not the inputs they came from.

### The corrected direction

Cache the **relocated output**, keyed by CGU identity per findings 15–18, and
rebuild only the contributions whose CGU changed. That is what the edit-class
analysis (bodies patch in place, additions append, cascades relink) was always
pointing at. M4-as-a-parse-cache should be struck from the plan rather than
implemented.

### The meta-point

This is the fifth premise in this project to survive reasoning and die to a
single measurement, and the first one that was tested *before* the code was
written rather than after. The experiment cost one example binary and two
commands. Implementing the cache first would have cost a crate, a codec
decision, a schema-versioning scheme, and a cache-invalidation story — all to
arrive at a linker that was slower.

## 42. Every section after the edit moves, so the cache needs slop or it is worthless

The corrected cache design (finding 41) stores *relocated output* keyed by
codegen unit and rebuilds only what changed. Its load-bearing premise is that
cached bytes stay valid — which requires the addresses they were relocated
against to stay put.

Tested with the smallest possible edit: adding `+ 0` to a function body in a
two-module Rust binary, changing no signature and no symbol name.

```
                  before              after
  __stubs         0x10008d90c    →    0x10008d930
  __const         0x10008e1e8    →    0x10008e210
  __gcc_except_tab 0x1000a0504   →    0x1000a052c
  __cstring       0x1000a2f74    →    0x1000a2f9c
  __unwind_info   0x1000a66c4    →    0x1000a66ec
  __eh_frame      0x1000af020    →    0x1000af048
```

**Every section after `__text` moved, by exactly 0x24 — 36 bytes.** The same
36 bytes finding 17 measured for this class of edit. `__text` itself did not
move only because it is first.

So a naive output cache is worth nothing. Any edit that changes a function's
size shifts every downstream address, and every cached relocated byte that
referenced one is stale. Finding 17 already established that *most* edits change
size — including "pure body" edits, which is the class the whole incremental
premise rests on.

### This is the padding trick, and it is not optional

The fix is to leave slop: pad each contribution so a function that grows by a
small amount consumes its own padding rather than displacing its neighbours.
Then a body-only edit rewrites one contribution's bytes in place and every
cached address downstream stays valid.

That turns the design question into two measurable ones, neither of which has
been answered:

1. **How much slop?** Finding 17's single data point is +36 bytes for a trivial
   edit. The distribution across real edits — not one example — decides whether
   slop costs 5% of image size or 50%.
2. **What happens when slop runs out?** An edit that outgrows its padding
   forces a relayout, which invalidates everything after it. The cache needs a
   defined behaviour there, and the honest one is to fall back to a cold link
   rather than to emit something subtly wrong.

### Why this had to be measured before the cache and not after

Building the cache first would have produced a correct-looking implementation
with a near-zero hit rate on exactly the workload it exists for — an agent
making one-line edits — and the failure would have looked like a tuning problem
rather than a design one. It cost two builds and a diff to learn instead.

That is the sixth premise in this project tested against reality rather than
argued about, and the second one tested *before* the code that depends on it.

## 43. Slop of about 5% covers every body edit measured

Finding 42 established that the output cache needs per-contribution padding or
it has a near-zero hit rate. The open question was how much. Measured across a
range of edits to a debug-profile Rust object (`opt-level=0`, which is what an
edit–build–test loop uses):

| edit | `__text` delta | of the contribution |
|---|---|---|
| `a + b` → `a + b + 0` | **+36** | 1.0% |
| add a branch | **+92** | 2.5% |
| `x * k` → `x * k + 1` | +44 | 1.2% |
| tighten a bounds check | +16 | 0.4% |
| add a `println!` | +52 | 1.4% |
| signature: add a parameter | +52 | 1.4% |
| additive: new unused function | +0 | 0.0% |
| struct: add a field | +0 | 0.0% |

Every body edit lands between **0.4% and 2.5%**, worst case 92 bytes. The +36
for `+ 0` reproduces finding 17 exactly, on a different project and a different
build path.

**Padding each contribution by ~5% of its size absorbs every edit measured**,
with the worst case at half the budget. That is a cheap price: blinker's images
are *already* 2.25× larger than the system linker's for want of dead-stripping
(finding 34), so 5% of slop is noise against a gap that is 125%.

### Two caveats this does not settle

- **Optimised builds behave differently.** At `-O` every one of these edits
  measured **+0**, because the optimiser deletes `+ 0`, and the small functions
  inline into their callers so the edit does not change a contribution at all.
  Slop is a debug-build concern, which is the right place for it — but a
  release build should not pay for padding it cannot use.
- **The two +0 rows are artifacts, not good news.** The new function was unused
  and removed; the struct field changed no code path. They are not evidence
  that additive and layout edits are free — findings 17 and 42 already showed
  otherwise for cases that are actually reached.

### What is now specified

The output cache can be designed against numbers rather than hopes: pad
contributions by 5%, invalidate on a CGU content-hash change (finding 15), fall
back to a cold link when an edit outgrows its padding, and expect that fallback
to be rare for the body edits that dominate an agent's edit loop.

What remains unmeasured is the *distribution over a real workload* rather than
eight hand-written edits — how often an agent's changes exceed 5%. That needs
the telemetry harness, and it is the last unknown before the cache is buildable.

## 44. 89% of codegen units are untouched by a one-function edit

`scripts/edit-class.py` classifies an edit by comparing two builds' object
files, keyed on the stable codegen-unit component of the filename (finding 15).
On a one-function change to a small Rust binary — replacing `n * 2` with a
branch:

```
  9 codegen units
    unchanged      8  (89%)
    additive       1  (11%)

  max growth +172.2%   over 5% slop budget: 1
```

**Eight of nine codegen units were byte-identical.** That is the cache's hit
rate for this edit, and it is the first direct evidence that the incremental
premise holds at all: the overwhelming majority of a build's object code does
not change when one function does.

The one that did change blew through the slop budget at +172%, and the reason
matters: that codegen unit is *tiny*. A branch costing ~90 bytes (finding 43)
is 2.5% of a 3.6 KB contribution and 172% of a 50-byte one. **Slop as a flat
percentage is wrong for small contributions** — it needs a floor, something like
`max(5%, 128 bytes)`, or small units need to be merged before padding.

It was also classified `additive` rather than `body`, because the branch
introduced new symbols — a panic landing pad. That is worth knowing: in Rust,
adding a branch to a function is frequently *not* a pure body edit at the
object level, which makes the "bodies patch in place" bucket narrower than the
source-level intuition suggests.

### What is now measured, and what is left

The cache design rests on four numbers, three of which are now in hand:

| question | answer | finding |
|---|---|---|
| can parses be cached? | no — parsing beats deserialising | 41 |
| do addresses stay put across an edit? | no — everything after it moves | 42 |
| how much slop does an edit need? | 0.4–2.5% of a contribution, floor needed | 43, 44 |
| how often does a real edit exceed it? | **unmeasured** — needs many edits | — |

The last one now has a tool. Running it across a realistic sequence of agent
edits is the remaining prerequisite, and the only one left before the cache can
be built against evidence rather than intuition.

## 45. A one-constant edit leaves 98% of codegen units untouched, inside budget

Running the classifier over a sequence of edits to a four-module Rust binary
(48 codegen units, most of them from `libstd`):

```
edit: change a constant (x * 2 -> x * 3)
  48 codegen units
    unchanged     47  (98%)
    body           1  (2%)
  max growth +0.0%   over 5% slop budget: 0
```

**47 of 48 codegen units byte-identical, and the one that changed grew by
nothing at all** — a body edit that fits its existing space exactly. For this
edit the cache would hit 98% of contributions and patch the last one in place
without touching layout.

That is the strongest evidence yet for the incremental design, and it is the
shape the whole project assumed but had never checked: the overwhelming
majority of a build's object code is `libstd` and dependencies that no source
edit touches.

### Caveat: this is two data points, not a distribution

The sequence was meant to cover five edit classes. Three of them never ran —
the shell harness split its edit descriptions on `:`, and three of the `sed`
expressions contain colons, so the labels corrupted the expressions and the
later generations built nothing. The tool behaved correctly on the runs that
executed; the *driver* was broken, and the empty "0 codegen units" output is
what exposed it.

Worth recording rather than quietly rerunning, because it is the same failure
as the first benchmark (finding 34): a harness that reports a number for work
that did not happen. The difference is that this one reported `0 codegen units`
instead of a plausible-looking percentage, so it could not be believed by
accident. Output that fails loudly is worth designing for.

### What is still needed

A driver that applies edits from separate files rather than inline `sed`, over
a longer and more varied sequence, on a project with real dependencies. Then
the last of the cache's four numbers — how often an edit exceeds its slop — has
a distribution behind it instead of a single reassuring sample.

## 46. The unwinding fault is in `__eh_frame`'s contents, located precisely

`panic=unwind` still faults, but the crash report now names the exact code:

```
  libunwind  LocalAddressSpace::getEncodedP
  libunwind  CFI_Parser::parseCIE
  libunwind  CFI_Parser::findFDE
  libunwind  UnwindCursor::setInfoBasedOnIPRegister
  libunwind  _Unwind_RaiseException
```

This is progress worth stating plainly. The unwinder **finds the FDE** — which
means `__unwind_info`, its DWARF-mode offsets, and the two-level index are all
being consumed correctly (findings 30, 32). It then follows the FDE to its CIE
and segfaults reading the CIE's encoded pointers.

So the fault is in `__eh_frame`'s **bytes**, not in the table that indexes them.
Everything above `__eh_frame` in the stack has now been verified against
ld-prime and works.

### The leading hypothesis, labelled as one

blinker **concatenates** each object's `__eh_frame` and aligns each contribution
to its declared alignment. CFI records are self-delimiting — each begins with
its own length, and **a zero length terminates the section**. Alignment padding
inserted *between* two objects' contributions is a run of zero bytes, which a
CFI walker reads as a terminator or, worse, as a record with a nonsense length.

ld-prime does not concatenate. It parses `__eh_frame`, deduplicates CIEs, and
rewrites every FDE's CIE back-pointer — which is why its `__eh_frame` is a
single coherent chain and blinker's is several chains with gaps.

This is a hypothesis. This project has recorded five that survived reasoning
and died to measurement (13, 33, 35, 36, 41), so it is written down as
something to test, not to act on. The test is cheap: dump blinker's
`__eh_frame` and walk the records by length, checking whether the chain reaches
the end of the section or hits a zero-length record partway.

### Why this matters for sequencing

`panic=unwind` is the default for `cargo build`. Until it works, blinker cannot
link a normal Rust project, and the incremental work — however fast — has
nothing usable to be fast *on*. This should be finished before step 2 of the
incremental design.

## 47. The `__eh_frame` chain is intact; the fault is inside a CIE's encoded pointers

Finding 46's hypothesis — that alignment padding between concatenated
`__eh_frame` contributions creates zero-length records that break the CFI
chain — is **wrong**. Walking both binaries' records by length:

```
  blinker:  23 CIEs, 1408 FDEs, 0 zero-length, 0 malformed, walked to 0x16cb0/0x16cb0
  ld-prime: 20 CIEs,  375 FDEs, 0 zero-length,              walked to  0x6920/0x6920
```

blinker's chain reaches the **exact end** of the section with no gaps and no
malformed records. Concatenation is not the problem, and rewriting
`__eh_frame` to deduplicate CIEs would have fixed nothing.

Sixth hypothesis in this project refuted by measurement. It cost one script.

### Where the fault must be

libunwind crashes in `getEncodedP` while parsing a CIE — reading a pointer
encoded according to that CIE's augmentation string. The chain is intact, so it
is reading the right CIE; the *pointer inside it* is bad.

A CIE's augmentation data carries the **personality routine**, and DWARF
pointer encodings include `DW_EH_PE_indirect`, which means the encoded value is
the address of a slot *containing* the personality address — a GOT entry — not
the personality function itself. Dereferencing a function address as though it
were a pointer slot reads instruction bytes as an address and jumps into
nothing.

That is the same defect already fixed once, in a different place: finding 41's
neighbour, where `__unwind_info`'s personality array had to point at a GOT slot
rather than at the function. The `__unwind_info` side was corrected; the CIE's
own personality pointer inside `__eh_frame` was not, because it is patched by
the generic relocation path, which resolves symbols to their addresses.

**Recorded as the next thing to test, not as a conclusion.** The check is to
decode one CIE's augmentation string, read its personality encoding byte, and
compare the value blinker wrote against the address of that symbol's GOT slot.

## 48. Confirmed: the CIE's personality points at the function, not its GOT slot

Decoding the first CIE's augmentation data from both linkers' output for the
same program:

```
blinker:   aug='zPLR' personality enc=indirect|pcrel|sdata4  ->  0x10017d873
ld-prime:  aug='zPLR' personality enc=indirect|pcrel|sdata4  ->  0x100044000
```

Both use `DW_EH_PE_indirect`, which means the value is the address of a **slot
containing** the personality routine's address. ld-prime's resolves to
`0x100044000` — eight-byte aligned, in `__DATA_CONST,__got`. blinker's resolves
to `0x10017d873`, an **odd address**, which cannot be a pointer slot at all.

libunwind dereferences it as a pointer, reads unaligned bytes as an address,
and segfaults in `getEncodedP` — exactly the crash in finding 46.

This is the same defect fixed once already in `__unwind_info`'s personality
array, which had to name a GOT slot rather than the function. That side was
corrected; the CIE's own personality pointer inside `__eh_frame` was not,
because it is patched by the generic relocation path, and that path resolves a
symbol to *the symbol's address*. For an indirect encoding, the right answer is
the address of the symbol's GOT entry.

### A methodology note on how this was nearly missed

The first run of this comparison reported blinker and ld-prime producing
**identical** values, which would have refuted the hypothesis. They were
identical because both decodes ran on the same file: an earlier command had
rebuilt `target/debug/rs2` with the default linker, so the "blinker" decode was
reading ld-prime's output.

The tell was that both reported `__eh_frame` at the same address, for binaries
known to differ in size by 2.25×. Two independent measurements agreeing exactly
is evidence of a harness fault more often than of a real result — the same
lesson as the benchmark that timed two crashing programs (finding 34).

### The fix

When patching a relocation whose field is a CIE personality reference with an
indirect encoding, target the symbol's GOT slot rather than its address —
allocating one if it does not already exist, as is already done for
`GOT_LOAD_PAGE21` and for `__unwind_info`'s personality array.

## 49. The personality fix is inert, because the personality set is empty

Finding 48 identified the bug exactly: a CIE's personality is encoded
`DW_EH_PE_indirect` and must name a GOT slot, and blinker writes the function's
address. The fix routes any `__eh_frame` relocation targeting a *known
personality symbol* through the GOT.

It changed nothing. The personality still resolves to the same odd address.

The reason is already written down, four findings earlier. Finding 31:

> In DWARF mode the personality and the LSDA live in the CIE and FDE
> respectively, not in the compact record — so there are no personality or LSDA
> *relocations* to find in `__compact_unwind`.

blinker's personality set is collected from `__compact_unwind`, and in DWARF
mode that section contains none. The set is empty, so the new branch never
fires. I built a mechanism keyed on information this project had already
recorded as absent.

### What identifying the personality actually requires

The personality is named only by the CIE's own augmentation data, so finding it
means doing what the decoder in finding 48 does — inside the linker:

1. Walk `__eh_frame`'s records to find each CIE.
2. Parse its augmentation string; if it contains `P`, read the encoding byte.
3. The field that follows is the personality reference; the relocation at that
   offset names the symbol.
4. If the encoding has `DW_EH_PE_indirect`, patch with the symbol's GOT slot.

The CFI walker for step 1 already exists — `eh_frame_fde_offsets` walks exactly
these records to find FDE boundaries. What is missing is augmentation parsing,
which the throwaway script in finding 48 does in about thirty lines.

### On leaving the code in

The inert branch is kept rather than reverted: it is the correct handling once
the set is populated, and removing it would mean writing it again. But it is
**not a fix**, and the tests do not claim it is — `panic=unwind` still faults,
and nothing new passes because of this change.

## 50. The object already says the CIE personality is a GOT pointer

Finding 49 concluded that identifying the CIE personality required parsing the
augmentation string. I wrote that parser — ULEB/SLEB decoding, augmentation
walking, encoding bytes — wired it in, and the link then failed with:

```
object 2: cannot apply PointerToGot:
  ARM64_RELOC_POINTER_TO_GOT at 0x1000aeffb needs an indirect address
```

The relocation on that field is **`ARM64_RELOC_POINTER_TO_GOT`**. The object
states outright that the field holds a pointer to a GOT entry. No augmentation
parsing was ever needed to identify it — the same lesson as finding 32, where
an FDE's function came from a relocation rather than from decoding a DWARF
pointer encoding, and I did not apply it here.

The failure was my own new branch bypassing the `got` context that
`PointerToGot` requires. Removing it restores the previous behaviour: the link
succeeds and `panic=unwind` still faults.

### Where the bug must now be

blinker already supports `ARM64_RELOC_POINTER_TO_GOT`, and it is reached — the
link would fail otherwise. So the fault is in *what that relocation produces*,
not in recognising the field:

- the GOT slot allocated for the personality symbol may be the wrong one, or
- the encoding is `pcrel|sdata4`, so the field wants
  `got_slot - field_address` as a 32-bit signed value, and the arithmetic in
  `apply` for this kind may not match what a CIE expects.

blinker writes `0x10017d873` where ld-prime writes `0x100044000`. The value is
odd, so whatever is being written is not a slot address at all.

### What is kept and what is not

The augmentation parser is retained but no longer decides patching. It is used
for one thing that is still correct and still needed: **ensuring a GOT slot
exists** for symbols referenced as personalities. It is dead weight if the
`PointerToGot` path turns out to allocate those slots itself, and the next
session should check that before keeping it.

Two attempts at this fix, both aimed at identifying the field, when the field
was never the problem. The evidence that it was not — a relocation kind whose
name is literally `POINTER_TO_GOT` — was in the error message the first attempt
produced.

## 51. `POINTER_TO_GOT` ignores the PC-relative flag, which is the whole bug

One grep, after two turns of building machinery aimed elsewhere:

```rust
PointerToGot => {
    let got = context.got.ok_or(...)?;
    write_scalar(bytes, offset, got, length)
}
```

It writes the GOT slot's address **absolutely**. A CIE's personality field is
encoded `indirect|pcrel|sdata4` — it needs **`got - place`**, as a signed
32-bit value. `apply()` is never given the relocation's `pc_relative` flag, so
it cannot tell the two forms apart and always emits the absolute form.

That is exactly the observed corruption. blinker writes a value which, decoded
as PC-relative by libunwind, lands on `0x10017d873` — odd, therefore not a slot
address, therefore a segfault when dereferenced.

`InputRelocation` has carried `pc_relative` since the parser was written. The
relocation engine's other kinds encode PC-relativity in the kind itself
(`BRANCH26`, `PAGE21` are always relative), so the flag was never plumbed
through — and `POINTER_TO_GOT` is the one kind that appears in both forms.

### The fix

`Context` gains the flag, or `apply` takes it, and `PointerToGot` writes
`got.wrapping_sub(place)` when set. Every other kind is unaffected because
their relativity is implied by the kind.

Worth a test that pins both forms, since the absolute form is presumably in use
somewhere and would otherwise silently break.

### Three turns, and the answer was one grep away

Finding 48 located the bad value. Finding 49 blamed the personality set and
built against data known to be absent. Finding 50 built an augmentation parser
that the relocation kind made unnecessary — and the error message that attempt
produced *named the relocation kind*. Reading the implementation of the kind
already known to be involved would have found this immediately, and it is what
"start by dumping what `apply` computes" should have meant on the first pass
rather than the fourth.

## 52. Fixed: `POINTER_TO_GOT` now honours PC-relativity, and unwinding gets much further

The fix from finding 51, applied: `Context` carries `pc_relative`, and
`PointerToGot` writes `got - place` when it is set. Five lines, plus tests
pinning both forms.

The CIE's personality pointer is now correct:

```
  before:  raw=0xce878   -> 0x10017d873   (odd — not a slot address)
  after:   raw=0x1f87d   -> 0x1000ce878   (8-byte aligned — a real slot)
```

And the failure changed character entirely:

```
  before:  SIGSEGV inside libunwind's CIE parser
  after:   thread 'main' panicked at src/main.rs:1:33:
           boom
           fatal runtime error: failed to initiate panic, error 3, aborting
```

libunwind no longer crashes. It parses the CIE, finds the personality, runs
phase 1 of the unwind, and *reports a failure* — error 3 is
`_URC_FATAL_PHASE1_ERROR`, the search phase failing to find a handler. A clean
diagnosable error where there was a segfault.

### Why the flag was missing for so long

Every other ARM64 relocation kind implies its own relativity: `BRANCH26` and
`PAGE21` are always PC-relative, `UNSIGNED` never is. `POINTER_TO_GOT` is the
only kind that appears in both forms — absolute in a pointer table,
PC-relative in a CIE — so it is the only one for which the flag on
`InputRelocation`, present since the parser was written, ever mattered.

Three tests now pin it: the absolute form, the PC-relative form, and a
backwards displacement, since a slot below the field must survive truncation to
four bytes as two's complement.

### What remains

Phase 1 failing means the unwinder cannot find a handler for the frame. The
next candidates are the LSDA pointer in the FDE — encoded the same indirect way
and reached by the same relocation machinery — and the FDE's address range,
which decides whether the unwinder believes the frame is covered at all.

`panic=unwind` still does not work. But it has gone from "segfault in a CIE
parser" to "the unwinder ran and could not find a handler", and each of those
is a different and much smaller problem.

## 53. The FDEs are correct too, so the remaining suspect is the LSDA

Decoding the first FDEs from both linkers' output, resolving each PC-begin
through its CIE's `R` encoding:

```
blinker:   __text 0x100000900..0x10008d87c
  FDE@0x14  pc=0x100000988 range=0x28  IN __text
  FDE@0x48  pc=0x1000009e4 range=0x20  IN __text
  FDE@0x94  pc=0x100000a50 range=0x4c  IN __text

ld-prime:  __text 0x100000888..0x10003507c
  FDE@0x14  pc=0x1000008f4 range=0x28  IN __text
  FDE@0x48  pc=0x10000091c range=0x20  IN __text
  FDE@0x94  pc=0x10000093c range=0x4c  IN __text
```

Same offsets, same ranges, every PC inside `__text`. The FDE table is right.

So of the pieces the unwinder touches, these are now verified against ld-prime:
`__unwind_info`'s index and DWARF offsets (findings 30, 32), the `__eh_frame`
record chain (47), CIE personality pointers (52), and FDE address ranges
(here). Phase 1 still fails.

**The remaining candidate is the LSDA** — the language-specific data area that
phase 1 reads to decide whether a frame has a handler. It lives in the FDE's
augmentation data, encoded per the CIE's `L` byte, and points into
`__gcc_except_tab`. A wrong LSDA pointer is exactly a phase-1 failure: the
unwinder finds the frame and the personality, calls it, and the personality
cannot locate its action table.

The check is the one that worked for the personality: decode the FDE's
augmentation, resolve the LSDA pointer, and compare it against ld-prime's and
against `__gcc_except_tab`'s bounds. If it lands outside that section, the
relocation patching it is wrong in the same way `POINTER_TO_GOT` was.

### Method note

Four of the five verified pieces were confirmed by decoding both linkers'
output and comparing, rather than by reasoning about the format. That approach
has now found two real bugs (52, and the section ordering in 28) and cleared
four suspects, at roughly one script each. It is the only technique in this
project with a positive record.

## 54. Found: LSDA pointers land outside `__gcc_except_tab`

Decoding each FDE's LSDA through its CIE's `L` encoding and checking it against
the section it must point into:

```
blinker:   __gcc_except_tab 0x1000a047c..0x1000a2eec
  FDE@0x94    lsda=0x1000a0529   in __gcc_except_tab
  FDE@0x9bc4  lsda=0x1000a9e65   *** OUTSIDE ***
  FDE@0x9c20  lsda=0x1000a9eed   *** OUTSIDE ***
  FDE@0x9c78  lsda=0x1000a9f81   *** OUTSIDE ***

ld-prime:  __gcc_except_tab 0x100039b88..0x10003ad20
  every LSDA inside
```

That is a phase-1 failure exactly: the unwinder finds the frame, calls the
personality, and the personality reads its action table from an address roughly
0x7000 past the end of the section that holds them.

**The first LSDA is correct and the later ones are not.** That pattern is the
tell — the first contribution to `__gcc_except_tab` starts at the section base,
so an error in the per-contribution offset is invisible there and grows for
every object after it.

### The likely cause, consistent with finding 30

`__eh_frame`'s LSDA references are `SUBTRACTOR` pairs, and in Mach-O the
**addend for these is stored inline in the patched field**, not in the
relocation entry. Finding 30 established exactly this for `__compact_unwind`
and had to read the inline value by hand; the generic relocation path passes
`pair.addend` from the parsed relocation, which for these is zero.

The FDE PC-begin fields are also `SUBTRACTOR` pairs and came out *correct* —
because their addend genuinely is zero: the target is the function symbol
itself. The LSDA's target is `__gcc_except_tab` **plus an offset**, so its
addend is non-zero, and dropping it produces precisely this failure.

If that holds, the fix belongs in `apply_relocations`' pair path — read the
inline value from the field and rebase it out of the input section's coordinate
space, the same correction finding 30 applied to compact unwind — and it will
fix every `SUBTRACTOR` pair with a non-zero addend, not just the LSDA.

**Recorded as the leading explanation, not a conclusion.** The evidence for it
is strong (the first-correct-then-wrong pattern, and the exact precedent in
finding 30), but this project has six hypotheses that died to measurement. The
test is to dump one LSDA relocation's inline field value and check whether it
equals the missing displacement.

### Where this leaves the unwinding chase

Every piece the unwinder touches has now been decoded and compared against
ld-prime: `__unwind_info` (30, 32), the `__eh_frame` chain (47), CIE
personalities (52, fixed), FDE ranges (53), and now the LSDA — which is the one
that is wrong.

## 55. The inline-addend explanation for the LSDA is wrong

Finding 54's leading explanation — that LSDA references are `SUBTRACTOR` pairs
whose inline addend the generic relocation path drops — was implemented and had
**no effect whatsoever**. Not a smaller error, not different addresses:

```
  before the fix:  LSDAs inside 6, outside 350   (first bad one at 0x1000a9e65)
  after the fix:   LSDAs inside 6, outside 350   (first bad one at 0x1000a9e65)
```

Byte-identical output means the changed code path never ran for these
relocations. Whatever patches an FDE's LSDA field, it is not the `SUBTRACTOR`
pair path.

The change was reverted rather than kept. It touches every `SUBTRACTOR` pair in
every link, and an unproven modification to the relocation engine is not worth
carrying on the theory that it might be right somewhere else.

### What the numbers actually say

Six LSDAs are correct and 350 are wrong. That is a far more useful ratio than
"the first is right" from finding 54 — six correct suggests one object's
contributions land correctly and every other object's do not, which still
points at a per-contribution offset, but through some path other than the pair
handler.

The next step is not another hypothesis. It is to find **which relocation
actually writes an LSDA field**: take one known-bad FDE, compute its LSDA
field's offset within its input `__eh_frame` section, and look up what
relocation exists at that offset — kind, target, addend, pc_relative. That is
the same "read what the object says" move that finding 51 should have started
with, and it has not been done for the LSDA.

### Seventh hypothesis, seventh refutation

The pattern is now unambiguous enough to state as a rule for this project: an
explanation that fits the evidence is worth roughly nothing here until it is
tested, and the test is almost always cheaper than the implementation. This one
cost a revert; finding 41's cost nothing because it was tested first. The
difference between those two outcomes is entirely in the order of operations.

## 56. The LSDA relocation, read at last

Reading what actually sits at an LSDA field's offset in a real `libstd` object,
instead of proposing an explanation:

```
FDE@0x99c4  LSDA field at section offset 0x99dd, encoding 0x10 (pcrel, 8 bytes)

relocations at that offset (__TEXT,__eh_frame):
  address   pcrel length extern type            symbolnum
  000099dd  0     3      1      1  SUBTRACTOR   1788
  000099dd  0     3      1      0  UNSIGNED       26
```

So they **are** `SUBTRACTOR` pairs, and finding 54's structural guess was
right — which makes finding 55's null result the interesting part. The pair
path *is* what patches these fields, yet reading the inline addend changed
nothing, which means the inline value is already zero and the addend was never
the problem.

The decisive detail is `extern=1` on both halves: the target is **symbol 26**,
not a section. These are symbol-relative pairs — `value = symbol26 -
symbol1788` — where symbol 26 is presumably a local label inside
`__gcc_except_tab` and 1788 is the FDE's own anchor.

That reframes the whole thing. A per-contribution offset error was the theory
because six LSDAs are right and 350 wrong; but if the targets are *local
symbols*, the suspect is local symbol resolution, which finding 29 already
found broken once — pointer-table slots were looked up under the linker's
synthetic object id and could not see locals, leaving them null.

`AddressMap` keys locals by `(object, name)`. Whether `target_address` reaches
them correctly for these relocations is the next thing to check, and it is a
check, not a hypothesis: resolve symbol 26 for this object and compare the
address against `__gcc_except_tab`'s bounds.

### 264 FDEs with an LSDA in one object

Worth recording for scale: this single `libstd` codegen unit has 264 FDEs
carrying LSDAs and 2361 relocations in `__eh_frame` alone. The six-correct,
350-wrong split across the whole link is consistent with one object resolving
and the rest not.

## 57. The LSDA's operands are same-named local labels in every object

Resolving the two symbols the LSDA relocation names:

```
symbol   26: 'GCC_except_table5'  (__TEXT,__gcc_except_tab)  value=0x4ea04
symbol 1788: 'ltmp18'             (__TEXT,__eh_frame)        value=0x828e80
```

Both are **local labels**, and the LSDA value is `GCC_except_table5 − ltmp18`.

The name is the point. *Every* object in the link defines its own
`GCC_except_table5`, `GCC_except_table6`, and so on, and its own `ltmpN`
labels. They are local precisely so that repetition is legal. Resolving one to
the wrong object's definition gives an address in the right *section* but the
wrong *contribution* — which is exactly the observed failure: six LSDAs land
inside `__gcc_except_tab` and 350 land past its end.

`AddressMap` was built for this: locals are keyed `(object, name)` and looked
up against the object that referenced them, because "two objects may
legitimately define the same local name". Finding 29 added that after null
`__thread_ptrs` slots traced to looking locals up under the linker's synthetic
object id.

So the machinery exists and the failure looks like exactly what it was built to
prevent. The check is narrow: for one known-bad FDE, resolve
`GCC_except_table5` through `AddressMap` with that FDE's owning object, and
compare against both `__gcc_except_tab`'s bounds and the address ld-prime
produced.

Two candidates worth distinguishing when someone runs it:

- the lookup falls through to the **global** map, where one arbitrary object's
  `GCC_except_table5` won, or
- the per-object key is right but the *contribution* offset used to place it is
  not — `address_map` computes `chunk + value − input.vm_address`, and
  `saturating_sub` there silently yields zero if the section's recorded address
  ever exceeds the symbol's.

### State of the unwinding chase

`__unwind_info` (30, 32), the `__eh_frame` chain (47), CIE personalities (52,
fixed), and FDE ranges (53) are all verified correct against ld-prime. The
LSDA is wrong, its relocation has been read (56), and its operands are now
identified. That is as far as this stretch got, and the next move is a lookup,
not a theory.

## 58. Fixed: `SUBTRACTOR` pairs dropped their addend, and Mach-O keeps it inline

`panic=unwind` works. The cause was not any of the four structures the unwinder
reads — all of them decoded correctly all along.

Mach-O relocation entries **have no addend field**. The value is written into
the bytes being patched, and the linker is expected to read it back out.
`InputRelocation::addend` comes from the `object` crate's `addend()`, which for
Mach-O correctly reports `0` on every relocation, because the addend is
implicit. `apply_pair` was adding that zero.

For an ordinary relocation dropping the addend would be a small error. For a
pair it is not, because of what the subtrahend is:

```
obj  17 off 0xebb9  sub ltmp18 = 0x1000afee8  min GCC_except_table1201 = 0x1000a2f1c
                    place 0x1000beaa1  reloc_addend 0  inline -60345  want -60345
```

`ltmp18` is the **anchor label at the start of that object's `__eh_frame`
contribution**, not a per-site label — so `minuend - subtrahend` is a distance
measured from the start of the contribution, while the field's `DW_EH_PE_pcrel`
encoding wants it measured from the field. The inline value is exactly that
gap. Across the whole link, all **1783** pairs agreed:

```
1783 with reloc_addend == 0, inline != 0, and inline == the needed correction
```

1783 of 1783. Not a trend — an identity.

### Why finding 55's null result was wrong

Finding 55 implemented this and reported byte-identical output, then reverted
it and recorded a refutation. The refutation was false. Nothing here contradicts
finding 55's *reasoning*; the measurement it rested on was simply not measuring
the build it thought it was — the same failure mode as the two decodes that read
one file in finding 47. A negative result from an unverified binary is not a
negative result.

The lesson is narrower than "test your hypotheses", which finding 55 already
said. It is: **a null result needs the same proof of provenance as a positive
one.** A change that does nothing and a change that never ran look identical
from the outside, and only one of them is evidence. The cheap guard is a
negative control — deliberately break the fix and confirm the test goes red —
which is what was done here:

```
with the addend:     6 passed
without the addend:  4 passed, 2 failed   (the two new unwind tests)
```

### The gap is only in the pair path

Checked rather than assumed. Counting every non-pair relocation whose patch
site holds a non-zero value, across a link spanning all of libstd's rlibs:

```
8112 Branch26   symbol  len=Word
4507 PageOff12  symbol  len=Word
4462 Page21     symbol  len=Word
 74  GotLoadPage21 / GotLoadPageOff12
 46  TlvpLoadPage21 / TlvpLoadPageOff12
 17  PointerToGot
```

Every one is an instruction-word kind, where those bytes are the instruction
and not an addend at all. `Unsigned` does not appear, and neither does any
section-targeted relocation: zero occurrences of either. So no non-pair
relocation in this corpus carries an implicit addend, and the fix stays scoped
to `apply_pair`.

`ARM64_RELOC_ADDEND` (r_type 10) is still refused with an error rather than
mis-applied, which is why an explicit addend cannot silently go missing.

### What now works

Both profiles, matching ld-prime exactly — same stdout, same exit status, and
the same symbolized backtrace frame for frame:

- caught panics, with `Drop` cleanup running innermost-first once per frame
- payload downcast through `catch_unwind`
- uncaught panics exiting 101 rather than aborting
- `RUST_BACKTRACE=1` walking the full stack through `catch_unwind`

Two regression tests cover it, and both were confirmed to fail without the fix.

## 59. The output cache passes the test the parse cache failed

Finding 41 killed the parse cache by measuring its premise: deserialising a
`ParsedObject` is 1.7–3.5× *slower* than parsing the object, because it must
allocate every `String` and `Vec` the borrowing parser merely pointed at.

The replacement design caches **relocated section bytes**. Same test, opposite
answer:

```
  recompute (the relocate stage)   7.3   ms
  load 1.03 MB from a warm cache   0.065 ms      112x
```

The reason is the reason finding 41 gave, inverted. Patched bytes have no
structure to rebuild: they deserialise into a `Vec<u8>` — one allocation and a
copy. The property that made parsing cheap is the same property that makes
*this* artifact cheap to reload, and it is the presence or absence of interior
structure that decides which way the comparison goes.

Worth stating as the general rule, since this project has now measured it in
both directions: **a cache is worth building only when the artifact is flatter
than the computation that produced it.**

## 60. Content-hashing every input costs exactly what the cache saves

The cache's validity rests on content hashes, never paths (finding 15). So the
next premise to test, before wiring anything: what does hashing cost?

```
  blake3 over all 56 inputs, 17.2 MB:   7.28 ms   (2.48 GB/s)
  the relocate stage it would replace:  7.3  ms
```

Net zero. Not marginal — the same number to two significant figures. A cache
validated this way would have been built, measured, and found to be exactly as
fast as no cache at all.

That is the third time in this project that a cache design has been killed or
reshaped by one measurement taken before the code (findings 41, 43, and this),
and the first where the fatal cost was in the *validation* rather than in the
storage.

## 61. Two input classes need two keys, and that makes the cache viable

Finding 60 does not kill the cache; it says the key is wrong for most inputs.
Finding 15's argument — that rustc's object filenames carry a per-build session
component, so a path-keyed cache has a 0% hit rate — is true of rustc's *own*
codegen output and of nothing else. Splitting the inputs by that property:

```
  37 .o    rustc per-build codegen units    0.31 MB    1.8% of bytes
  19 .rlib toolchain libraries             16.87 MB   98.2% of bytes
```

The bytes are almost entirely toolchain rlibs, whose paths are not merely
stable but *already content-addressed*: rustup names them
`libstd-4f24f0876fd27385.rlib`, hash included. They cannot change under a fixed
path. Path, mtime and size are a sound key for them.

Only the 1.8% that rustc rewrites every build needs hashing:

```
  blake3 over the 37 .o files only:  0.16 ms
```

**0.16 ms to validate, against 7.3 ms saved.** 45×, where the single-key design
was 1.0×. The whole difference is in noticing that "inputs" is not one
population.

This also explains why finding 60's number looked so much like a coincidence.
It is not one: both quantities scale with the same 17 MB, so hashing everything
was always going to land near the cost of relocating everything. The ratio only
moves once the key stops touching bytes that cannot have changed.

## 62. Writing the cache costs 1.9 ms, so it must not run when nothing reads it

The recording pass — noting which addresses each object read, and hashing their
names — is not free:

```
  relocate, recording off   7.4 ms   (25.3 ms link)
  relocate, recording on    9.2 ms   (27.6 ms link)
```

1.9 ms, about 25% on top of the stage. That is a fine price for producing an
artifact worth 7.3 ms on the next link, and pure waste on a link that will not
write one — which is every link in the test suite and every link with no cache
configured.

It was measured only because the profile was re-run after wiring, and the first
version had no flag: the cost was being paid unconditionally. A stage that gets
1.9 ms slower is invisible without a before-and-after, and the "before" existed
only because finding 40 had already established the baseline properly.

Recording is now gated on a cache path actually being requested, and the
profile is back to 7.4 ms.

### What the number implies for the design

The recording cost scales with distinct references, and the saving scales with
relocations. Their ratio is what makes the cache worth writing even on a link
that never gets reused, provided reuse happens more than about a quarter of the
time — which for an edit-compile-run loop it overwhelmingly does.

## 63. A negative control found a test that could not fail

The five cache tests all passed on the first run, which is a reason to check
them rather than to trust them. Deliberately breaking the linker three ways:

```
  record no dependencies         -> deps test FAILED        (has teeth)
  cache unrelocated bytes        -> bytes test FAILED       (has teeth)
  give every entry every range   -> ranges test passed      (does not)
```

The third was a real hole. The range test swept for overlaps by comparing each
claim with the next one *in the order they were recorded*, so when both entries
claimed the identical set of ranges, the two copies of each range were never
adjacent and never compared. Sorting before the sweep fixed it, and the control
then failed as it should.

This is the same lesson as finding 58 from the other direction. There, a null
result was believed without checking that the code ran; here, a passing test
was nearly believed without checking that it could fail. Both are the same
missing step — **confirm the experiment can produce the other answer** — and it
costs one deliberate break.

## 64. The cache reused nothing on a real link, and had no symptom

The reuse path was wired, all ten tests passed, and the C fixture reused both
objects. On the real 47-object Rust link it reused **zero**, and nothing said
so: the binary was byte-identical, the link succeeded, and the only trace was
that `relocate` had gone *up* rather than down.

```
  no cache          relocate  7.8 ms
  cache, no reuse   relocate 10.6 ms     <- the recording cost, no saving
  cache, reusing    relocate  4.9 ms
```

Instrumenting the plan showed every condition passing:

```
  previous entries 47   addresses 7121 -> 7121   changed 0
```

47 entries matched, no address moved, and no object was reused. The failure was
past all three conditions, in the byte copy: a **zero-filled section** has
contributions with a real length and no bytes on either side, and the copy read
that as the two layouts disagreeing and refused.

*Corrected by finding 66:* exactly **one** object failed the copy, not most of
them — and it alone accounted for half the relocate time, because it is the
libstd codegen unit that holds most of the link's relocations. The "0 of 47"
above was itself wrong, produced by the miscounting finding 66 describes.

### The symptom that did not exist

This is a new failure class for this project. Every previous bug produced a
crash, a wrong answer, or a number that moved the wrong way in a benchmark. This
one produced a *correct binary* and a cache that did nothing, and it survived a
test suite that was written specifically for it.

What eventually caught it was printing a count that had never been printed —
`reused_objects` was a field on `LinkTimings` that nothing displayed. The
general form: **a cache needs a hit-rate counter surfaced by default**, because
correctness tests cannot see the difference between a cache that works and a
cache that is switched off, and that is precisely the difference it exists to
make.

### The test for it

The C fixture written to cover this passes with the fix reverted, and is kept
only for the weaker property it does check. The guard is
`crates/cli/tests/rust_cache.rs`, which replays a captured Rust link twice and
requires *every* object to be reused. It fails with the fix reverted, but only
after finding 66 — until the counter was honest, it could not have.

## 65. Tentative (common) symbols are not resolved

Found incidentally: a C file-scope `int arr[4096];` with no initialiser is a
*tentative* definition, which the assembler emits as a common symbol rather
than a `__bss` definition. blinker reports it undefined:

```
UndefinedSymbols { names: ["_big_uninitialised"] }
```

`static int arr[4096];` links fine, because file-local storage is a plain
`__bss` definition with no tentative-definition rules to apply.

Rust never emits these, which is why nothing had hit it before, and C code that
does is common enough that this is a real gap rather than a curiosity. Recorded
where it was found rather than fixed in passing.

### Fixed

Two parts, and the first was not where it looked.

`object`'s `is_common()` returns **false** for a Mach-O common symbol. The
format has no distinct type for one: it is `N_UNDF | N_EXT` with a non-zero
`n_value` holding the size, which is the same encoding as an undefined
reference plus a number. A trait method written against formats that *do* have
a type for it cannot see that, so the test is now made directly — undefined,
external, non-zero value — and it has to run *before* `is_undefined()`, which
would otherwise claim the symbol first.

Then the storage: commons are collected across all objects, deduplicated by
name at the largest size requested, dropped entirely if any object defines the
name outright, and placed in a synthesised zero-filled `__DATA,__common`
section sized before layout like `__got` and `__stubs`. `address_map` cannot
see them — it walks each symbol's defining section and a common has none — so
their addresses are inserted after layout.

Matches ld-prime on shared commons, mismatched sizes, and a real definition
overriding both.

## 69. Two of the six new tests could not fail

The common-symbol tests all passed on the first run, so both obvious
breakages were tried against them:

```
  allocate the last size seen instead of the largest  ->  all 6 passed
  let commons shadow a real definition                ->  all 6 passed
```

Neither was caught, for the same reason in different clothes: **the wrong
answer and the right answer were the same number.**

- The size test wrote 256 integers and read one back. An undersized allocation
  still writes somewhere and still reads back what it wrote — `__common` had
  slack after it, so the overrun was invisible. It needs a *canary*: a second
  common that the section's name ordering places immediately after, which the
  overrun destroys.
- The override test linked three objects and checked the final answer, which is
  42 whether the references resolve to the real object or to freshly zeroed
  common storage. It needs the initialiser to be **observed** — read `111` out
  of the definition before anything writes to it.

Both now fail under their controls.

This is the third time in this session that a negative control has found a
hollow test (63, 66, this). The pattern across all three is worth stating: a
test proves something only when the failure it is written against would produce
a *different observable value* — not merely a different internal state. Writing
the assertion first and the breakage second is what makes that hard to see, and
running the breakage is what makes it obvious.

## 66. The counter added to detect a dead cache reported a dead cache as healthy

Finding 64's conclusion was that a cache needs its hit rate surfaced, because a
cache that stops working is invisible. The counter was added. It lied.

Reverting the zero-fill fix, to check that the new test could catch it:

```
  with the fix     reused 47/47 objects   relocate  4.7 ms
  without the fix  reused 47/47 objects   relocate 10.1 ms
```

Identical hit rate, and the link doing twice the relocation work. Every test
asserting on that counter passed, including the one written specifically to
catch this bug.

The cause is a one-word distinction: `reused_objects` was taken from
`plan.entries.len()` — the objects the plan *selected* — and the plan is built
before any copy is attempted. When a copy failed, the object fell through to a
full relocation and the plan still counted it. The counter measured intent.

Counting at the copy instead:

```
  with the fix     reused 47/47 objects   relocate  4.8 ms
  without the fix  reused 46/47 objects   relocate  9.8 ms
```

One object, and half the relocate time — it is the libstd codegen unit that
holds most of the link's relocations, which is also why "reuse 46 of 47" is a
catastrophic result rather than a good one. **A hit rate by object count is the
wrong unit** when object sizes span three orders of magnitude. Reporting the
share of relocations skipped instead, which is what the cache is actually for:

```
  with the fix     47/47 objects, 100% of relocations   relocate 4.6 ms
  without the fix  46/47 objects,  32% of relocations   relocate 9.6 ms
```

"46 of 47" reads as a near-perfect hit rate. "32% of relocations" reads as the
failure it is. Same link, same instant, two units — and only one of them can be
acted on.

### The lesson, which is not the one finding 64 drew

Finding 64 said: surface the hit rate, because correctness tests cannot see a
dead cache. True, and insufficient. The instrument was added and had **the same
failure mode as the thing it was built to detect** — reporting success while
nothing happened — and it took the same evidence to catch it: a timing that
moved when it should not have.

So: **an instrument needs its own negative control.** Deliberately break the
thing being measured and confirm the number moves. That is one extra command
after adding any counter, and without it a metric is an assertion about the
code that nobody has tested.

## 67. Proving the inputs unchanged costs 0.18 ms, and that buys the whole link

The cache reuses relocated bytes, which skips one stage of five. The stages it
cannot skip — `read+parse` at 6.9 ms and `resolve` at 6.2 ms — run *before* the
cache is consulted, because addresses have to exist before anything can be
checked against them. That put a floor of about 17 ms under any incremental
link, and made 58% of the work permanently unreachable.

The floor is an artifact of checking too late. Measuring what it costs to
decide, from paths alone, that nothing changed:

```
  blake3 over rustc's 37 objects (0.31 MB)   0.16  ms
  stat the 19 toolchain rlibs                0.024 ms
                                             ----
                                             0.18  ms

  the link it would replace                 22.6   ms
```

125×. So when every input is unchanged and the request is identical, the right
answer is the binary already on disk, and reaching it needs no pipeline at all
— no parse, no resolve, no layout, no relocation, no assembly.

```
  ld-prime                     32.8 ms
  blinker, unchanged relink     9.2 ms      0.28x
```

Byte-identical output, and it still unwinds. This is the first configuration in
which blinker is *faster* than the system linker rather than close to it.

### What this case is and is not

It is the unchanged relink: `cargo` re-running the linker after a dependency's
timestamp moved, a build after `cargo test`, an IDE reissuing a build. It is
not the edit-compile loop, where one codegen unit changed — that still goes
through the pipeline with per-object reuse, at 22.6 ms.

Quoting the 0.28× as blinker's speed would be dishonest by omission. What it
establishes is narrower and still worth having: **the checking-before-reading
structure works**, and the same 0.18 ms proof is what a partial fast path needs
to identify the one changed object without reading the other 46.

### The general shape

Three caches have now been designed in this project, and the difference between
the two that worked and the one that did not is *when the validity check
happens relative to the expensive work*:

- the parse cache checked after reading, and lost to re-parsing (41);
- the relocation cache checks after parsing and resolving, and saves the one
  stage below it (59);
- this checks before any of it, and saves everything.

The artifact being cached mattered less than the position of the check.

## 68. Reading is not the cost; archive extraction is

`read+parse` was 6.9 ms of a 25 ms cold link and the obvious assumption was
that 17.2 MB of I/O dominates it. Measured instead:

```
  read 19 rlibs (16.87 MB)   1.18 ms   (14.95 GB/s, warm)
  read 37 objects (0.31 MB)  0.78 ms   ( 0.41 GB/s — per-file overhead)
```

Two megabytes of reading, and the rlibs — 98% of the bytes — cost a fifth of
what the small files cost, because at these sizes the price is per *file*, not
per byte. Any plan built on "avoid reading the rlibs" would have been chasing
1.2 ms.

Splitting the stage properly:

```
  initial read+parse loop     2.0 ms
  archive extraction          4.1 ms   (47 objects, 4 rounds,
                                         1.8 ms scanning for undefined symbols)
```

**Extraction is two thirds of the stage**, and nearly half of *that* is
re-scanning every object's symbol table once per round to find what is still
undefined — an O(rounds x symbols) loop where the rounds are only 4 and the
symbols are tens of thousands. The rest is parsing the 10 extracted members and
copying their bytes out of the archive.

### The parallelism, and what it was worth

The initial loop is embarrassingly parallel, and threading it took the stage
from 6.9 ms to 5.5 ms. Worth keeping, and much less than hoped, because it only
ever addressed the 2 ms half.

Two things showed up doing it:

- **Contiguous chunks are the wrong split.** A linker command line puts small
  objects first and large archives last, so slicing the input list by position
  handed one thread every rlib. A shared cursor balances itself.
- **Thread spawn is not free at this scale.** With 56 tasks averaging under a
  millisecond, eight threads spend a measurable fraction of the win starting
  up; the cursor version and the chunk version came out within noise of each
  other despite the chunk version being badly balanced.

Output stays byte-identical, which is the property that matters: object ids are
assigned by position before anything is read, so results are collected
positionally rather than as they finish. A link whose layout depends on thread
scheduling is not a link.

### The scan was allocation, not scanning

The 1.8 ms spent finding undefined symbols was not the search. Both sets were
keyed by `String`, so every round cloned the *entire symbol table of every
object* — tens of thousands of allocations — to answer a question about set
membership. Borrowing instead, and copying only the handful of names actually
returned:

```
  cloning     5.7  5.7  5.6 ms
  borrowing   5.0  5.1  5.0 ms      (sd 0.1-0.2, 20 iterations each)
```

0.6 ms, clean against the noise. `read+parse` is 6.9 ms to 5.0 across both
changes.

Worth naming because it is the same shape as finding 41, one level down: the
expensive part of a data structure is rarely the operation it is named for. A
`HashSet<String>` used only for `contains` is a copy of the data wearing a
lookup's clothes.

### What this says to do next

Not more parallelism, and not more of this. Cold-link work is now within a few
milliseconds of its floor for this structure, and the remaining stages —
`resolve` at 6.4 ms and `relocate` at 7.3 — are doing work that a changed
input genuinely requires. The only way to stop paying for *unchanged* inputs is
not to process them at all, which is the partial fast path finding 67 points
at.

## 70. The size gap is `__text`, and closing it needs atom-level stripping

blinker's output has been described as "2.0x ld-prime's" since finding 34. The
comparison needed a correction and then a breakdown.

The correction: **2.0x is against ld-prime *with* `-dead_strip`**, which is how
rustc invokes it. Against the same linker without the flag, blinker is smaller:

```
  blinker                        1055 KB
  ld-prime, -dead_strip           521 KB     2.02x
  ld-prime, no -dead_strip       1409 KB     0.75x
```

So blinker is not producing bloated output; it is producing *unstripped*
output, and the entire gap is one feature it does not implement.

Where the 534 KB sits:

```
  section                    blinker   ld-prime     delta
  __TEXT,__text                 574K       222K     +352K
  __TEXT,__eh_frame              96K        32K      +64K
  __TEXT,__const                 75K        13K      +61K
  __TEXT,__unwind_info           36K         6K      +30K
  __DATA_CONST,__const           27K         8K      +19K
  __TEXT,__cstring               15K         7K       +8K
  __TEXT,__gcc_except_tab        11K         5K       +6K
```

`__text` is two thirds of it, and every other line follows from it: unwind
tables, exception tables and string literals exist to serve code, so dropping
unreachable functions drops their metadata with them. There is no separate
problem to solve in those sections.

### Why it cannot be done at section granularity

The gap is *inside* objects that are legitimately needed. Archive members are
pulled in only to satisfy a referenced symbol, so no whole member is dead —
what is dead is the other forty functions that came with the one that was
wanted. Removing them means splitting a section into per-symbol atoms.

Mach-O says when that is legal, and every object in a Rust link says yes:

```
  subsections_via_symbols=YES   symbols.o
  subsections_via_symbols=YES   uw-...rcgu.o
  subsections_via_symbols=YES   std-...cgu.0.rcgu.o
```

`MH_SUBSECTIONS_VIA_SYMBOLS` is the compiler's assertion that no code in the
object refers to anything except through a symbol, so a section may be cut at
symbol boundaries without changing behaviour. Without that flag an object must
be kept whole; with it, atoms are the unit.

### What the work is

Not a filter over the existing pipeline. It changes the unit of layout from
"an object's section" to "an atom", which touches placement, `AddressMap`, the
relocation pass, and the unwind and exception tables that index by function
address. It is a milestone, not an optimisation, and it is the last large
correctness-adjacent feature blinker is missing.

The reachability itself is the easy half: roots are the entry symbol, the
exported symbols, and any section flagged no-dead-strip; edges are the
relocations, which the link already surveys.

## 71. Dead-stripping's analysis half, measured against the linker that does it

The reachability graph is built and reports what *would* be stripped, changing
no output. Built first, and separately, so the model could be checked against a
linker that already strips correctly before any layout is rebuilt around it.

On the 47-object Rust link:

```
  atoms live          938 of 2274
  __text live         255K of 574K
  ld-prime actual     222K
```

**255K predicted against 222K achieved.** A 15% over-estimate, and in the safe
direction: the analysis keeps things it cannot prove dead, so a stripper built
on it removes less than ld does rather than removing something needed. That
closes 319K of the 352K gap finding 70 measured, and confirms the atom model is
right before the expensive part is written.

### The first version reported 2274 of 2274

Everything live, which is an analysis saying nothing. The cause is worth
recording because it is not obvious: **unwind and exception tables name every
function in their object**, so rooting from any section outside `__text` made
every function reachable from its own `__compact_unwind` entry.

Metadata *describes* code rather than using it, and is dropped alongside the
function it describes. Excluding `__eh_frame`, `__compact_unwind`,
`__gcc_except_tab` and the debug sections from the root set took it from
2274/2274 to 938/2274 in one change.

The general form: in a reachability analysis, an edge from *B describes A* must
not be treated as *B uses A*. Both are relocations, and the format does not
distinguish them — only the section they live in does.

### There is no cheap version of this

Before writing the hard half, the obvious shortcut was measured: drop input
sections in which *no* atom is live. That needs no change to the unit of layout
— a dead section is discarded exactly like a linker-internal one — so if it
recovered most of the gap it would be worth landing on its own.

```
  strippable in total                    319K
  in sections where nothing is live        1K
```

**1K.** Every `__text` section in the link has at least one live atom, which
makes sense in hindsight: a section arrives because something in it was
referenced, so an entirely dead one can only come from an object pulled in for
a symbol in a *different* section. That is rare enough to be worth nothing.

So the shortcut does not exist, and the 15-line version of this feature would
have been implemented, measured, and deleted. The number that mattered took one
extra field on the report to obtain.

### One of the six tests cannot fail, again

Restoring metadata as a root leaves the test written for it passing. The reason
is specific and worth keeping: `__compact_unwind` refers to its function
through a **section** relocation with an inline addend, so symbol-based rooting
never sees it. `__eh_frame` uses symbol-named `SUBTRACTOR` pairs (finding 56)
and does trigger it — and macOS clang emits `__eh_frame` only where compact
unwind cannot describe a frame, which no small C fixture reaches.

So the guard for this one is the real Rust link and the numbers above. The test
is labelled as not reproducing it rather than left to imply it does.

Two of the six *are* verified by their controls, including the one that
matters most: dropping the data-pointer root makes
`a_function_referenced_only_from_data_is_live` fail. That is the direction that
would produce a binary that jumps into deleted code.

## 72. Dead-stripping, executed: atoms without making atoms the unit of layout

The analysis half (finding 71) predicted 255K of `__text` would survive against
ld-prime's 222K. Executing it produced:

```
  section            blinker   ld-prime    delta
  __TEXT,__text       224516     215028      +9K
  __TEXT,__const       11996      11652    +0.3K
  __TEXT,__cstring      7566       5889    +1.7K
  __TEXT,__eh_frame    28036      26912    +1.1K
  __TEXT,__gcc_except   4680       4504    +0.2K
  __TEXT,__unwind_info 10140       4888      +5K

  whole image         368531     468856    0.79x
```

`__text` within 4.4% of the linker that has done this for twenty years, and in
the safe direction: blinker keeps 9K of code ld drops, and drops none ld keeps.
The image is *smaller* than ld's overall, which is not a win — it is ld emitting
a fuller symbol table, a `__stub_helper` and a lazy pointer section that blinker
does not.

Cost: 2.8 ms on a 28 ms link, and roughly free in the total, because everything
downstream then relocates and emits a third less content.

```
  ld-prime    25.9 ms
  blinker     28.3 ms   1.09x     (1.10x before this)
```

### Atoms are the unit of *liveness*, not of layout

The milestone was scoped as "atoms replace sections as the unit of layout",
which would have split every input section into one placement per atom and
rewritten `Contribution`, placement, `AddressMap` and the relocation pass
around it.

That was more than the problem required. The survivors of a section keep their
original relative order, so the same image comes from leaving the section as
one contribution and **compacting** it — closing the gaps and recording where
each surviving byte moved. One `Strip::remap(object, section, offset)` then
serves every consumer of "where did this input byte end up", and none of them
has to learn what an atom is.

Six call sites changed by one line each; the relocation pass gained a guard.
The general shape: when a transformation preserves order, a coordinate
remapping is equivalent to a restructuring, and much cheaper to land.

### The one field in Mach-O that stripping cannot move

Everything in a relocatable object that points somewhere is a relocation —
with one exception, and it is in the section that matters most.

An `__eh_frame` FDE's second word is the distance **backwards** to the CIE
describing it. The assembler computed it; no relocation covers it. Compacting
the section moves records apart, so every one of those distances becomes the
distance they used to be.

The failure is a bad one to debug from behaviour: the binary links, starts,
prints, and dies of SIGSEGV the moment it unwinds — after `stack backtrace:`
has already been printed. What named it in one step was pointing `lldb` at the
file, which parses `__eh_frame` statically before running anything:

```
error: unable to find CIE at 0x1a8 for cie_id = 0xfc for entry at 0x2a0
error: Invalid cie offset of 0xffffffffffffffc0 found in cie/fde at 0x718
```

The general form: **a debugger's static parse is a linter for your output
format**, and it costs one command. Reach for it before reasoning about a
crash in generated code.

The related case that *is* a relocation, and was already handled: a `SUBTRACTOR`
pair whose subtrahend is the section's own `ltmpN` anchor stores the distance
from the anchor to the field, so that the result comes out field-relative.
Compaction moves both ends, and the addend is re-measured from where they
landed rather than where they were.

### Two premises, measured before the code depended on them

Both took minutes and both changed the design.

**Every reference lands exactly on a symbol.** Across the 47-object link, all
1832 pointer relocations in `__const` and every one elsewhere carry an inline
addend of zero. That is `MH_SUBSECTIONS_VIA_SYMBOLS` being true rather than
merely asserted, and it is what makes symbol-address remapping sufficient — no
reference needs resolving by offset. Sections that break the rule are kept
whole, so the guarantee is enforced rather than assumed.

The first version of that measurement was **wrong in the direction that would
have caused work**: it reported 1819 of 1832 relocations carrying an addend up
to 361120, which would have forced a full offset-based resolution path. The
model was wrong, not the data — for an *external* relocation Mach-O stores the
addend alone, and for a *section* one it stores the whole target address. I had
subtracted the symbol value from both. A null result needs proof of provenance
(finding 58); so does a positive one.

**Only `__compact_unwind` and the debug sections use section-relative targets.**
Which means exactly one place in the link reads a meaningful inline offset and
has to undo stripping by hand, and it is a place that already existed.

### The verification pass is the load-bearing part

The propagation is supposed to guarantee that no live atom refers to a dead
one. Guaranteed-by-argument is not guaranteed, so a final pass walks every live
atom's references and revives anything dead, counting what it had to fix.

That count is the model's own negative control. It is zero on a real Rust link,
and removing any single propagation rule makes it non-zero — which is what
makes asserting it worth anything. Without the assertion the pass would be a
liability rather than a safety net: it *repairs* the mistake, so an incomplete
model would produce a correct binary and no symptom at all. That is finding 64
in a different costume.

### rustc always passes `-dead_strip`, so building without the flag is no control

The obvious test for "did stripping remove anything" builds the fixture twice,
once with `-Wl,-dead_strip`. On macOS rustc passes that flag on every link, so
both builds are stripped and the assertion cannot fail. The first version
reported `224492 against 224492` and passed.

The control has to come from replaying the recorded argument list with
`-dead_strip` removed — the only way to reach a Rust link that was not
stripped. Third time a test in this project has been found unable to fail
(63, 66, 69), and the first where the cause was a *toolchain default* rather
than the fixture.

## 73. The partial fast path was already built, and the milestone it was scoped against no longer existed

The plan's last milestone read: *"relocating the changed object alone and
patching its bytes into the cached image… the payoff is the difference between
22 ms and something near the 10.4 ms the unchanged case gets."*

Before building it, the premise was measured: a real cargo project, built
warm, with one constant changed in one function body.

```
                       reused        relocs   read resolve layout reloc emit | total
  cold                  0/26           0%      6.0   1.0     0.7   8.5   0.9 | 22.1
  one-line body edit   24/26         100%      5.4   1.0     0.6   3.1   0.8 | 16.1
```

**The edit-compile case already reuses 24 of 26 objects and skips 100% of the
relocations.** Per-object reuse (finding 62) covers it. The thing the milestone
proposed to build — relocating only the changed object — would remove part of
the remaining 3.1 ms, out of 16.1.

The milestone was written when relocation was the largest stage of a link. It
had stopped being that, and the plan still described the old shape. **A stale
plan is a premise like any other, and it survives longer than the measurement
that produced it** — there is nothing to notice unless you re-measure before
starting, which is the same rule as findings 41, 60 and 68 applied to your own
notes rather than to the code.

## 74. A quarter of every link was spent parsing a file that never changes

With relocation no longer dominant, the same measurement showed where a warm
link's time had gone instead. Splitting the stages that were left:

```
  resolve                 6.8 ms
    undefined_references  0.3
    stub library          5.6      <- parse libSystem.B.tbd, 334 KB of YAML
    symbol table          0.6
```

`resolve_imports` answers one question — which undefined names does the system
provide? — and to answer it, it parses the whole of `libSystem.B.tbd`: 40 YAML
documents, 9264 exported symbols, **5.6 ms, a quarter of the link**. On every
link, cold and warm alike, for a file that changes when Xcode is updated.

### The fix is not a cache

A cache is the obvious answer and it is justified by finding 59 — a sorted list
of 9264 strings is far flatter than the YAML parse that produced it. It is
still the wrong answer, because it buys a warm-link win for the price of new
on-disk state whose staleness could change an output.

The export list depends on **nothing the objects produce**. So it is read
alongside them instead of after them:

```rust
let (objects, exported) = std::thread::scope(|scope| {
    let stub = scope.spawn(|| request.dynamic_symbols());
    let objects = load_objects(&request.objects);
    (objects, stub.join().expect("the stub reader did not panic"))
});
```

```
  resolve   6.8 ms  ->  1.0 ms
  total    22.1 ms  -> 16.1 ms   (warm)
```

Nine lines, no new state, no invalidation to get wrong, and the whole 5.6 ms
disappears into the 5.4 ms the object read was already taking.

**Before caching a pure computation, check whether it can simply happen at the
same time as something else.** Concurrency and caching solve the same problem
here — work that is on the critical path and need not be — and only one of them
adds a way to be wrong.

### Which puts blinker level with the system linker

```
  ld-prime    26.6 ms  (min 24.6, sd 1.4)
  blinker     25.6 ms  (min 23.9, sd 3.1)
```

0.91–0.96x across runs, and the difference is inside the spread: they are now
comparable, having been 1.10x. blinker's own spread is the wider of the two,
which is the next thing to understand rather than a result to claim around.

## 75. The 9 ms I measured was my own harness deleting an environment variable

With the stub *parse* overlapped (finding 74), the stub *path lookup* was still
on the critical path — and it spawns `xcrun`:

```
  default_stub_library()   7.46 ms      spawn xcrun --show-sdk-path
                           0.06 ms      read /var/db/xcode_select_link
```

`xcode-select` records the active developer directory as a symlink, and `xcrun`
resolves the SDK beneath it, so reading the link answers the same question
without a process. The change is right and is kept: `xcrun` remains the
fallback, the discovered path is asserted to equal the one `xcrun` names, and
the output is byte-identical.

**It speeds up nothing that is actually run.** Establishing that took four
measurements, three of which I first got wrong, and the sequence is the
finding.

### 1. An A/B said 10 ms, the project's own harness said 0.9 ms

Same two binaries, same inputs, both interleaved:

```
  standalone A/B      old 40.1 ms   new 29.9 ms
  scripts/bench.py    old 25.3 ms   new 24.4 ms
```

Two reproducible measurements that cannot both describe the same thing. I
recorded the disagreement rather than the number that flattered the change,
which was the only part of the first write-up worth keeping.

### 2. `SDKROOT` decides it, and both compiler drivers set it

`discover_stub_library` checks `SDKROOT` before spawning anything. Measured
with a shim linker that prints its environment: **`rustc` sets `SDKROOT`, and
so does plain `cc`.**

```
  SDKROOT=[.../MacOSX.sdk]  DEVELOPER_DIR=[<unset>]
```

So no real build ever reached the spawn. That much of the first write-up was
right, and I concluded from it that the benchmark — which I believed ran
without `SDKROOT` — had been measuring a configuration nobody uses.

### 3. It was my harness that ran without it

The difference between the two harnesses was one line I had written to be
careful:

```python
env = {k: v for k, v in os.environ.items() if k != 'SDKROOT'}
```

I had checked with `printenv SDKROOT` in the shell, seen nothing, and
"controlled" for it. `bench.py` passes no `env=` at all and inherits.

```
  old, inherit env      26.5 ms
  old, explicit env     38.5 ms      <- the same binary, minus one variable
```

The variable I deleted to be careful was the entire effect I then measured.
**A harness that controls for a variable by removing it manufactures the
configuration it goes on to report.**

### 4. Why the shell and the harness disagreed about the environment

```
  printenv SDKROOT                            unset
  python3 -c "'SDKROOT' in os.environ"        True
  env -i /usr/bin/python3 -c "...os.environ"  /Applications/Xcode.app/...
```

`/usr/bin/python3` is one of the `xcode-select` shim binaries — 78 hard links
to one stub — and it launches the real interpreter through `xcrun`, which
**exports `SDKROOT` into the process it starts**. An empty environment does not
help; `/bin/sh` under `env -i` has nothing, and `python3` under `env -i` has
the SDK.

So every Python benchmarking harness on macOS runs with `SDKROOT` set, whatever
the shell says. On this platform `python3` is not a neutral parent, and a
harness written in it inherits an environment its author never chose.

### What this costs and what it is worth

The change stands on its own terms — it removes a process spawn from the one
path that still reaches it, and a build system that invokes the linker directly
(a Makefile calling `ld`, not `cc`) does reach it. It is worth roughly zero on
every workload measured here, and the honest place for it is a footnote rather
than a performance claim.

The general form, which cost more to learn than the 9 ms was ever worth:
**check what the environment *is* before controlling for it, and check it from
inside the harness rather than from the shell you launched the harness from.**

## 76. Where a link's time is now, and two attempts at the largest stage

After dead-stripping (72), the overlapped stub parse (74) and the withdrawn
fast path (73), a cold link of the 47-object Rust program:

```
  read+parse   5.4 ms   34%      dead-strip  2.8 ms  17%
  relocate     3.1 ms   19%      resolve     1.0 ms   6%
  layout       0.6 ms    4%      emit+sign   0.8 ms   5%
```

`read+parse` splits into a parallel half and a sequential one:

```
  parallel phase   2.26 ms    8 objects + 19 archives, read and indexed
  member pull      3.7 ms     10 members, 4 rounds, one thread
    undefined      1.2 ms     recomputing the frontier each round
    parse          1.1 ms
    copy           1.0 ms     13,423 KB memcpy'd to own the member bytes
    extract        0.0 ms     member_data is a subslice; it costs nothing
```

Two changes followed, both A/B'd against the previous binary, interleaved, 25
iterations, inheriting the environment (75). **One delivered nothing and was
reverted; the other delivered nothing in time and a third of the memory.**

### The frontier: measured 1.2 ms, delivered 0 ms, reverted

Pulling a member can only satisfy the names it defines and raise the ones it
references, so the undefined set looked like an obvious thing to carry across
rounds rather than recompute from every symbol of every object four times.

```
  base       median 27.4 ms
  frontier   median 28.6 ms      (byte-identical output)
```

Nothing, or slightly worse. The reason is a commit from earlier in the same
project: `undefined_references` uses `HashSet<&str>` — borrowed, no
allocation — because ffcf851 removed exactly that cloning. A frontier carried
*across* rounds cannot borrow from the vector it is pushing objects into, so it
has to own its names, and the incremental version traded three cheap borrowed
scans for one scan plus half a million `String` allocations.

**An incremental version of a cheap computation can cost more than the
computation.** The thing that makes recomputation cheap here — borrowing — is
exactly what incrementality forbids.

### The copy: measured 1.0 ms, delivered 0.5 ms of time and 12 MB of memory

Each archive member's bytes were copied out of the archive's buffer with
`to_vec()`, so the link held them twice: once in the archive it came from, and
once in the object parsed out of it. Sharing them needed a small `Deref`
newtype, so that reading them stays spelled the way it was.

```
  time        base 27.0 ms   shared 26.5 ms      within noise
  peak RSS    base 42 MB     shared 30 MB        reproducible, 3 runs each
```

Kept — for the memory, which is a 29% reduction and holds run to run, and not
for the time, which does not. The 1.0 ms the instrumented build attributed to
`memcpy` does not appear in wall clock; either it overlaps with other work or
timing a tight loop inflated it. Both are reasons to believe the A/B over the
profile.

**A profile says where time is spent; only an A/B says what removing it is
worth.** Two changes here were built on profile numbers of 1.2 ms and 1.0 ms,
and between them they bought no measurable time at all.

### Where the profile ran out

Dead-stripping is the newest stage and the only one never examined from
inside, so it was profiled before assuming anything about it:

```
  dead-strip   2.5 ms
    Atoms::build   0.40 ms      7599 atoms
    liveness       1.85 ms
    Strip::build   0.08 ms
    report         0.08 ms
```

`liveness` is three quarters of it, and roughly half of `liveness` is the
verification pass — a second walk over every live atom's relocations,
confirming that nothing live points at anything dead. Making the propagation
establish that invariant by construction would remove it, worth about 0.9 ms.

**It was not built.** 0.9 ms is the same size as the two changes above, which
between them delivered nothing measurable, and the pass being removed is the
one that turns a hole in the reachability model from a corrupt binary into a
number (72). Trading a correctness guarantee for a gain the previous two
measurements say will not appear is a bad trade twice over.

That is the state of the profile: **nothing identified is above the noise
floor.** The link is at parity with the system linker, and every remaining item
is ~1 ms on a ~25 ms job with a run-to-run spread wider than that. Further
work here needs either a different workload to measure against, or a
structural change rather than another line of the profile.

## 77. The fixture was too small to show that the linker was quadratic

Every performance number in findings 72–76 came from one 47-object link. The
spec asks for "at least one large real Rust project" and there was none, so
blinker was pointed at the largest one to hand: **itself**.

```
                  fixture      blinker's own binary
  inputs           27 files     79 files, 89.3 MB
  objects          47           921
  blinker/ld64     0.92x        7.44x
```

**7.44x.** Not a regression — it had always been there, and a fixture five
times too small never asked the question. Relocation alone went from 3.1 ms to
**187 ms**, sixty times worse for five times the input.

### One line, called once per relocation

```rust
image.layout.sections.iter().enumerate()
    .find(|(_, s)| s.address_of(object.parsed.id, relocation.section).is_some())
```

`address_of` scans every output section, and within each one every
contribution. At 27 inputs that is a few hundred comparisons per relocation and
invisible. At 79 inputs it is ~900 contributions × 15 sections × 200 000
relocations.

Nothing was wrong with the lookup. It was asked once per relocation when the
answer changes once per *input section*, so it is now built once into a
`HashMap<(object, section), (index, address)>` and read.

```
  relocate      187.0 ms  ->  26.9 ms
  whole link    345 ms    ->  156 ms      7.44x -> 3.77x
```

The same pattern was in `address_map`, `output_symbols`, `entry_offset` and
`target_address` — each doing a linear scan per *symbol*.

### What the real workload actually costs

```
  read+parse   56.9 ms   38%      921 objects, 89 MB
  dead-strip   30.6 ms   21%
  relocate     25.9 ms   18%
  emit+sign    11.3 ms    8%
  resolve       7.7 ms    5%      layout 6.9 ms
                                  ld64: 41.5 ms total
```

Nothing here is quadratic any more — every stage grew sublinearly in object
count between the two workloads. blinker is 3.6x the system linker on a real
binary, which is an honest starting number rather than a bug.

`read+parse` is now the largest stage and the reason is structural: blinker
materialises every symbol and relocation of every object into owned `Vec`s and
`String`s, because `ParsedObject` was designed to be serialisable for a parse
cache — a cache that was then measured and abandoned (41). The representation
outlived its reason and is now the single largest cost in the link.

### The rule

**A benchmark fixture is a claim about scale, and it expires.** Five findings'
worth of measurements, every one of them carefully interleaved and
negative-controlled, were all taken on a workload too small to exhibit the
dominant behaviour of the system. The methodology was sound and the sample was
not, and no amount of care inside the harness could have shown that.

## 78. Three changes that were worth nothing at fixture scale, and 60 ms at real scale

Finding 77 fixed the quadratic address lookup and left blinker at 3.77x the
system linker on its own binary. Profiling *that* workload rather than the
fixture turned up three more, and two of them had already been examined and
dismissed.

```
                     fixture   blinker's own binary
  member_defining      ~0 ms      25 ms
  the frontier          0 ms      23 ms      <- built, measured, reverted (76)
  the verify pass     0.9 ms      14 ms      <- measured, declined (76)
```

### `member_defining` was a linear scan

```rust
self.symbol_map.iter().find(|(name, _)| name == symbol)
```

`libstd.rlib` lists tens of thousands of symbols, and the linker asks this
question once per still-undefined name, per archive, per extraction round. It
reads like a lookup and behaved like one at 27 inputs. The table is now sorted
at parse time and binary-searched — a sorted `Vec` rather than a `HashMap`
because it is built once, read many times, stays serialisable, and needs no
second copy of every name.

### The frontier, reverted in 76, restored here

Carrying the undefined set across extraction rounds instead of recomputing it
was built, measured at **0 ms** on the fixture, and reverted with a clear
explanation: the recomputation borrows (`HashSet<&str>`) and an incremental
version must own its names, so incrementality cost more than the computation.

That explanation was correct and the conclusion was wrong. The fixture ran four
rounds over 47 objects; a real link runs **eleven rounds over 921**. The
allocation cost is linear in names and the scan cost is quadratic in rounds ×
objects, so the trade flips. Restored, it is worth 23 ms.

### The verify pass, declined in 76, made cheap here

Dead-stripping ends by walking every live atom's relocations to confirm nothing
live points at anything dead. At 0.9 ms it was left alone, deliberately —
removing a correctness guarantee for a gain that size is a bad trade.

At 14 ms it is worth thinking about, and thinking produced a version that keeps
the guarantee whole. Propagation marks the targets of every atom it visits, and
every live atom is visited exactly once, so the invariant holds by construction
everywhere *except* the atoms whose edges were deliberately suppressed. Only
those need checking. The guarantee is identical; the work is proportional to
the suppressed set rather than to the live one.

```
  dead-strip   32.3 ms -> 18.7 ms      byte-identical output
```

### What the sequence shows

```
  7.44x  ->  3.77x  ->  3.04x  ->  2.92x      against ld-prime
```

Two of these three were correctly analysed and wrongly concluded, months of
reasoning apart from nothing but a change of workload. The analysis said "this
costs 0.9 ms and buys a guarantee"; it was true, and it was true of a link
nobody runs.

**A decision not to optimise is as scale-dependent as a decision to.** Both
need re-deciding when the workload changes, and a rejected optimisation leaves
no trace in the code to prompt it — which is why the two above are named in the
comments where they now live.

## 79. The cold number was never the problem. The edit number is.

"blinker is 2.92x the system linker" is a fact about the case blinker exists to
avoid. Measured at real scale, on blinker's own 937-object binary:

```
  ld-prime, every time              41.8 ms
  blinker, cold                    122 ms      2.92x
  blinker, nothing changed          13.3 ms    0.32x   921/921 objects reused
  blinker, one crate edited        128 ms      3.1x    728/937 reused, 80% of relocations
```

The unchanged relink is **three times faster than the system linker**. The
edit relink — the only one a developer actually experiences — is three and a
half times *slower*, and barely faster than blinker's own cold link.

### Why reusing 69% of the work saves 6% of the time

The incremental path skips one stage. At fixture scale that was most of the
link and the total was 16 ms, so it looked like the design worked:

```
  read+parse   26.1 ms   21%     re-run in full
  dead-strip   18.7 ms   16%     re-run in full
  relocate     27.7 ms   23%     <- the only stage reuse touches
  emit+sign    12.0 ms   10%     re-run in full
  resolve       7.9 ms    6%     re-run in full
  layout        6.9 ms    6%     re-run in full
```

Skipping 69% of 23% is 16% of the link. Everything else runs exactly as it did
cold, because parsing, reachability, layout, symbol-table generation and
signing are all whole-image computations with no incremental form.

This is finding 73 arriving in a different costume. There, measuring the
edit case on a 47-object fixture showed relocation reuse already at 100% and a
16 ms link, and the conclusion was "the partial fast path is already built".
It was — and it was built on the only stage that was worth anything *at that
size*. At 937 objects the same design leaves 84% of the link untouched.

### What this actually costs to fix

Not another optimisation. Every remaining stage needs an incremental form, and
they are not equally hard:

- **emit+sign** (12 ms) — the signature is a Merkle tree over pages; only
  changed pages need rehashing. Self-contained.
- **layout** (7 ms) — the `Slop` machinery already exists and is tested
  (findings 42–44): reserved slack lets a contribution grow without moving its
  neighbours. Nothing calls it. Wiring it also stabilises the cache's
  placement keys, which is a second win.
- **dead-strip** (19 ms) — reachability is a fixpoint over a graph that barely
  changes between edits, and nothing about it is incremental yet.
- **read+parse** (26 ms) — the hardest, and finding 41 already ruled out the
  obvious answer: deserialising a parsed object is slower than parsing it.

### The rule

**An incremental system is only as incremental as its slowest whole-image
stage,** and which stage that is depends on scale. Every measurement that
justified the current design was taken where `relocate` dominated. It does not
dominate a real link, and no amount of improving it would have mattered.

## 80. Reserved slack raised the hit rate and did not make the link faster

Finding 79 named four whole-image stages and put `layout` first because the
machinery already existed, tested and unwired since findings 42–44. Wiring it:

```
                        reused        relocations   link
  one crate edited   691/937  ->  728/937   69% -> 80%   142 ms -> 128 ms
```

The hit rate went up exactly as designed, and the link went from 142 ms cold
to 128 ms — **10%**, for 80% of the relocations skipped. The padding inflates
the image, so `emit+sign` gives back part of what `relocate` saved, and
`relocate` is only 23% of the link to begin with.

The first version of this measurement said 183 ms, and was wrong: those runs
had `--blinker-record-invocation` on, which copies all 89 MB of inputs aside
before linking. Recording is a corpus tool and it has no business inside a
timing loop — the fourth time in this project that a harness measured itself
(75).

Which is finding 79 restated as a result rather than an analysis: **improving
the reuse rate cannot help while the reused stage is a fifth of the work.**
Going from 69% to 80% of relocations skipped is worth 14 ms on a 142 ms link,
and no achievable hit rate changes that — even 100% reuse of `relocate` leaves
115 ms.
Slack is still the right thing to have — it is what makes a *future*
incremental layout possible at all, and the hit rate it buys is real — but on
its own it is a cost, and the honest accounting is that this was the cheap item
on the list and it bought nothing yet.

### Two production bugs found by getting there

**The cache did not include the linker.** Changing blinker and relinking
replayed the *previous* build's binary: inputs unchanged, request unchanged,
whole-image fast path fires. It cost an hour — a correct fix measured as broken
because the cache was serving output from the build before it — and in a
release it would mean upgrading the linker silently changes nothing. The key
now includes the executable's own path, size and mtime.

**Padding is not universally inert.** A deny-list was the wrong shape:

- `__eh_frame` is a chain walked by each record's length field, so a gap is a
  record header made of zeroes. It links, runs, and dies unwinding.
- `__thread_vars` is an array of 24-byte descriptors, and dyld checks: *"size
  (512) of thread-locals section __thread_vars is not a multiple of 24"*.
- `__got`, `__stubs`, `__thread_ptrs`, `__la_symbol_ptr` and `__mod_init_func`
  are fixed-stride arrays indexed by position.

Guessing which tolerate a gap is not a judgement a linker should make, so
padding is now an allow-list: it goes only where it is known to be harmless.

And the first attempt tied slack to *whether a cache was being written*, which
made a cold link and a cached one lay out differently — breaking the property
the entire design rests on, that an incremental output is what a cold link
would have produced. It is a property of the request now, applied to both.

## 81. The layout probe was signing an image nobody reads

The linker assembles twice: once to discover where everything lands, once with
real content. Both passes went through the same builder, so both produced a
complete Mach-O image — including an ad-hoc code signature, which is SHA-256
over every page of the file.

The probe's bytes are dropped; only `Image::layout` is read from it.

```
  layout stage    6.9 ms  ->  0.8 ms
  whole link      122 ms  ->  107 ms       byte-identical output
  vs ld-prime     2.92x   ->  2.65x
```

**6.2 ms per link, cold and warm alike, hashing megabytes that are then
freed.** It hid inside `layout_probe_ms`, which is why every profile in
findings 76–80 reported layout as a 6 ms stage and nobody asked what a layout
pass was doing for six milliseconds.

The space is still reserved when the signature is skipped, so the offsets the
probe reports are the ones the signed image will have — otherwise the probe
would describe a layout the real pass does not produce.

### The reason it survived four rounds of profiling

The stage names came from the pipeline's structure rather than from what the
code does, and `layout` sounded like an answer. A stage that is 6% of the link
and named after something cheap does not invite the question "6% of *what*?" —
and the two passes were only visible from inside `assemble`, one call away from
the timer.

**A profile is only as honest as its stage boundaries.** These were drawn where
the functions were, not where the work was.

## 82. Where this leaves the linker

```
  ld-prime, every time         41.9 ms
  blinker, unchanged relink    13.3 ms   0.32x    921/921 objects reused
  blinker, one crate edited   123 ms     2.9x     728/937, 80% of relocations
  blinker, cold               111 ms     2.65x
```

Measured on blinker linking its own 937-object binary. The cold link came down
from 7.44x over findings 77–81, entirely by removing work that was quadratic,
redundant, or thrown away — no algorithmic cleverness and no risk taken with
correctness: every step verified byte-identical output.

The unchanged relink is three times faster than the system linker and is the
proof that the caching design works. The edit relink is the product and it is
still 2.9x, because reuse covers `relocate` and nothing else:

```
  read+parse   26.8 ms   re-runs        relocate   28.8 ms   reused
  dead-strip   20.9 ms   re-runs        emit+sign  11.4 ms   re-runs
  resolve       9.1 ms   re-runs        layout      0.8 ms   re-runs
```

What remains is not tuning. Five stages need incremental forms, and the two
that matter — `read+parse` and `dead-strip`, 48 ms between them — are genuine
design work. Finding 41 already ruled out the obvious answer for the first
(deserialising a parsed object is slower than parsing it), which points at the
daemon the spec's Stage G was always for: the only way not to re-read 89 MB is
to still have it.

## 83. Parallel member parsing bought 2 ms of the 7 it looked worth

900 archive members were parsed one after another on one thread. Restructuring
each extraction round to choose its members first and parse them concurrently:

```
  read+parse   26.8 ms -> 24.5 ms      whole link 107 -> 100 ms, byte-identical
```

Determinism is kept by construction rather than by luck: the round's members
are chosen before any of them is parsed, ids are assigned by position, and
results are collected positionally — so no thread's timing can reach the
output. Verified byte-identical.

2.3 ms, where an earlier split had attributed ~8 ms to parsing. The difference
is that the earlier measurement was taken before findings 78's fixes; three
quarters of what parallelism looked worth had already been removed by making
the surrounding loop cheaper.

Where the rest of `read+parse` goes, measured rather than inferred:

```
  read+parse          24.8 ms
    Frontier::absorb   5.9 ms      cloning every symbol name into a HashSet
    member parse       ~2 ms       now concurrent
    the rest          ~17 ms       reading 89 MB, parsing 21 objects, probing
```

`absorb` is a quarter of the stage, not the bulk — the inference was right in
kind and wrong in size, which is why it was then measured. It is the same
owned-`String` representation finding 82 names as the structural problem,
showing up in a third place.

**Parallelism moves a cost; it does not remove one.** Three quarters of what
this looked worth had already been fixed by making the surrounding loop
cheaper, and the remainder is an allocation problem that no number of threads
addresses.

## 84. Dropping local symbols did not lose names, it invented them

The output symbol table carried only non-local definitions. The comment
justifying it argued locals are "invisible outside their object by definition,
and the only consumer that would want them is a debugger". Both halves are
wrong, and the error compounds.

Most Rust functions are local. Anything not `pub`, plus nearly every
monomorphisation pulled out of `std`, is a local symbol — 543 of them in a
hello-world, against 289 externals. And the consumer is not a debugger; it is
the panicking program itself, symbolicating its own backtrace from its own
symbol table.

The failure is not a missing name, because a symbolicator resolves an address
to the nearest symbol *at or below* it. Removing the locals does not remove
the answer. It moves the answer to whatever global happens to precede the
frame, and prints it with no mark of uncertainty:

```
  ld-prime                        blinker
  1: hello::deep                  5: hello::main
  2: hello::deep                  6: hello::main
  3: hello::deep                  7: hello::main
  4: hello::deep                  8: std::rt::lang_start::<()>
  5: hello::middle
```

Four frames in `deep` and one in `middle`, reported as three in `main`. In a
second run the same frames came out as `core::fmt::rt::Argument::new_display`.
The backtrace is well-formed, plausible, and describes a call stack that never
existed — a silent mislink reached through the symbol table rather than
through a relocation.

Two details the fix depends on:

- **Assembler temporaries must still be dropped.** Mach-O reserves the `L`
  prefix for them, and they are emitted in bulk to anchor section starts. A
  section-anchor label sits at the exact address a real function begins, so
  emitting them makes the backtrace *worse* than dropping the locals did. `ld`
  applies the same rule; a binary it links contains no `L` symbol at all.
- **`n_sect` is read.** Every symbol was emitted claiming section 1, which
  made data symbols look like stray text.

Cost, measured interleaved against the unchanged linker on the same 60-input
link: **+6.5 ms on 38.7** (±1 ms; a variant ordering measured 5.8 and 7.3 for
identical code, which is the width of the noise), and **+8.7% output size**.
The size is not a regression but a correction: the output was 0.94x ld's
because it was missing what ld emits, and is now 1.03x.

## 85. Every image blinker produced claimed the same identity

Found while chasing 84, and the reason it took two hours rather than ten
minutes: after emitting the locals, the *same executable* symbolicated
correctly in one directory and incorrectly in another. Byte-identical file,
`cmp`-verified. Copying it to an empty directory fixed it; copying a sibling
blinker binary in broke it again.

`LC_UUID` was sixteen zero bytes, with a comment saying a real UUID was "a
later step":

```
  hello-ld    UUID: 7F8C9E98-AD31-360A-A8B2-6FE70BB9C2E4
  hello-bl2   UUID: 00000000-0000-0000-0000-000000000000
  hello-bl3   UUID: 00000000-0000-0000-0000-000000000000
  fixture     UUID: 00000000-0000-0000-0000-000000000000
```

macOS resolves debug information *by* UUID — Spotlight indexes `.dSYM` bundles
under the UUID of the binary they describe — so every blinker output was a
candidate answer for every question about any other. The observed effect was a
backtrace symbolicated from another program's debug information, which is why
the wrong names looked arbitrary rather than merely shifted.

The final proof was one command, two binaries from the same linker with the
same symbol table, in the same directory:

```
  hello-u1   (content-derived UUID)   1: hello::deep
  hello-bl3  (zero UUID)              1: std::rt::lang_start::<()>
```

It is now a SHA-256 of the image up to the signature, truncated to sixteen
bytes and stamped with the RFC 4122 variant and version-5 nibble. Derived from
content rather than from a clock or a counter because determinism is
load-bearing here: byte-identical output is what the cache is built on. It is
stamped after truncation and before signing — the signature covers the UUID,
so a later stamp invalidates every page hash over the load commands, and an
earlier one would fold the reserved signature space into the image's identity.

**What this cost, methodologically.** The end-to-end reproduction is
environment-dependent: it needs a `.dSYM` next to the binary, which `rustc`
produces directly and `cargo`'s dev profile does not. The kept test asserts
the property that has no environment in it — two different programs get two
different non-zero UUIDs — and the doc comment carries the observation. A test
that needs Spotlight to have indexed a directory is a test that fails for
reasons unrelated to the linker.

**The rule this is an instance of.** Both 84 and 85 were features whose
absence was recorded in a code comment as a deliberate deferral. Neither
comment was wrong about the deferral; both were wrong about the consequence.
"Not implemented yet" and "produces confidently incorrect output" are not the
same state, and only one of them is safe to leave alone.

## 86. The debug map's `SO` names do not have to be right

The debug map is how a Mach-O executable says where its debug information
*is*: the DWARF stays in the `.o` files and the executable carries stabs
naming the object each function came from and the address it landed at. The
shape `ld` emits, per compilation unit:

```
  SO    "<dir>/"      SO "<file>"      opens the unit
  OSO   "<path.o>"    n_desc 1, n_value the object's mtime
    BNSYM / FUN "_name" / FUN "" (size) / ENSYM     per function
    GSYM "_global"                                  global data
    STSYM "_static"                                 static data
  SO    ""            closes it
```

`ld` fills the two `SO` strings from the object's DWARF `DW_AT_comp_dir` and
`DW_AT_name`, which means a DWARF parser — abbrev tables, form decoding, the
whole thing — for two strings. Before writing one, the question was whether
they are load-bearing. They are not, and the check was five lines: overwrite
`ld`'s own `SO` strings in an already-linked binary with same-length garbage
and ask `atos` the same question.

```
  correct SO   helper (in a-ld)       (a.c:2)
  wrong SO     helper (in a-wrongso)  (a.c:2)
```

`/nowhere/XXXXXX...` and `z.c`, and the file and line still resolve, because
they come from the DWARF the `OSO` points at. The `SO` names a compilation
unit; it does not locate anything. blinker derives both from the object's own
path and skips the parser entirely.

The rest is mechanical, with one part that has to be computed rather than
copied: **a Mach-O symbol table does not record function sizes**, so the size
in the second `FUN` stab is the distance to the next definition in the same
chunk, or to the chunk's end for the last one. Checked against `ld`'s numbers
for the same object — same machine code, two linkers, so the addresses differ
and the sizes must not.

Verified end to end without a debugger, which matters because `lldb` needs an
attach authorisation this project does not ask for. `atos` and `dsymutil` read
the debug map and need no permission at all:

```
  atos -o a-bl 0x100000548     helper (in a-bl) (a.c:2)      identical to ld's
  Rust backtrace               fixture::deep at ./src/main.rs:1:38
```

**Cost, measured interleaved:** +1.7 ms and +7.7% output. The size was then
mostly recovered by interning the string table — the debug map names every
function a second time, so half the added strings were copies of the other
half — bringing it to **+1.0 ms and +2.0%**. blinker's output is now 1.05x
`ld`'s, against 0.94x when it was missing what `ld` emits.

**What this closes.** Spec §23 lists five debug behaviours the MVP must state
the status of. Four now work and are tested: breakpoint-by-name has the
symbols to resolve against, and stack traces, source-line display, panic
backtraces and test-failure backtraces all produce what `ld`'s output does.
Full `.dSYM` production remains out of scope, as §23 allows.

## 87. The largest hidden stage, and what it was hiding

`dead_strip_ms` was measured inside the link and dropped on the way out: the
record carried five stages that summed to well under the link's own total, and
nothing said where the rest went. Plumbing it through is four lines. What it
showed made the plan for the next three days wrong.

The whole link, on a 60-input Rust workload (blinker linking itself):

```
  read+parse    7.94   22.2%
  emit+sign     7.36   20.6%
  relocate      6.71   18.8%
  dead-strip    4.82   13.5%
  unaccounted   3.92   11.0%
  layout        2.58    7.2%
  resolve       2.40    6.7%
                              35.7 ms
```

Dead-strip is **4.8 ms, not the 21 ms** the plan called it. The 21 ms came from
a different, larger workload and had been carried forward as though it were a
property of the linker. It is fourth, not first.

The rule: a number measured on one workload is a fact about that workload. It
stops being evidence the moment the workload changes, and carrying it forward
is how a plan comes to be about a linker that does not exist.

## 88. A one-crate edit reused nine relocations out of eighty-three thousand

With the stage visible, the next question was what an *edit* relink costs. The
first attempt answered 1.38 ms and reused everything — measuring the second
link, not the first: after one edit the inputs stop changing, so iterations two
and three replayed a cached whole image. Alternating the edited rlib between
two versions so every iteration really is an edit:

```
  reused_inputs           1 of 105
  reused_relocations      9 of 83 687
  wall                   55.2 ms      against a 35.7 ms cold link
```

The incremental path was **slower than the cold path**, on the workload it
exists for, while reporting `mode: incremental`.

**First hypothesis, refuted:** dead-stripping compacts each section to exactly
its surviving bytes, so a liveness change anywhere resizes contributions
everywhere — which would defeat the reserved slack. Re-running with
`-dead_strip` removed: still 1 input, still 9 relocations. Not it. Five minutes
to test, and it would have been a day to build the fix for.

**What it actually was**, found by asking the only question that separates the
candidates — does the edit move addresses? — and answering it by diffing the
two images:

```
  symbols in both  2413      moved  1381  (57.2%)

  __TEXT,__text        100000900 -> 100000900   1016064 -> 1015296   changed size
  __TEXT,__stubs       1000f8a00 -> 1000f8700      1536 ->    1536   moved
  __TEXT,__const       1000f9000 -> 1000f8d00    220672 ->  220672   moved
  __TEXT,__cstring     100139e00 -> 100139b00     36352 ->   36352   moved
  ... six more, all moved by exactly 0x300, none changed size
```

`__text` changed by 768 bytes and every section after it slid by exactly that,
with their own sizes unchanged to the byte. The cache matches an object's
cached bytes by *where they landed*, so almost nothing matched.

Slop reserved padding after each contribution — inside a section. Nothing
reserved anything between sections, so the stabilisation stopped at the first
section boundary. Padding the *gap* fixes it, and padding the gap rather than
the sections is what makes it safe: nothing reads the gap, so no section's size
or internal structure changes. That is not a detail — a padded `__eh_frame`
dies in the unwinder and dyld rejects a `__thread_vars` whose size is not a
multiple of its record size, both learned the hard way (finding 80).

Choosing the stride, by the two things that trade against each other:

```
  stride    relocations reused    image      edit relink
   none            9 of 83687     1775 KB      55.2 ms
   1 KB            9 of 83687     1920 KB      57.9 ms     all cost, no benefit
   4 KB       13 123 of 83687     1952 KB      51.1 ms
  16 KB       13 123 of 83687     2096 KB      51.1 ms     144 KB for nothing
```

4 KB. The arm64 page is 16 KB and buys nothing beyond 4 KB; 1 KB is smaller
than the shift one edit produces, so it pays the space and stabilises nothing —
which is worth recording, because 1 KB is the value a reasonable person would
have picked without measuring.

**Where this leaves it.** 9 relocations to 13 123, and 55.2 ms to 51.1. Both
real, neither large: 84% of relocations still miss, because `__text` itself
changed size and everything after the edited crate's contribution *within* it
still moved.

**And the honest conclusion, which is the useful part.** Contribution slop
cannot close that gap. Reserving enough slack per contribution to absorb a real
code change means reserving kilobytes each, and a Rust link has thousands of
contributions — tens of megabytes of padding to save a few milliseconds. The
approach does not scale, and no amount of tuning makes it.

The spec already says what does: Stage E is "reuse the previous output layout
when possible … avoid moving unchanged content" — *reuse* it, not recompute it
with padding and hope the result matches. Place the changed object's sections
into a hole or at the end and leave every other address exactly as it was. That
is a different mechanism from the one built here, and it is the next one.

## 89. The profile, and what four turns of reasoning had missed

Everything above was reasoned from stage timers. Stage timers say *when* the
time goes, not *what* spends it, and after three findings in a row that
corrected an inferred priority it was worth getting the other view.

`cargo flamegraph` uses `dtrace` on macOS and needs root, which this project
does not ask for. `sample` needs nothing — but it needs a target that outlives
a 44 ms link, so `crates/link/examples/relink_loop.rs` links the same inputs
for a fixed duration and `sample` profiles that.

Self time, 10 seconds of continuous linking:

```
  2259  sha256::compress256                  the code signature
  1897  thread_start                         thread overhead
  1642  read                                 \
   908  open                                  |  file I/O: 2831
   281  close                                /
  1136  SipHasher::write        (blinker_link)
   626  memcmp
   466  malloc  +  453 free                  919 in the allocator
   351  memmove  +  292 memset
   282  reachability::Strip::remap
   239  reachability::plan
   167  yaml_rust2::scanner::fetch_plain_scalar
```

Four things here that no amount of staring at stage timers would have shown:

- **Signing is the single largest cost in the linker.** SHA-256 over every
  page of the image, recomputed in full every link. It is not "part of emit",
  it *is* emit.
- **File I/O is second**: `read` + `open` + `close` together outweigh it. This
  is the daemon's target, and the profile says so more clearly than the stage
  timer did.
- **SipHash is still 1136 samples after this session's fix**, which covered the
  three `(object, section)` maps. The rest are keyed by symbol *name* —
  `HashSet<String>`, `HashMap<&str, _>` — and the comment in `hashing.rs`
  saying names should keep the default hasher was written from caution, not
  measurement. It is wrong.
- **The `.tbd` stub's YAML is parsed on every link.** 167 samples inside a
  scanner that reads the same SDK file to the same answer every time.

And what the profile says is *not* worth chasing: `Strip::remap` and
`reachability::plan`, the two functions the plan had spent three turns aiming
at, are 282 and 239 samples — together under a fifth of signing.

**Two changes made before the profile existed, and what they were worth:**

```
  ObjectIndex (removing a quadratic scan)   +1.4 ms   (sd 2.8 — noise)
  FastHasher on the (object, section) maps  -2.5 ms   (5.3%)
```

The quadratic scan was real — `build_contents` searched all objects for every
contribution — and fixing it changed nothing measurable, because this workload
has 105 objects rather than the 900 the shape needs to bite. Kept as an
asymptotic fix and reported as the zero it measured, not as the win it looked
like.

**Acting on it.** The first item the profile named — the name-keyed maps the
comment had excluded on untested reasoning — was five lines. Every map in the
link crate now uses the fast hasher, name-keyed included:

```
  (object, section) maps only     -2.5 ms
  every map, names included       -5.2 ms   (9.7%)
```

The half the comment had ruled out was worth as much as the half it allowed.

**The rule.** Three findings in a row (87, 88, and this one) corrected a
priority that had been inferred rather than measured, and each inference was
reasonable. The stage timer was the best instrument available and it was still
pointing at the wrong stage. Getting a second instrument cost twenty minutes.

## 90. Signing was the largest cost and the most parallel work in the linker

Finding 89's profile put `sha256::compress256` at the top — above reading every
input from disk. It is also the most parallel thing the linker does: each
page's hash depends on that page and nothing else.

```
  serial page hashing     43.9 ms
  threaded                41.0 ms      -2.9 ms  (6.6%)
```

Determinism is by construction rather than by care: every slot writes to its
own index in a pre-sized vector, so no thread's timing can reach the output.
Two tests hold that — one comparing every slot against a serial `Sha256` of the
same page, one signing the same image twice and comparing the bytes.

**Less than the profile suggested, and the reason is worth recording.** 2259 of
~14 000 samples is 16% of self time, and this took 6.6% off the wall clock.
Profiler self-time counts where the CPU was, not what the wall clock was
waiting for — and the same profile has 2831 samples in `read`/`open`/`close`,
which no amount of threading the hashing overlaps with. A share of CPU time is
an upper bound on what removing that CPU time can buy, not a prediction.

Cumulative for the session's performance work, all interleaved against the
unchanged linker on the same 60-input link:

```
  fast hashing, (object, section) maps    -2.5 ms
  fast hashing, names included            -2.7 ms
  threaded page hashing                   -2.9 ms
  quadratic scan in build_contents         0    ms   (kept; asymptotic only)
                                          -8.1 ms of 46.3   (17.5%)
```

What remains at the top of the profile is the part threading cannot touch:
file I/O, which is the daemon's problem, and the allocator, which is the
owned-`String` representation finding 82 named.

## 91. Caching the stub parse: a null result, in the case it was built for

Finding 89's profile put 167 samples in `yaml_rust2`'s scanner — `libSystem.tbd`
is YAML, it is the same file on every link, and `dynamic_symbols` parsed it
once per link. Memoising it by path is twenty lines.

The measurement, in the scenario that most favours it — one process linking
the same inputs repeatedly, so the cache is warm for every link after the
first:

```
  before   217 links in 6s
  after    209 links in 6s
```

No gain, and if anything a loss. 167 of ~14 000 samples is 1.2%, which is
under the run-to-run spread of this measurement, and the added `Mutex` is not
free. Reverted.

Two things worth keeping from it:

- **The profile's smallest entries are not a work queue.** A share of self time
  that small cannot be distinguished from noise by the only instrument that
  can confirm it, so acting on it is faith either way.
- **The scenario chosen to measure in was the one that could only flatter the
  change** — repeated links in a single process, where the cache hits every
  time after the first. It still showed nothing. A null result under
  favourable conditions is worth more than a positive one under vague ones.

## 92. Two null results in a row, and what separates them

`Frontier::absorb` cloned every symbol name before testing whether the set
already held it, which in a Rust link means allocating and discarding for the
great majority of symbols. Probing first:

```
  run 1   -0.6 ms
  run 2   +0.7 ms
```

Two interleaved runs disagreeing in sign. Zero, on this workload.

Kept, where finding 91's stub cache was reverted, and the difference is worth
being explicit about because "it measured nothing" is true of both:

- The stub cache **added machinery** — a global, a `Mutex`, a lock ordering to
  reason about — to buy nothing. That is a net loss even at zero.
- This is a **reordering of two lines** with nothing new to go wrong, targeting
  a cost that *was* measured at 5.9 ms, on a 921-object link (finding 83) that
  is not the one set up here.

Neither is kept because a number says it helps. One is reverted because it
costs something and the other is not because it does not, and both say so in
the code rather than leaving a future reader to infer a win.

**The workload is now the limiting instrument.** Three of this session's last
four changes landed at or under its noise floor of roughly ±1 ms on 41 ms. The
next performance question worth asking is not which micro-cost to attack but
whether the 60-input benchmark can still see the answer — finding 77 recorded
the same failure one scale down, when a twelve-object fixture could not show a
quadratic linker.

## 93. The benchmark could see the answer; the harness was measuring the wrong thing

Finding 92 ended by asking whether the 60-input link could still resolve a
2% change, and named a 921-object workload as the thing that would settle it.
Building that workload turned up two answers, and the first one was not about
scale at all.

### Every workload this project ever measured had already evaporated

`corpus/` holds thirteen recorded invocations. Every one of them names an
inputs directory, and **all thirteen are gone** — they were archived under
`/private/tmp/.../scratchpad/`, which is where the operating system reclaims
things. The records survived because they are small text. The 89 MB of object
files they describe did not.

So the workload behind findings 77–83 could not be re-run, and neither could
any of the twelve others. That is not a filing accident: it is why finding 92
had to leave its question open, and it made every number in this file a claim
nobody could check.

`scripts/workload.py` builds one from nothing but the repository and cargo. It
drives the machinery that already existed for this — `--blinker-record-invocation`
archives inputs precisely because rustc deletes them the instant the link
returns — and writes `target/workloads/<name>/`, which is gitignored but lives
in the repository rather than in a temporary directory.

```
  self         61 files    681 objects   59.5 MB   (release)
  self-debug  132 files    745 objects   34.6 MB   (debug)
```

Three details in that script are load-bearing rather than tidy: the linker is
**copied** before the build, because capturing this repository's own link has
cargo rewrite the binary it is executing; the build gets its **own target
directory**, so capturing a workload cannot invalidate the repository's build;
and the result is **verified to link with both linkers** before it is written,
because a workload that fails produces timings, and produces them fast (75).

### The noise was in the harness, not the workload

The new workload's first measurement spread 42% around its median — far worse
than the 60-input link it was meant to replace. It was not the workload:

```
  wall clock (spawn to exit)      sd 1.5 ms on 41    spread 17%
  the link itself (own record)    sd 0.5 ms on 31    spread 11%
```

Twenty of the sixty milliseconds are process spawn, dyld, and the kernel
handing over 59 MB of page cache. Real cost — but cost that does not move when
the linker moves, so it is variance laid on top of the signal. Every A/B in
findings 84–92 was taken through it.

Measured from blinker's own record instead, **the noise floor is ±0.3 ms on a
31 ms link**, roughly 1% — a sharper instrument than the workload it replaced,
not a blunter one. `scripts/ab.py` now reports both arms, and running it with
the same binary twice measures the floor directly.

### With that fixed, the answers

The session's four optimisation commits, re-measured at scale:

```
                        before    after     delta
  60 inputs             46.3      38.2      -8.1 ms   -17.5%
  681 objects           35.8      29.9      -5.9 ms   -16.5%
  745 objects, debug    39.6      33.1      -6.5 ms   -16.4%
```

Stable to within half a percentage point across three workloads and two build
profiles. The changes were proportional, and the small workload had been
telling the truth about them.

And the change finding 92 kept as an explicit null result, `Frontier::absorb`,
re-measured on the workload whose absence was its defence: **+0.5 ms at 681
objects, +0.1 ms at 745**, against a 0.3 ms floor. It does not help at any
scale reachable here. It stays, for the reason already recorded — no new
machinery — and its comment now says it was retested rather than leaving the
921-object figure standing as an unpaid promise.

### The rule

Finding 77 said a fixture is a claim about scale and it expires. This is the
other half: **a workload that cannot be rebuilt has already expired, and a
harness is part of the instrument.** The 42% spread was not noise to average
away over more iterations; it was 20 ms of process startup being measured as
though it were linking, and no amount of interleaving removes a constant that
large. Measure the thing that changes.
