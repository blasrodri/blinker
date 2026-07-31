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
