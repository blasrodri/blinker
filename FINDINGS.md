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

## 94. The cache cannot win at its current design point, at any hit rate

An architectural review made the case that blinker is "a fast full linker with
a post-layout relocation cache", not an incremental linker, and that the cache
is consulted so late that even perfect reuse has a low ceiling. The reviewer
could not run the benchmarks. With the workload of finding 93 that could now be
answered, and the answer is worse than the argument: the ceiling is not low, it
is **below zero**.

```
  one-crate edit, cache off    31.9 ms
  one-crate edit, cache on     42.1 ms      reused 1/108 objects, 0% of relocations
  cold link                    30.5 ms
```

Asking for the cache costs **10.2 ms** on an edit relink, and the edit relink
is slower than linking from cold either way.

### Where the 10.2 ms goes, now that the record says

The four cache stages are newly named, because every one of them was charged to
something else: loading and planning to `relocate`, building and storing to no
stage at all — they run after `emit_ms` stops, and appeared only as
"unmeasured".

```
  cache load    0.62      relocate, cache off    5.58
  cache plan    0.24      relocate, cache on    11.89
  cache build   0.62                            ------
  cache store   1.19      the bookkeeping        6.31
                ----
                 2.67
```

**Two thirds of the cost is not the cache's I/O — it is the bookkeeping the
cache forces on the stage it exists to accelerate.** `apply_relocations` takes
a flag saying a cache is being built, and under it records each object's
patched byte ranges; that doubles the stage. So the trade is 10.2 ms spent to
save at most 5.6 ms, which is the entire relocation stage. **No hit rate makes
this profitable.** 80% reuse — the best ever recorded (79) — saves 4.5 ms.

### And the hit rate is not a property of the edit

The first pairing of captures for this measurement reported 78% of relocations
reused. It was wrong, and the way it was wrong is worth keeping: the two
captures had been recorded into differently-named directories, that path
travels in `RUSTFLAGS`, rustflags feed the crate metadata hash, and so *every
rlib in the second build had a different filename*. The harness paired them by
name, matched only the few that happened to coincide, and alternated a link
that was not the edit it claimed to be.

With both builds recorded through one fixed path — same rustflags, same
filenames, only the genuinely-changed inputs differing — the same edit reuses
**9 of 84 116 relocations**. That is the number finding 88 recorded *before*
the section-stride padding was added, which says plainly that the padding does
not survive a 14-input blast radius: touching one crate that everything depends
on changes fourteen rlibs, and their combined size delta walks straight through
a 4 KiB stride.

### What this settles

Findings 79 and 88 each concluded that improving the reuse *rate* was not where
the win was. This is the sharper version: at the current design point the cache
is a cost centre with no achievable payoff, because it is consulted after
parsing, resolution, dead-stripping, layout and content assembly have all
already run, and it makes one of the remaining stages twice as expensive in
order to be consulted at all.

Padding a recomputed layout is not layout reuse. The next thing built is the
allocator that consumes the previous placement table — and with it, the
decision that an incremental output need not be byte-identical to a cold one
(D5), because a history-dependent allocator cannot be and still do its job.

## 95. Two mechanisms called one name, with opposite economics

`--blinker-cache` meant two things at once:

- **replay an unchanged image** — skips the entire linker, costs a fingerprint
  check, and is the whole of the no-op rebuild case;
- **reuse individual objects' relocated bytes** — skips one stage of six, and
  to be *able* to, makes every link record what each object read.

Finding 94 measured the second at 10.2 ms of cost against at most 5.6 ms of
saving. They are now separate: `--blinker-cache` does the first, and
`--blinker-cache-relocations` adds the second and says in its help text that it
measured slower than not using it.

```
  edit relink, no cache          31.9 ms
  edit relink, --blinker-cache   37.5 ms      was 42.1
```

The per-object machinery is kept, and not out of sentiment: it is the record of
what each object read, which is exactly what the retained-placement allocator
needs to know when an address it depends on has moved. What it cannot do is
carry that cost on every ordinary link while returning nothing.

The tests that assert per-object reuse now ask for it explicitly, which is the
part worth keeping: nine tests failed the moment the default changed, each
naming the behaviour it depended on. A default that changes silently under a
green suite is the failure mode finding 64 recorded — a cache that had stopped
working while every test passed.

## 96. Reading the previous layout back: 0% to 77% on the edit padding could not hold

Finding 94 measured a one-crate edit with a fourteen-rlib blast radius reusing
**9 of 84 116 relocations** — the number from before the section-stride padding
existed, which is what said the padding had stopped helping. The retained
allocator, wired end to end, on the same edit:

```
  padding, recomputed layout        9 / 84 116     0%
  previous layout, read back   65 403 / 85 396    77%
```

Nothing about the edit changed. What changed is that an unchanged
contribution's address is now looked up rather than arrived at.

### The three pieces it took, and why they landed separately

- **Identity** (`crates/link/src/identity.rs`) — path, archive member, section
  name and ordinal, hashed. Not `ObjectId`, which is assigned by input order
  and names a different object the moment one fewer archive member is pulled
  in.
- **The table in the cache** — the one thing in that file which is not a copy
  of something the image already holds. Where a contribution sat is a
  *decision*, and a decision cannot be recomputed; recomputing it with padding
  and hoping is what finding 94 measured.
- **The allocator** (`crates/layout/src/reuse.rs`) — kept slots occupy their
  whole reservation, the rest is a free list, and sections whose shape carries
  meaning are still packed from the start because a hole in `__eh_frame` is a
  zero-length record.

Each landed with its own tests, and the last commit's only new thing was the
wiring. That is deliberate: three untested pieces meeting for the first time
inside one commit is a debugging session, not an integration.

### What it has not yet bought

**Nothing, in time.** The edit relink is 37.8 ms, the same as before the
allocator existed, because reuse is gated on the per-object relocation
machinery that finding 95 turned off for costing more than it saves. The 77% is
visible only with `--blinker-cache-relocations`, and under that flag the link
is *slower*, exactly as measured.

That is the expected shape and worth stating plainly rather than reading the
77% as a win: retained placement is the precondition for reuse being worth
anything, not the mechanism that collects it. What collects it is persisted
per-contribution fixups — relocating only what changed instead of recording
everything in order to skip some of it — and that is the next milestone.

### The test that had to change, and the one that did not

One test compared an incremental output to a cold one byte for byte. It now
compares behaviour: same stdout, same exit status. That is D5 arriving in the
test suite — an incremental link keeps unchanged contributions where the
previous link put them, a cold link has no previous link, and the two differ by
design. The claim underneath it — that an object is *not* reused across a move
of a symbol it reads — is unchanged and still enforced, because a stale reuse
shows up as a wrong answer.

The harness also earned its keep: pointed at a stale workload it failed with
undefined symbols rather than reporting a time. Finding 75 is why it checks.

## 97. The invariant found the hole three commits after it was predicted

The placement counter was added to turn "77% of relocations reused" into the
property it stands in for. It broke on the first real link it saw:

```
  placement: 555/692 contributions kept their address (80%), 137 moved
  of those, 48 belonged to inputs that did not change
```

Forty-eight contributions of files that are byte-for-byte what they were last
link moved anyway. The 77% had been hiding it, because a hit rate averages a
failure away and an invariant does not.

### Why they moved

Dead-stripping. An unchanged object's *live set* is not a property of that
object — it is a property of what reaches it, and something else changing can
make more of it live. Its contribution then grows past a capacity sized from
the previous link's smaller live size, and it has to move.

This is exactly the boundary an architectural review named before any of the
allocator was built: "an unchanged object can still acquire a different live
set because another object changed its references." It arrived as a number
three commits later, which is the right order — the prediction was cheap and
the number is what says how much it matters.

### What it means for retained placement

Retained placement holds for what fits, and "unchanged input" is not the same
condition as "unchanged size". The fix is one of two things and not a third:

- **Capacity absorbs liveness churn** — reserve against the object's *full*
  size rather than its stripped size, so stripping less of it later still
  fits. Costs image size in proportion to what dead-stripping removes, which
  on a Rust link is most of it.
- **Liveness becomes incremental** — persist the reachability result per
  contribution, and recompute only where the graph changed. Larger, and the
  only version that also removes the 4.6 ms the stage costs.

What it is **not** is more padding chosen to make today's number go green.
That is finding 94 again, and the counter that just caught this is the thing
that would catch it.

## 98. Finding 97's explanation was wrong, and the counter is what said so

Finding 97 read the 48 moved-but-unchanged contributions and concluded
dead-stripping had changed their live size: an unchanged object keeping more of
itself, outgrowing a slot sized from last link's smaller live set. It was a
good story, it matched a prediction made before the allocator existed, and it
was wrong.

The fix it implied — reserving capacity against each contribution's full
unstripped size — was built, and moved the number from 48 to 43. That is the
shape of a wrong hypothesis: an expensive change (image size in proportion to
everything dead-stripping removes) buying almost nothing.

So the movers were asked where they lived, instead of being reasoned about:

```
  93  __TEXT,__eh_frame   fits=true
  33  __TEXT,__literal8   fits=true
   3  __TEXT,__literal16  fits=true
```

**Every single one fits its slot.** Capacity was never the problem. Not one of
them is in a section the allocator is willing to retain at all, because
`may_be_padded` — an allow-list written to answer *"may this section carry
padding"* — had quietly become the answer to a second question it was never
asked: *"may a contribution here keep its address"*.

Those are not the same question. A literal pool is referenced by relocations
naming addresses, so a gap between literals is never read and never walked —
exactly the property `__const` and `__cstring` have, and exactly not the
property `__got` has, where position *is* the index. The literal sections were
excluded by an omission, not a decision.

```
  placement, before   557/689 kept, 48 unchanged movers
  placement, after    591/689 kept, 31 unchanged movers
```

### What is left, and why it is different

Every remaining mover is `__eh_frame`, and that one is not an oversight.
`__eh_frame` is a chain walked by the length each record begins with; a hole in
it is a record header made of zeroes, and a binary with one links, runs, and
dies at the first unwind. It cannot be hole-filled, so it cannot be retained by
this allocator — retention *creates* holes the moment something ahead of it
shrinks. Making its contributions keep their addresses needs the section
rebuilt as a chain rather than allocated as space, which is a different
mechanism and belongs with the unwind work.

### The rule

**A predicted cause and a measured one are not the same evidence, even when the
prediction was good.** The liveness explanation came from a careful review of
the architecture, it was written down before the code existed, and finding 97
adopted it on sight because the number arrived exactly where it had been
foretold. Fifteen lines of "print what moved and whether it fit" refuted it in
one run.

The counter earned its keep twice over: once by catching a failure the 77% hit
rate had averaged away, and once by refusing the story told about it.

## 99. Optimising against a profile that a previous fix had already invalidated

Incremental code signing was built: page hashes carried in the cache, a
previous image paired with them, and every page whose bytes are unchanged
taking its hash from the last link instead of being hashed again. It is
correct, it is tested, and on the edit relink it is worth **nothing
measurable** — emit stayed at 5.9 ms.

The reason is the evidence it was chosen from. `sha256::compress256` was the
largest single item in the profile that drove this session — 2259 samples,
more than reading every input from disk — and that profile was taken *before*
page hashing was threaded. Threading it measured -2.9 ms at the time. Signing
a 1.8 MB image across ten cores is now a few tenths of a millisecond, and an
incremental path can save at most what is being spent.

So the target was picked from a number that one of this session's own commits
had already made false. Findings 77 and 93 are both about a measurement
expiring; this is the same failure at the smallest scale — **a profile expires
the moment you act on it**, and the one that justifies the next change has to
be taken after the last one.

### It is kept, and why that is not sunk cost

Two reasons, neither of them "it is already written":

- It removes an obstacle to output patching. That milestone writes changed
  ranges into a clone of the previous binary and never materialises the whole
  image in memory — at which point there is nothing to hash and the page
  hashes must come from somewhere. They now do.
- It costs nothing to carry: no global state, no lock, one field in the cache,
  and a comparison that is two orders of magnitude cheaper than the hash it
  skips.

Finding 91 reverted a null result because it added a global and a mutex.
Finding 92 kept one because it added nothing. The rule is unchanged, and this
is the second kind.

### Two bugs the tests caught on the way

**The pair was not checked.** The first API took an image and hashes as two
public fields. Hand it an image from one link and hashes from another, and
every page they happen to share takes a hash describing different content — a
signature that will not verify, which is a binary the kernel refuses to run.
The test wrote that mistake deliberately and the code accepted it. The fields
are now private behind a constructor that samples the pairing.

**The pair was validated against the wrong link.** The constructor required the
previous hash count to match the *current* image's page count. That rejects
every edit that changes the image's size, which is every edit. The reuse path
was wired end to end and fired on nothing — and the emit time not moving is
exactly what that looks like from outside, which is why it took a second look
to tell it apart from the answer above.

## 100. A profile cannot see that work is hidden, and that is why finding 91 was null

Re-profiling after the signing work — because finding 99 had just established
that a profile expires when you act on it — put `yaml_rust2::parser` third,
above `sha256`:

```
  3082  blinker_link::load_one
  3033  std::fs::read::inner
  1546  yaml_rust2::parser          <- .tbd stub parsing
   967  sha256::compress256         <- was 2259 before threading
   962  open
   809  LinkRequest::dynamic_symbols
```

Parsing the SDK's `.tbd` stubs is YAML parsing of files that never change,
which reads as the most obviously wasteful thing in the link. Finding 91 built
a cache for exactly that, measured 217 → 209 links per six seconds, and
reverted it as a null result.

Both are true, and the reason is that `sample` counts CPU across threads while
a link is measured in wall clock. The stub parse already runs on its own
thread, alongside reading the objects. Timing the two halves separately rather
than the pair:

```
  read_and_parse   8.24 ms      the whole overlapped stage
  stub_parse       6.28 ms      of which this is hidden inside
```

**Overlapped work is free only while it is the shorter half**, and a profile
cannot distinguish a thread that is entirely hidden from one setting the pace —
they produce identical sample counts. Finding 91's cache removed CPU that
nothing was waiting for.

### What the number is actually for

6.28 of 8.24 ms means the stub parse is 76% of the stage it hides in. It is
free now and it stops being free the moment reading the objects gets faster,
which is precisely what the daemon is for. Cache the stub parse first and it
measures nothing; make reading fast first and the same cache is suddenly worth
most of what remains.

So the ordering is not "cheapest first" or "biggest sample count first" — it is
**whichever half is longer**, and that is a question a profile cannot answer
and two `Instant::now()` calls can. `stub_parse_ms` is now reported next to the
stage it lives in, so the day it becomes the longer half is visible rather than
deduced.

## 101. Copying a relocated byte costs more than relocating it

Finding 94 measured the per-object relocation cache at 10.2 ms of cost against
5.6 ms of possible saving, and switched it off. The reason given was that reuse
was 0%: the layout kept moving, so nothing could be reused. Retained placement
has since taken reuse to 73%, so the measurement was worth repeating.

```
  edit relink, image replay only     36.3, 36.7 ms
  edit relink, + relocation reuse    40.3, 39.1 ms
```

Still negative, by 2.4–4.0 ms. And the stage itself says why:

```
  relocate, no cache          7.6 ms     every object relocated
  relocate, 73% reused       10.0 ms     three quarters of the work skipped
```

**Skipping three quarters of the work made the stage forty per cent slower.**

That is not overhead around a good idea; it is the idea. Applying a relocation
is a handful of arithmetic on bytes that are already in cache — a load, an add,
a store, on the same cache line as the last one. Reusing it means decoding a
cache file, looking up the object's ranges, and memcpy-ing the identical bytes
into the same place. The copy moves as many bytes as the arithmetic did, and
does not save the arithmetic's cache line.

### What this eliminates from the plan

The next milestone was to be "fixup ownership": persist per-contribution
dependencies and relocate only what changed. Half of that is still right — not
*walking* an unchanged object's relocations is a real saving. The other half,
keeping its relocated *bytes* to copy back in, is now measured as a loss twice
over, at 0% and at 73% reuse.

Which points past it. The only way an unchanged contribution costs nothing is
if nothing touches its bytes at all — no relocation, no copy, no write. That
means the previous output file *is* the byte store: clone it, patch the ranges
that changed, and leave the rest of the file untouched on disk. Unchanged
contributions are then not fast, they are absent.

That also settles what the cache should hold. Storing every patched section
*and* the finished image is storing two copies of bytes the output file already
contains, and the measurement above says the second copy has no reader worth
paying for. What is worth persisting is what cannot be recomputed: the
placement table, input identities, and which contribution owns which output
range.

### The rule

**A cache that saves an operation cheaper than a memory copy cannot win.** The
question to ask before building one is not "how often will it hit" — the hit
rate here went from 0% to 73% and the answer did not change sign — but "what
does the work cost, and what does the lookup cost?" Relocation is one of the
cheapest things a linker does per byte. It was chosen as the first thing to
cache because it was easy to attribute per object, not because it was
expensive.

## 102. The UUID was hashing the whole image, next to a signature doing it better

`emit` was 5.9 ms and had never been broken down. Splitting it — four
`Instant::now()` calls — put almost all of it in one place, and then splitting
*that* found the actual culprit:

```
  emit_layout       0.16 ms
  emit_contents     0.00
  emit_linkedit     0.55
  emit_assemble     0.19
  emit_uuid         4.43        <- a single-threaded SHA-256 of the whole image
  emit_sign         0.60        <- the same bytes, threaded and incremental
```

`LC_UUID` is content-derived, and the way it was derived was `Sha256::digest`
over the entire 1.8 MB image. Immediately after it, the code signature hashed
**the same bytes** — in 16 KiB pages, across ten threads, skipping pages
unchanged since the previous link. One of those took 4.43 ms and the other 0.60.

Deriving the UUID from the page hashes instead makes it a Merkle root over the
same content. Identical bytes still give an identical UUID and different bytes
still give a different one, which is the whole of what `LC_UUID` promises — and
it inherits the threading and the reuse for free.

```
  emit_uuid     4.43 -> 0.65 ms
  emit          6.51 -> 3.24
  the link     30.6  -> 25.9      -4.7 ms, -15.5%
```

Two interleaved runs, sd 0.6/0.7 against a noise floor of 0.3.

### Why it hid for so long

Nothing was wrong with either piece on its own. `content_uuid` is six lines and
obviously correct; the signature is careful, threaded and now incremental. The
waste was entirely in their *relationship* — two full hashes of one buffer,
written months apart, each reasonable in isolation.

A stage timer would never have shown it: `emit` is a legitimate stage and 5.9 ms
is a legitimate size for it. It took splitting a stage that nobody suspected,
for no reason other than that the next milestone was going to be chosen from
it. Findings 99 and 101 are both cases of choosing a target from a number too
coarse to hold the answer; this is what asking one level down costs, and what it
returns.

**The largest remaining item in a profile is not the largest remaining
opportunity. The largest opportunity is in whatever has never been measured at
the resolution the decision needs.**

## 103. The whole link, measured at the resolution a decision needs

Finding 102 came from splitting one stage nobody suspected. Applying that to
every remaining stage took an afternoon of `Instant::now()` calls and leaves
this — a one-crate edit relink, every item over a millisecond named:

```
  read+parse        7.6      of which .tbd YAML parse   6.0  (overlapped)
  relocate          6.4      apply       3.5
                             synthetic   1.5
                             address map 0.8
                             contents    0.2
  dead-strip        3.7
  layout            3.7
  resolve           3.1
  emit              2.5      uuid        0.7
                             sign        0.6
                             linkedit    0.5
                             layout      0.2
  cache             1.3
  survey            1.0
  symbols           0.7
  unmeasured        2.4
                   ----
                   ~30 ms
```

`unmeasured` was 13% before this and is 8% now; everything above a millisecond
has a name.

### What the shape says

**There is no big item left.** The largest is a stage that is really two
overlapped halves, and the largest single job in the link — applying every
relocation — is 3.5 ms, 12%. Nothing here can be halved into a 15% win the way
the UUID could.

That is the answer to "is 35 ms a lot of I/O": no. Reading 60 MB out of the
page cache, threaded and now mapped rather than copied, is a couple of
milliseconds. The link is 30 ms because it does eleven different things costing
1-7 ms each, and *all of them* are work a cold link genuinely has to do.

### Which is the argument for the daemon, stated properly

An incremental link should not be a cold link made faster. Every item above is
recomputed from scratch on every invocation, and for an edit touching one crate
almost all of it is recomputing the same answer from the same bytes:

- **read+parse** — the same 60 MB, of which ~2 MB changed.
- **the .tbd parse** — the same SDK stubs, which never change.
- **dead-strip, resolve, survey** — the same graph, from the same symbols.
- **layout** — already incremental (retained placement).
- **apply, emit** — genuinely proportional to what changed, once the rest is.

Nothing on that list gets to zero by being optimised. They get to zero by not
running, which needs the answers to still exist when the next invocation
arrives — and that is a resident process, not a cache file. A cache file can
only hold what is cheap to serialise and cheaper to read back than to recompute,
and finding 101 is what happens when that test is failed.

**The remaining work is not performance work. It is a change of process
model.**

## 104. Residency, and what it turns out to be worth

Finding 103 ended by saying the remaining work was not performance work but a
change of process model. Here is what that change is worth, measured on the
resident loop — the same 61 inputs linked over and over in one process:

```
                             per link     links in 6 s
  before                       41.9 ms         212
  parses held                  21.3            251
  extraction order held        19.2            312
```

**2.2x, from not doing work whose answer had not changed.**

The pieces, in the order they mattered:

- **Archive members.** 685 of the objects in this link come out of 19 rlibs.
  Holding the archives' bytes without holding what was parsed out of them left
  the larger half of the work in place: `read+parse` went from 10.7 to 5.0 ms
  only once members were held too.
- **The extraction frontier.** With every parse held, the whole remaining cost
  of the stage was the fixed point that decides *which* members to pull in —
  rounds of "which names are still undefined, which archive defines one" over
  every symbol of every object. It is a pure function of symbols that did not
  change, so a link where nothing was re-read replays the order the last one
  settled on: 5.0 ms to 1.4.
- **The `.tbd` stubs.** Finding 100 measured this at 6.0 ms of the 7.6 ms stage,
  hidden behind reading the objects, and predicted it would stop being free the
  moment reading got faster. Holding it was one field.

### What separates this from finding 101

Finding 101 killed a cache: relocated bytes, serialised to a file and read back,
losing to memcpy because relocating was cheaper than copying. The rule it left
was "a cache that saves an operation cheaper than a memory copy cannot win".

Nothing here is copied or decoded. An `Arc<ParsedObject>` handed to the next
link is the same allocation, in the same shape, and the cost of proving it still
valid is a `stat`. That is the difference between *persisting* an answer and
*keeping* one, and it is why the same idea fails in a cache file and works in a
process.

### Two bugs, and what each one says

**A held member was given the whole archive as its byte window.** Every section
then read from the wrong offset. It failed loudly — a misaligned relocation on
the second link — and only because the offsets happened not to line up. A member
whose sections landed somewhere plausible would have produced a running,
*wrong* binary, and every test asserting "the link succeeded" would have passed.
The test written afterwards compares bytes against a cold link and runs the
program.

**The hit counter did not count the interesting misses.** Both early exits — no
entry, and a failed probe — returned before the counter was touched, so a link
that correctly re-read a changed archive reported reading nothing. A counter
that undercounts misses is worse than no counter: it makes a cache look perfect
exactly when it has stopped working, which is finding 64 with a different
mechanism. Caught by a test asserting `inputs_read > 0` after an edit.

## 105. The daemon was slower than no daemon, because of a sleep

Residency measured 2.2x in-process (104). Put behind a socket and measured end
to end — a real process spawn, a real link, the client printing and exiting:

```
  direct   38.1 ms
  daemon   48.7 ms      +10.6 ms
```

**Worse than not having one**, and by almost exactly the average of a 20 ms
poll interval. The first `serve` loop used a non-blocking `accept` with
`sleep(20ms)` between attempts, so every request waited an average of 10 ms for
the daemon to notice it had arrived.

`std` has no accept timeout, and both obvious alternatives are wrong: a
blocking `accept` never notices it should exit when idle, and a polling one
trades latency for that. `SO_RCVTIMEO` on the listening socket gives both — an
`accept` that returns the instant a client connects and gives up after a
second.

```
  direct   36.5 ms
  daemon   28.1 ms      -8.4 ms, -23%
```

### What it says

The sleep was invisible in every test: the protocol tests pass, the end-to-end
tests pass, the binary is byte-identical. It cost a third of the link and
nothing was wrong.

A linker's unit of work is tens of milliseconds. Anything in the request path
measured in *milliseconds* — a poll interval, a retry backoff, a lock timeout,
a sleep waiting for a file — is not a small constant, it is a large fraction.
The instinct that a 20 ms poll is "fast enough" comes from services where the
work takes seconds; here the work takes 19.

### What is left in the 28.1 ms

About 19 ms of it is the link. The rest is the client: a process spawn, dyld
loading a 1.7 MB binary, and the socket exchange. `rustc` spawns a linker per
crate and there is no avoiding *a* process — but there is no reason it has to
be this one. A client that only speaks the protocol would be a fraction of the
size, and that is the next thing worth measuring rather than assuming.

## 106. The fourteen-crate blast radius was the harness rebuilding from scratch

Findings 94, 97 and 98 all reason about a one-crate edit that changes
**fourteen of sixty-one inputs**. Every design decision downstream took that
as the shape of the problem: retained placement was judged against it, the
extraction replay was disabled by it, and finding 98's remaining
`__eh_frame` movers were counted under it.

It was an artifact of the measuring instrument. `scripts/workload.py` gave each
capture its own cargo target directory, so the second capture built the entire
project from scratch — and rustc, building the same source twice from nothing,
emits codegen units in a different order. Thirteen crates the edit never
touched came out with different bytes and different member ordering inside
their rlibs.

Sharing the build directory, so the second capture is the *incremental rebuild*
a developer's edit actually produces:

```
  from-scratch captures     14 of 61 inputs changed
  incremental captures       2 of 62
```

**Two.** With the session holding the rest: 60 inputs held, 2 read.

### What it invalidates and what it does not

The measurements stand — they were real links of real inputs. What changes is
what they were measurements *of*: not "a one-crate edit" but "a one-crate edit
plus a from-scratch rebuild of everything else", which is a different and much
harder problem, and not one a build system poses.

So finding 98's conclusion — that the remaining moved contributions are all
`__eh_frame` — was measured on a blast radius seven times too large. The
`__eh_frame` reasoning holds (it cannot be hole-filled), but how much it costs
is now an open question rather than a settled one.

### The rule, again

Finding 77 said a benchmark fixture is a claim about scale and it expires.
Finding 93 said a workload that cannot be rebuilt has already expired. This is
the third form: **a harness that produces its inputs differently from the way
production produces them is measuring a different system.** The from-scratch
rebuild was not a shortcut or an approximation — it was the harness
manufacturing a workload that no build would ever hand a linker.

It cost more than a wrong number. Three sessions of design were spent on the
question "how do we retain addresses when fourteen crates change at once",
which is a question nobody asked.

## 107. At the real blast radius the placement invariant is total, and the whole edit relink is 22 ms

Finding 106 left an open question: the `__eh_frame` movers, and everything else
finding 98 counted, were measured on a blast radius seven times too large. Here
is the same measurement on a pair of captures taken back to back, so the second
is the incremental rebuild the edit actually causes.

Editing a *private* function in `blinker-diagnostics` — the ordinary developer
change, and the one no exported symbol moves for — then relinking through a
resident daemon:

```
  blast radius: 2 of 62 inputs changed
  placement: 724/727 contributions kept their address (100%), 3 moved
  of those, 0 belonged to inputs that did not change — the invariant holds
  session: 60 inputs held, 2 read; extraction replayed, resolution held
```

**Three moved contributions, all of them in the two rebuilt rlibs.** Not one
unchanged input moved. The `__eh_frame` problem finding 98 spent a page on does
not appear at this blast radius: with two inputs changing, the packed
`__eh_frame` chain absorbs them and nothing downstream shifts.

That does not make `__eh_frame` retention wrong, it makes it **not urgent**. It
is a fourteen-input problem, and fourteen inputs is what a `cargo clean` does.

### Every held-state rule fires, and each is worth what it claimed

The three mechanisms built for this case — input residency, extraction replay,
and resolution holding — had never been observed firing together, because the
faked blast radius disabled all three. Cold link versus the same link through
the daemon, same machine, same minute:

| stage | cold | resident | |
|---|---|---|---|
| `read_and_parse` | 16.03 ms | 1.25 ms | 60 of 62 inputs held |
| `resolve` | 5.89 ms | 0.00 ms | resolution held |
| `stub_parse` | 11.82 ms | 0.00 ms | the SDK stub is parsed once, ever |

The link goes **42.1 ms → 22.4 ms**, and the wall through the client 52.7 → 30.2.

(Both numbers are inflated maybe 1.3x: the machine had a load average of 6 and
ld64 moved from its usual 28 ms to 36 ms alongside blinker. The ratio is what
survives that, and the counters do not care.)

### What is left, and what it says to do next

```
    relocate          6.71 ms   33.5%     apply 3.96, synthetic 1.52
    dead_strip        3.55 ms   17.7%
    layout            1.66 ms    8.3%
    emit              1.37 ms    6.8%
    symbols + survey  2.24 ms   11.1%
    cache             1.49 ms    7.4%
    read_and_parse    1.25 ms    6.2%
```

Relocation is now a third of the link, and it is applying **87,545
relocations** to produce an output in which 724 of 727 contributions sit at
exactly the address they sat at last time, from input bytes that did not
change. Those bytes are, necessarily, the bytes that are already in the
previous output.

This is the argument for output patching, and it is a different argument from
the one that killed the relocation cache in finding 91. That cached *relocated
section contents* and lost because copying a section costs about what relocating
it costs. Patching does not copy: the previous image stays in the session, and a
link writes only the ranges that are dirty. The work it avoids is not a memcpy,
it is `apply` — and now that the placement invariant holds at 100%, "dirty" is a
small and precisely known set.

## 108. The unmeasured time was preparation and a diagnostic

Finding 107's profile of the warm relink ended with `unmeasured 1.92 ms` — 10%
of the link, larger than `layout`, `emit` or `read_and_parse`, and belonging to
no stage. A budget cannot be built over a hole that size, so this closed it.

It was two things, neither of them a stage:

```
    prepare           1.07 ms     placements, __eh_frame personalities,
                                  unwind sizing, commons
    accounting        0.46 ms     counting the placement invariant
    unmeasured        0.31 ms
```

`prepare` is real work that fell between `dead_strip` and `layout` because it
was never anybody's stage. It scans every object's `__eh_frame` for personality
fields, sizes the unwind table, and collects common symbols — all of it global,
all of it recomputed per link, and all of it as incrementalisable as the stages
on either side.

`accounting` is not link work at all. It is the counter behind
`contributions_retained`, and it walks every contribution *and probes every
input* — and probing an input whose path does not identify it means hashing its
contents. A measurement that costs half a millisecond of the thing it measures.
It is now computed only when something has asked for diagnostics.

### The rule

Stage timers measure the stages someone thought to name. The gap between them is
where work accumulates precisely *because* nobody named it — every addition
lands in `unmeasured`, and `unmeasured` is the one line a profile reader skips.
The fix is not more timers, it is a timer that **must** sum: `unmeasured` is
printed as a line item so that a stage nobody owns still shows up as somebody's
problem.

## 109. QoS is a 3-5x cliff and the linker cannot defend against it

Darwin schedules by quality-of-service class and a process inherits its class
from whoever spawned it, which for a linker is always someone else. This
machine is an M5 Pro — 5 "Super" cores and 10 "Performance" cores — so the
question is not academic: if the inherited class cannot reach the fast cores,
every threaded stage is on the wrong ones.

Same link, same machine, under `taskpolicy`:

```
  inherited              28.7 ms
  background            141.0 ms      <-- 5x
  utility                28.6 ms
  user-initiated         27.6 ms
  user-interactive       27.6 ms
```

The obvious move is for blinker to raise its own class at startup —
`pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0)`, one syscall. It
was written, and then A/B tested in one binary with an environment switch,
interleaved so drift hits both arms:

```
  normal                  qos-on  28.69   qos-off  28.71
  clamped to background   qos-on  90.38   qos-off  86.37
```

**It does nothing.** A QoS clamp is a ceiling, not a suggestion; a thread cannot
raise itself above the task's clamp. And with no clamp, `utility` and above are
already the same speed, so there is nothing to gain. The code was deleted.

### What is worth keeping

Two things. First, the cliff is real and it is *outside* the linker: a build run
under `nice`, or as a background IDE task, or by a CI runner that clamps its
workers, gets a linker three to five times slower and nothing in the profile
explains it. That belongs in the documentation, not in the code.

Second — the reason to write this down at all — the one-syscall fix measured as
a 30% improvement (141 to 97 ms) on the first try, and it was noise. The
interleaved A/B in a single binary is what caught it. A change that costs one
line and *appears* to work is the hardest kind to reject, because nobody asks a
free change to prove itself.

## 110. The cache file was a resident process talking to itself through the disk

A link with `--blinker-cache` loaded a cache and stored one: read a few
megabytes, decode them, and at the end encode and write them back. Inside a
daemon both halves are the same process, one link apart. It encoded a structure
it was holding, wrote it, dropped it, and read it back to recover what it had
never stopped having.

The cache file is a **restart** mechanism. It exists so a cold process can pick
up where a previous one left off, and that is worth its cost exactly once.
Between two links in one process it is pure overhead:

```
                    before   after
  cache_load        0.59 ms   0.00 ms
  cache_store       0.58 ms   0.04 ms
```

The session now holds the cache and *takes* it — moved, not cloned, because it
contains the whole output image and a borrow would have meant copying two
megabytes to avoid copying two megabytes. The file is written on a session's
first link and left alone after that, so a restart still finds a usable cache;
it is one link stale, which costs one colder link and never a wrong one,
because every cache is validated against its inputs before it is believed.

### Where the warm relink stands

Everything in the link is now accounted for. 69 inputs, 900 objects, a
private-function edit that changes 2 of them, through the daemon:

```
  wall  24.9 ms      link  17.9 ms

    relocate          6.51      apply 3.62, synthetic 1.5, address map 0.8
    dead_strip        3.69      liveness 2.21, atoms 1.20
    emit              1.41
    read_and_parse    1.25
    layout            1.15
    prepare           0.99
    survey            0.93
    symbols           0.56
    cache_build       0.46
    accounting        0.31      diagnostics only; not in a production link
    unmeasured        0.33
```

There is no big item left, which is the point: every remaining stage recomputes
a global answer that barely changed. `relocate` applies 87,545 relocations to
produce an output in which 1060 of 1063 contributions sit where they already
sat. That is the next thing, and it is the last structural one.

## 111. Appending an item to a Rust file is not a body edit

`scripts/relink.py --body-only` claimed to measure the ordinary developer
change: edit a function, relink. It appended a private function to a source
file. Three variants were tried, and the address blast radius each produced —
how many of the link's symbol addresses moved — says they are three different
experiments, none of them the intended one:

```
  appended `#[allow(dead_code)] fn`         0 of 6842 changed   (0.00%)
  appended `pub fn`                       252 of 6877 changed   (3.66%)
  appended `#[used]`-kept private fn     8498 of 6877 changed   (123%)
```

The first is deleted again by dead-strip, so the edit never reaches the output:
the harness was timing an edit that does nothing. The third is the opposite —
adding an item repartitions rustc's codegen units, and symbols get renamed
across the whole crate, an edit *larger* than adding a public function. (Over
100% because the old and new name sets barely overlap.)

**Appending an item to a Rust file is not a body edit, whatever the item is.**
A body edit modifies code that already exists, adds no item, moves no codegen
unit boundary, and renames nothing.

There is no way to write one from outside the source, so the source now
contains a seam: `blinker_diagnostics::relink_seam`, a live function on every
invocation's path, whose one literal the harness rewrites. That measures:

```
  literal changed inside an existing function   24 of 6879 changed   (0.35%)
```

**0.35%.** That is the real shape of the problem an incremental linker is for,
and it is an order of magnitude smaller than the best previous estimate.

## 112. An ordinary edit renames a fifth of a crate's global symbols, and none of them matter

With the fixture fixed, the true body edit was *slower* than the fake one:
`read_and_parse` 1.27 ms to 5.60, extraction recomputed, resolution redone. The
session refused to replay because both changed rlibs reported a changed symbol
interface — on an edit that changes one integer literal.

Diffing the two rlibs says why. 29 of 134 global symbols differ, and all 29 look
like this:

```
  _anon.ed7a2420ca2a47dccd3066b2a97f7049.4.llvm.16877640159684202088
```

LLVM promotes an internal constant that needs an address to a module-level
symbol and gives it a name nobody chose. The trailing component is a hash of
*the module*, so changing one line renames every one of them.

No other crate can reference such a name — it is unpredictable by construction —
so renaming them cannot change which archive member satisfies anybody's
reference. They are excluded from the interface digest and from the archive
symbol-table comparison, for the same reason local symbols already were: they
are invisible to the extraction frontier, and they are exactly what an ordinary
edit churns.

```
                       before   after
  read_and_parse       5.60 ms   1.27 ms
  extraction          recomputed  replayed
  resolution          redone      held
  link                25.8 ms    19.6 ms
```

### The rule

Both of this session's two largest wins came from the same place: a *name* that
changes for reasons that have nothing to do with meaning. Finding 106 was
codegen-unit ordering; this is LLVM's module hash. An incremental linker is a
machine for deciding what changed, and it is only as good as its notion of
sameness — which means every identity it compares has to be checked against the
question being asked, not against byte equality. Byte equality is always
available and it is almost always the wrong test.

## 113. The relocation cache was never the problem; the file was

Finding 91 killed per-object relocation reuse, and finding 94 confirmed it: at
73% reuse it *lost*, because recording what each object read doubled the
relocation stage and copying cached bytes cost about what relocating them cost.
The conclusion drawn was "do not cache relocated bytes", and it was written down
as settled.

It was settled about the wrong thing. Every cost in that verdict belonged to the
cache **file**, not to the reuse:

- the entries were decoded from disk on every link and encoded back afterwards
- the cached section bytes were a `Vec<u8>` reconstructed per link
- and the recording ran unconditionally, including for the objects it would
  never help

Finding 110 moved the cache into the session. Running the same code, unchanged,
with the same flag, on the same workload:

```
                    file-backed   session-held
  apply                 3.67 ms       0.83 ms
  link                 19.6  ms      16.7  ms
  relocations reused          -   83,964 / 87,834  (96%)
```

**96%.** It is now on automatically whenever the session is resident, and off
otherwise — which is not a compromise but the actual shape of the economics.
Recording is an investment in the *next* link; a process about to exit has no
next link, so for a one-shot invocation the original verdict still holds exactly
as measured. `Session::is_resident` is the flag, and the daemon is the only
thing that sets it.

A test pins the property that makes this safe: two sessions link the same inputs
three times each, differing only in whether reuse was on, and the binaries must
match. Reused bytes that are subtly stale still link and still run — the failure
this can have is silent, so "it succeeded" is not the assertion.

### What this says about killed ideas

The measurement that killed relocation reuse was correct. What was wrong was the
scope of the conclusion: it measured *reuse through a file* and concluded
something about *reuse*. Both of the mechanisms that made it win — a resident
process, and holding the cache in memory — were built afterwards, for other
reasons, and nothing went back to ask whether the dead idea was still dead.

A rejected design should carry the conditions it was rejected under, and those
conditions should be re-checked when the system underneath them changes.

### Where the warm relink stands

```
  wall  21.6 ms      link  16.7 ms      (was 27.0 / 18.7 at finding 107)

    dead_strip        3.60      liveness 2.19, atoms 1.15
    relocate          5.18      apply 0.84, address table 0.78, plan 0.62
    emit              1.43
    read_and_parse    1.27
    layout            1.14
    survey            1.02
    prepare           1.01
    cache_build       0.66
    symbols           0.65
    unmeasured        0.14
```

Dead-strip is now the largest single item, and it is entirely global: it rebuilds
the atom graph and re-derives liveness over 900 objects to discover that 24 of
6879 addresses moved.

## 114. What is left is four independent walks over every relocation in the program

With the cache round-trip gone (110) and relocation reuse on (113), the warm
relink is 16.9 ms and fully accounted. Splitting the last composite stage —
`synthetic` — finishes the map:

```
    eh_frame          0.08 ms     repairing __eh_frame
    tables            0.02 ms     GOT, stubs, thread pointers
    unwind            1.36 ms     rebuilding __unwind_info
```

The indirect tables, which sound like the expensive part, are 20 microseconds:
they are proportional to the number of *slots*, and slots barely move.
`__unwind_info` is 1.36 ms because it is rebuilt from every function in the
program, every link.

That is the pattern in everything that remains. Sorted by cost:

```
    liveness          2.07     traverses the whole reference graph
    emit              1.51
    unwind-info       1.36     every function's unwind entry
    layout            1.04
    atoms             1.01     every section's atom boundaries, every relocation
    prepare           0.96     every __eh_frame section's personality fields
    survey            0.92     every relocation, to find GOT/stub/TLV needs
    apply             0.84     4% of relocations; 96% reused
    address_table     0.83     hashes every defined name
    address_map       0.71
    cache_build       0.67
    cache_plan        0.63
    symbols           0.63
```

**Four of these — atoms, prepare, survey, unwind — are independent walks over
the same 900 objects' relocations, each collecting something different, each
recomputed in full.** Together they are 4.25 ms, a quarter of the link. Every
one of them is a *pure function of a single object*: the atom boundaries of an
object depend on that object, its personality fields depend on that object, the
GOT names it needs depend on that object, its unwind entries depend on that
object. Two of 69 inputs changed.

So the next structural move is not four separate optimisations. It is one: a
per-object memo in the session, holding each object's projection alongside the
parse that is already held there. The session already proves an object unchanged
in order to hand back its `ParsedObject`; everything derived from that object
and nothing else is valid for exactly as long.

`liveness` (2.07 ms) is the one that does not fit — it is genuinely global,
and it is the hard problem left. `apply` at 0.84 ms is what this looks like when
it is solved: 96% of the work skipped, and what remains is proportional to the
edit.

## 115. The allocations were not the cost; the walk is

Two stages built hash sets keyed by `String` and asked them a question once per
*relocation* while getting a new answer once per *name*: `Atoms`' owner map
(~7,000 clones) and `survey_relocations`' four seen-sets (an allocation on all
87,000 relocations to record about a thousand distinct names). Both were changed
to borrow the names out of the parsed objects, which outlive both calls.

```
                before   after
  atoms         1.15 ms  0.96 ms
  survey        0.92 ms  0.87 ms
```

A quarter of a millisecond, most of it in `atoms`, and `survey` moved by less
than its run-to-run spread. **Tens of thousands of short-string allocations cost
almost nothing here**, which is worth knowing precisely because it is not what
the code looked like it was doing.

The changes stay — fewer allocations for less code is not a trade — but they are
recorded as *not a win*, so that nobody reads the diff later and infers that
allocation was the problem. The problem is the walk: `survey` is 0.87 ms because
it visits 87,000 relocations, and it will be 0.87 ms however cheaply it visits
them. The only thing that removes it is not visiting the 96% that belong to
objects nothing about has changed — which is finding 114's per-object memo, and
is a different change entirely.

## 116. A resident linker that got slower the longer it ran

Finding 110 had the session hold the cache and write the file only on its
*first* link, so a restart stays warm without every link paying to serialise a
few megabytes. The marker for "already written" was one boolean on the session.

One boolean is wrong, and only a daemon shows it. A resident session serves
**many different links** — every crate in a workspace, every project on the
machine. After the first of them wrote its file, `wrote_cache` stayed true
forever, so no other cache path was ever written. And the no-op fast path
(`reuse_finished_image`) reads that file. So every program linked after the
first one permanently lost the ability to skip a rebuild that changes nothing:

```
                              wall      link
  no-op rebuild, no daemon    6.70 ms   1.21 ms    replayed the cached image
  no-op rebuild, daemon      21.77 ms  15.67 ms    linked it all again
```

**The daemon was three times slower than no daemon** on the case a daemon should
win most. It was found by mislabelling a benchmark: an arm called "cold" was in
fact replaying a finished image, and noticing that the warm arm lost to it is
what exposed the bug.

Two fixes. The marker is now the *path* whose file has been written, so it is
once per program per session rather than once per session. And the fast path
consults the session's own cache before the file — which is not only cheaper but
more correct: once a session holds a cache, the file is the one its *first* link
wrote, and a link reusing a previous layout does not produce the same bytes as
one that had none (D5). Replaying the file would have handed back an image two
links old, and the next link would produce the current one again — an output
alternating between two valid binaries.

```
  no-op rebuild, daemon       6.19 ms   0.61 ms
```

### The rule

State that is "per session" and state that is "per link target" look the same
when the session only ever serves one target, which is what every test does. The
daemon's tests link one program; the corpus links one program; the benchmark
links one program. It took a stray measurement of a *different* program through
an already-warm daemon to see it.

## 117. Dead-strip's traversal was hashing integers that were already indices

Finding 114 named `liveness` (2.07 ms) as the largest remaining item and the one
that does not decompose per object. Splitting it says most of that is true:

```
    group             0.31 ms     relocations per object, by section, sorted
    traverse          2.22 ms     reachability from the roots
```

Grouping is a pure function of one object and could be held in a session — and
it is worth 0.31 ms, so it is not the thing to build. The traversal is the cost,
and it is genuinely global.

But it was not global work that made it slow. Atoms are numbered `0..n`, and the
live set was a `HashSet<usize>` — hashing an integer that is already a perfect
index, and scattering one bit of information across a hash table, once per edge.
Every atom is asked about several times, once per reference that points at it.

A bit per atom:

```
                before   after
  traverse      2.22 ms  1.54 ms
  dead_strip    3.75 ms  3.01 ms
```

**0.68 ms**, for a container swap behind an unchanged three-method interface.

Worth putting next to finding 115, which measured the opposite result an hour
earlier: removing ~90,000 short-string allocations from the same stage bought
0.05 ms. Same intuition — "this data structure is doing needless work" — and the
two differ by more than a factor of ten. The allocations were on a path walked
once per *name*; the hashing was on a path walked once per *edge*. Neither the
allocation count nor the container type predicted which mattered. The access
pattern did, and only measurement showed it.

`traverse` at 1.54 ms is now the largest single item in the link, and it is
still recomputing global reachability to discover that 24 of 6879 addresses
moved. That remains the hard problem.

## 118. A hash map keyed by a tuple cannot be looked up without allocating

`AddressMap` held local symbols as `HashMap<(u32, String), u64>`. Rust can
borrow a `String` key as `&str`, but it cannot borrow a `(u32, String)` key as
`(u32, &str)` — so every lookup built the key it needed:

```rust
self.local.get(&(object.0, name.to_string()))
```

An allocation per question, thrown away immediately, on the path that answers
"where did this name go" — asked by every relocation applied, every GOT slot
filled, and every unwind record built. Nesting the map by object first removes
it: two lookups in two maps, no heap.

```
                before   after
  address_map   0.94 ms  0.74 ms
  apply         0.98 ms  0.75 ms
  unwind        1.52 ms  1.21 ms
  link         16.6  ms 15.7  ms
```

Nearly a millisecond, and the beneficiaries were three separate stages that
never appeared to have anything in common.

### Three measurements of the same intuition

This session removed allocations from three places, all of them "obviously"
wasteful:

```
  survey's seen-sets      ~90,000 clones     0.05 ms   (finding 115)
  dead-strip's live set   hashing indices    0.68 ms   (finding 117)
  address map's lookups   ~1 alloc/lookup    0.90 ms   (this)
```

A factor of eighteen between the first and the last. What separates them is not
how many allocations there were — the useless one removed the most — but
**where in the loop nest they sat**. `survey` allocated once per distinct name;
the other two sat on paths walked once per edge and once per lookup. Counting
allocations predicts nothing. Knowing which loop you are in predicts everything,
and the only reliable way to find out is to measure the stage before and after.

## 119. The relocation plan was re-proving inputs the session had just proved

Deciding which objects can skip relocation starts by asking whether their file
changed — `InputKey::probe`, which for a content-addressed rlib is a `stat` and
for one of rustc's objects is a read and a hash. `plan_reuse` probed every
distinct input file itself.

Every one of them had already been probed, minutes of CPU earlier, by the
session: that is how it decides whether to hand back a held parse at all, and it
keeps the key it proved. Asking the session instead:

```
  cache_plan   0.71 ms -> 0.55 ms
```

Small, and it is the shape that matters rather than the size: the session is
accumulating the answers to questions the rest of the link keeps asking
independently. `key_for` is the second of those to be shared, after the cache
itself. Every stage that still walks all the inputs to work out what changed is
recomputing something one component already knows.

## 120. Three quarters of a millisecond to find nothing

Splitting `prepare` — the bucket of work that fell between two named stages in
finding 108 — into its four unrelated jobs:

```
    placements        0.04 ms
    personality       0.10 ms
    unwind-size       0.12 ms
    commons           0.78 ms
```

`common_symbols` looks for tentative definitions: C's `int x;` at file scope,
where several translation units declare the same object and the linker allocates
one. **rustc does not emit them at all.** On this link the answer is the empty
set, every time.

Getting to the empty set cost 0.78 ms, because the function began by building a
`HashSet<&str>` of *every defined name in the program* — tens of thousands of
string hashes — so that the loop after it could ask whether each common symbol
was already defined. With no common symbols, that set answers no questions.

Testing the cheap condition first — one enum comparison per symbol, the same
walk without the hashing:

```
  commons     0.78 ms -> 0.02 ms
  prepare     1.04 ms -> 0.35 ms
```

The six tests covering tentative definitions still pass, which is what makes
this a short circuit rather than a removal: the expensive path is still there
and still correct for the C programs that need it.

### Why it survived this long

It never appeared in a profile. `common_symbols` was inside the unnamed gap
between `dead_strip` and `layout`, which finding 108 found only by insisting
that the stage timers sum to the total. Then it was inside `prepare`, a
one-line bucket, until it was split. Two rounds of "make the profile add up"
stood between this and being visible, and neither of them was looking for it.

A general-purpose function paying its full cost on inputs that need none of it
is invisible precisely because nothing about the call site suggests expense.


## 121. Resolving every relocation to read one of them, and the bug that nearly hid in the fix

`fill_unwind_info` was 1.25 ms. Splitting it:

```
    eh_frame_fde_offsets   1.00 ms
    compact_unwind_entries 0.23 ms
    unwind::build          0.02 ms
```

Encoding the table — the thing the function is named for — is two hundredths of
a millisecond. All the cost is in working out where each function's FDE landed.

That function built a `HashMap<offset, address>` of the section's relocations
and then looked up one entry per FDE. Two separate wastes. It called
`target_address` for *every* relocation in `__eh_frame` — the LSDA pointers, the
personality references, the CIE back-pointers — when the only one ever read back
is the FDE's `PC begin`. And it hashed and stored every result to answer lookups
that arrive in strictly increasing order, which a cursor answers without hashing
anything.

Walking the relocations in lockstep with the records:

```
  unwind      1.68 ms -> 0.75 ms
  synthetic   1.46 ms -> 0.85 ms
```

### The part worth writing down

The first version of the cursor was wrong, and it produced a binary that linked,
ran, and passed every test that checks a program's output.

A `PC begin` field is a `SUBTRACTOR` pair: **two relocations at the same
offset**, the anchor and then the function. The map this replaced inserted both
and kept whichever came last, which is the function. A cursor that stops at the
first match takes the anchor instead. The result is an unwind table pointing at
the wrong addresses — invisible until something actually unwinds.

`a_caught_panic_still_runs_destructors_after_stripping` caught it. That test
exists because catching a panic and running destructors is the one thing that
reads the unwind tables at runtime, and nothing else in a passing test suite
touches them. Without it this would have shipped: every binary correct except
when a Rust program panics.

The fix is two lines — a stable sort, and taking the last match rather than the
first — and neither is obvious from reading the code that was replaced. "Insert
into a map" quietly encodes a last-wins policy over duplicate keys, and
replacing it with anything ordered has to reproduce that policy on purpose.

## 122. Four hundred copies of a list the process already had

A reused object's `deps` — the sorted name hashes its relocations resolved
against — was a `Vec<NameHash>` inside the cache entry, and every link copied it
twice: once when the reuse path carries a reused object's record forward, and
once when the next cache is built from those records. On a link reusing 211
objects that is 422 copies of lists this process is already holding.

Sharing them behind an `Arc<[NameHash]>`:

```
  cache_build   1.14 ms -> 0.93 ms
  apply         0.96 ms -> 0.82 ms
```

The cache codec changes with it — decode collects into the `Arc`, encode
iterates it — which is the reason to note this at all. `deps` is *only* ever
read; nothing mutates an entry's dependency list after the link that produced
it. A `Vec` in a structure that is cloned and never modified is an owned copy of
something that has no owner.

## 123. A change kept without a number

`Atoms::owners` maps each externally visible name to the atoms defining it, and
stored a `Vec<usize>` per name — a heap allocation per symbol, around seven
thousand of them, almost all holding exactly one index. Weak symbols may have
several definitions and all are kept, which is why it was a `Vec` at all.

Storing the first inline with an empty `Vec` for the rest — and an empty `Vec`
does not allocate — removes every one of those allocations for the common case.

The output is byte-identical, verified against the same three hashes as every
other change in this run. The timing is not conclusive:

```
  before (load 6.1)   atoms 1.05   traverse 1.75   link 15.8
  after  (load 4.3)   atoms 0.97   traverse 1.63   link 15.2
```

The machine got quieter between the two, by more than the difference. **This is
recorded as "no measured effect"**, not as a 0.6 ms improvement, because the
only honest reading of those two rows is that the load changed.

It stays because it is strictly less work for identical output, which needs no
justification from a benchmark. But finding 115 measured exactly this intuition
at 0.05 ms and finding 118 measured it at 0.90 ms, and the difference between
them was never predictable in advance. Writing "kept, unmeasured" is cheaper
than a number that turns out to be the machine.

### On measuring at all today

Every timing in findings 117–122 was taken with load averages between 4 and 7 on
a machine running someone's browser. The stage-level before/afters are trustworthy
because both halves come from the same run minutes apart; whole-link medians drift
by more than most individual changes are worth. That is why the harness now takes
a median per stage rather than the last record, and why the interleaved
`scripts/ab.py` exists for anything closer than a millisecond.

## 124. A quarter of a million iterations to partition a list of a thousand

`object_ranges(image, object)` answers "where did this object's bytes land" by
scanning every contribution of every output section and keeping the ones that
match. Both the reuse plan and the cache builder called it **once per object**.

237 objects against 1,063 contributions is 252,000 iterations, twice a link, to
compute a partition of the very list being scanned. One pass grouping by object
produces all of it:

```
  cache_plan    0.38 ms -> 0.26 ms
  cache_build   0.93 ms -> 0.57 ms
```

`build_cache` was also re-probing every input file — the same redundancy finding
119 removed from `plan_reuse`, in the function immediately below it. Both now
ask the session, which proved them during loading.

### The shape

This is the third instance of one pattern in this session: **a helper that
answers a question about one item by scanning all of them, called once per
item.** `eh_frame_fde_offsets` built a map of every relocation to read one per
record (121); `common_symbols` hashed every defined name to check a list that
was empty (120); this partitions a thousand contributions two hundred times.

None of them is a bad function. Each is the obvious way to answer the question
it was written for, and each became quadratic when a caller started asking it in
a loop. That transition leaves no trace at either site — the helper still reads
correctly, and the loop still reads correctly.

What finds them is a profile with no unexplained residue, so that a stage which
is 0.9 ms for no visible reason has nowhere to hide.

## 125. The same quadratic again, in the function next door

`OutputSection::address_of` finds where an input section landed by scanning
every contribution of that output section. Two callers ask it inside a loop over
every object that has an `__eh_frame` section — against the output `__eh_frame`
section, which holds a contribution from every one of them. Both then subtract
`vm_address` from the answer immediately, so what they actually wanted was the
contribution's offset.

One pass builds every answer, and the call sites index it.

This is the fourth appearance of the pattern in findings 120, 121 and 124: a
helper that answers a question about one item by scanning all of them, called
once per item. It is now frequent enough to be worth stating as a rule rather
than a series of anecdotes:

> **A method that takes an identity and searches a collection is a lookup
> wearing the clothes of an accessor.** `image.address_of(object, section)`
> reads like a field access and costs a scan. The cost is invisible at the call
> site by construction, so it can only be found by profiling the caller or by
> reading the accessor — and nobody reads an accessor.

### Not measured

The machine reached a load average of 21 while this was being timed, and a
20-iteration run reported a 46.5 ms link with 12.6 ms unaccounted. There is no
honest number to report, so none is recorded: the change is in because it is
asymptotically less work for byte-identical output, verified against the same
three hashes as everything else in this run, with the suite green.

Finding 123 made the same call for the same reason. Two "kept, unmeasured"
entries in one session is a fair record of what it is like to benchmark
milliseconds on a machine somebody else is using.

## 126. What dyld charges for is fixups, not bytes

The release profile justified LTO partly on startup: rustc spawns a linker per
crate, so every link pays `execve` plus dyld's work over the whole image before
`main` runs. Measured against a 16 KB no-op process, blinker's spawn cost above
that floor:

```
  1.9 MB, no LTO          2.48 ms
  1.3 MB, fat LTO         1.13 ms
```

So the obvious next step was `panic = "abort"`. The one `catch_unwind` in the
workspace is in a test module, and cargo builds test targets with unwinding
whatever the profile says, so it was available. It removed 132 KB — another 10%
of the binary.

```
  1.15 MB, +panic=abort   1.14 ms
```

**Nothing.** Unwind tables are not read at load time; nothing touches one until
something unwinds. dyld's cost is in *fixups* — the cross-crate symbol
references and relocations it resolves before `main` — and LTO helped because it
deleted those, not because it deleted bytes. Two changes that both "made the
binary smaller", and only one of them was ever about size.

It was reverted, and not because it cost anything. It changes what happens on a
panic: aborting runs no destructor, and this linker writes to a temporary file
and renames it. Trading that away for a smaller file with no measured benefit is
a bad trade in the direction nobody notices until it matters.

### The pattern, again

This is the fourth "obviously free improvement" this session that measured at
zero: the QoS class (109), the survey's allocations (115), the atom owner
storage (123), and this. Each had a mechanism that sounded decisive. What they
share is that the mechanism was real and the *path it sat on* was not the one
being paid for.

## 127. The first thing the session remembers that is not an input

Everything the session held until now was something it had *read*: parsed
objects, archive indexes, the SDK's exports, the previous cache. `Atoms`' work
of splitting each section into independently-strippable pieces is the first
thing it holds that it *computed*.

The rule for what may go in: **a pure function of one object and nothing else**.
Atom boundaries qualify — where a section divides depends on that object's own
symbols and relocations, on `subsections_via_symbols`, and on the section's
flags. It does not depend on the layout, the strip, the imports, or on any other
input. So a boundary computed for a parse is valid for exactly as long as that
parse is.

Keying is by the identity of the `Arc<ParsedObject>` — its pointer — which is
exact in both directions: the same parse gives the same key, and a re-read input
gets a fresh allocation and therefore a miss. That is only sound because the
entry *holds the `Arc`*: without it the allocation could be freed and its
address handed to the next parse, and one object's derived facts would be served
for another's. A dangling-pointer bug that never dereferences a pointer.

Entries for parses a link did not use are dropped at the end of the strip, and
the `Arc`s with them. Otherwise a resident linker's memory grows with every
rebuild rather than with the program being linked.

### Not measured

Load average was between 8 and 21 while this was written, with `ld64` itself
spreading 164% and touching 108 ms. There is no number to report. The change is
in on correctness alone: the suite is green and the output is byte-identical to
the same three hashes every change today was checked against.

This is the third "kept, unmeasured" entry (123, 125). What makes it acceptable
is that it is a *mechanism*, not an optimisation: the value is that the next
four things — grouped relocations, opacity, personality fields, per-object
survey contributions — now have somewhere to go.

## 128. The benchmark was a release build, and release is the easy case

Every performance number in findings 92–127 was taken on one workload: blinker
linking itself, **release** profile. On that workload blinker reached parity
with ld64 (1.04x) and an edit relink of 21 ms.

A developer's edit–test loop does not use the release profile. Measuring the
debug link of the same program — after fixing the harness to pick a link by
*name* rather than by input count, which had silently captured a different
binary:

```
                inputs  objects   output    ld64     blinker
  release           69      900     2 MB   32.5 ms   34.0 ms   1.04x
  debug             80    1,643   8-9 MB   43.7 ms  114.9 ms   2.63x  <-- slower
```

**blinker is 2.6x slower than ld64 on the build people actually run.** Not
slower than its own release number by a little: 114.9 ms against 34.0, on inputs
that grew from 63 MB to 102 MB and from 900 objects to 1,643.

The link is correct — the binary runs and its signature validates — so this is
about cost, not breakage. (The output is 11% *smaller* than ld64's, which is
unexplained and worth its own investigation before it is called an advantage.)

### What this invalidates

Not the individual measurements; each was real. What it invalidates is the
*conclusion drawn from them*. "blinker is at parity cold and 1.6x faster on an
edit" describes the release profile only. The stage profile that every
optimisation this session was aimed at — traverse 1.66, emit 1.53, layout
1.10 — is the release profile's shape. The debug link is three times the size
and nothing says the same stages dominate it.

And the strategic claim built on top, that linking is only 1.8% of a rebuild and
therefore not worth optimising, was doubly wrong: it compared a release link to a
debug rebuild, and the debug link it should have used is five times larger than
the number it used.

### The rule, for the third time

Finding 77: a fixture is a claim about scale and it expires. Finding 106: a
harness that produces inputs differently from production measures a different
system. This is the same failure with a different variable — **a fixture is also
a claim about the *build profile*, and the profile a benchmark is convenient to
build is not the profile anybody waits on.**

Release was chosen because it is what `workload.py` defaulted to. No argument
was ever made for it. Nine sessions of optimisation were aimed at the case that
was already fine.

## 129. The incremental machinery holds at debug scale; the global stages do not

Finding 128 measured the cold debug link. This is the edit relink of the same
program — the number the product exists for, on the profile people build.

```
  ld64, cold debug                43.7 ms
  blinker, cold debug            114.9 ms
  blinker, edit relink, daemon    65.5 ms link   /  73.6 ms wall
```

**blinker's incremental link is still 1.5x slower than simply running ld64
cold.** On debug, today, there is no reason to use it.

### Every held-state mechanism works, at four times the scale

The session machinery is not what is failing. At 3,565 contributions, 277,657
relocations and 27,803 addresses — roughly four times the release workload —
every rule fires and the invariant is exact:

```
  session          78 of 80 inputs held; extraction replayed, resolution held
  placement        3562/3565 contributions kept their address, 0 unchanged movers
  relocations      266,152 of 277,657 reused (96%)
  addresses moved  3 of 27,803  (0.01%)
  read_and_parse   29.2 ms -> 3.3 ms
  resolve          11.5 ms -> 0.0 ms
  apply            13.5 ms -> 2.4 ms
```

**Three addresses out of twenty-eight thousand.** The incremental model is right
and it scales.

### What is left is exactly what was already on the list

```
  relocate    18.0 ms   of which apply is 2.4 — the rest is address map,
                        address table, synthetic tables, cache bookkeeping
  dead_strip  15.0 ms   liveness 10.8, atoms 3.6
  emit        10.6 ms
```

41 ms of the 65 is three stages that rebuild a global answer, in a link where
0.01% of the program moved. Those are the three items already identified as
needing architectural work: incremental liveness, clone-and-patch output, and an
address map that is patched rather than rebuilt.

The difference from the release profile is scale, not shape — but at this scale
the shape finally costs something worth fixing. A release link spends 1.7 ms in
`traverse`; a debug link spends 10.8. The optimisation targets do not change,
their value changes by a factor of six.

### The product position, stated honestly

blinker is at parity with ld64 on release links, faster than its own cold link
on an edit, and **not yet worth using on debug builds, which is where linking is
a real complaint about Rust.** The architecture is validated — the held state,
the retained placement, the relocation reuse all work at scale and produce a
correct, running, signed binary. What is missing is that three stages still do
global work, and the debug profile is where that bill comes due.

## 130. blinker scales with symbols; ld64 apparently does not

Between the release and debug workloads of the same program the work grows:
objects 1.8x, input bytes 1.6x, **output 4.1x**. The two linkers respond very
differently:

```
  ld64      32.5 -> 43.7 ms    1.34x
  blinker   34.0 -> 114.9 ms   3.38x
```

Per stage, sorted by how badly each scales (total grew 3.73x):

```
  emit_linkedit    0.56 ->  4.01    7.17x    symbol and string table encoding
  symbols          0.64 ->  4.08    6.37x    output symbols + debug map
  traverse         1.41 ->  7.56    5.36x
  unwind           0.68 ->  3.64    5.35x
  liveness         1.78 ->  9.00    5.06x
  synthetic        0.77 ->  3.90    5.05x
  atoms            1.00 ->  4.66    4.67x
  apply            2.88 -> 11.55    4.01x
  ...
  read_and_parse   9.23 -> 28.02    3.04x
  layout           0.90 ->  2.12    2.34x
  stub_parse       5.41 ->  6.48    1.20x    fixed: the SDK does not grow
```

Nothing here is quadratic — the quadratics were found and removed (120, 121,
124, 125). What this shows is different: **almost every stage grows faster than
the object count, and the two worst are both the symbol table.** A debug build
does not add many objects; it adds an enormous number of *symbols* to the
objects it already had, and blinker's cost is per symbol far more than per byte.

### Why this outranks the incremental work

An incremental linker's value is bounded by its cold link. blinker's incremental
debug relink is 65.5 ms against ld64's 43.7 ms *cold* — the held state is
working perfectly (96% of relocations reused, 3 of 27,803 addresses moved) and it
still loses, because it starts 2.6x too high.

Worse, the trend runs the wrong way. blinker's disadvantage grows with scale, so
at the size where linking is a real complaint about Rust — binaries where ld64
takes seconds, not 44 ms — extrapolating this slope makes blinker far worse than
2.6x, not better. **It becomes less competitive exactly as it becomes more
needed.**

### The correction to the whole project

Every performance decision recorded before this was made on a workload where the
baseline is 44 ms. Nobody has ever complained about a 44 ms link. The Rust
linking complaint is about projects an order of magnitude larger, and no
measurement in this repository has ever touched one.

So the ordering changes. Not "make the incremental path faster" — that
architecture is validated and works at 4x scale. It is:

1. find why per-symbol work dominates, starting with `emit_linkedit` and
   `symbols`, the two worst scalers and both symbol-table code;
2. get a workload where ld64 takes seconds, and check the slope holds;
3. return to incremental, which is already good and will inherit every gain.

## 131. The hasher note said "names too", and one crate never got the message

`blinker_hashing`'s own module docs record a correction: an early version kept
`std`'s SipHash for symbol names, on the reasoning that names are long and
hashed rarely, and a profile afterwards showed 1136 samples still in
`SipHasher::write`. Both halves of the reasoning were wrong.

`blinker-output`'s symbol-table encoder was never converted. It interns every
symbol name through a `std::collections::HashMap` — once per symbol, on
`emit_linkedit`, which finding 130 measured as **the worst-scaling stage in the
whole link** (7.2x when the work grew 3.7x). It was missed because the hasher
lived inside `blinker-link`, and `output` cannot depend on `link` — the
dependency runs the other way. The fix that was applied everywhere it was
reachable simply stopped at a crate boundary.

The hasher now lives in its own crate below both, and `link::hashing` is a
re-export so every existing `crate::hashing::FastMap` path still means what it
did.

### Not measured

Machine spread was 40-53% (`ld64` itself ranged 42.7 to 66.6 ms on a 45 ms
link). `emit_linkedit` read 4.01 ms before and 5.03 ms after, which is noise in
both directions. **No effect is claimed.** The change is in because the
reasoning that justified the hasher everywhere else applies here unchanged, and
because the crate boundary that hid it is worth removing whatever the timing
says.

Kept, unmeasured — the fourth this session (123, 125, 127). Three of those four
are structural changes rather than optimisations, which is the honest pattern:
a mechanism can be right for reasons a stopwatch cannot see.

## 132. Where to pick this up

The state at the end of this session, so the next one does not have to
re-derive it.

**What is settled.** The incremental architecture works and scales: at 3,565
contributions and 277,657 relocations, 96% of relocations are reused, 3 of
27,803 addresses move, and the placement invariant is exact with zero unchanged
movers. Held inputs, replayed extraction, held resolution, the session-owned
cache — all of it fires on the debug workload as designed.

**What is broken.** The cold linker scales with *symbols*, and ld64 does not:
3.4x against 1.3x for the same growth in work. blinker is 2.6x slower than ld64
on a debug link, and its incremental relink (65.5 ms) still loses to ld64's cold
link (45 ms). The disadvantage grows with scale, which is the wrong direction.

**The first concrete lead**, found and not yet acted on: `debug_map` emits five
to six stab entries per function, each carrying an owned `String` name, and
`symbols` + `emit_linkedit` are the two worst-scaling stages in the link
(6.4x and 7.2x). A debug build multiplies function count, and the debug map
multiplies that again. Making `OutputSymbol` borrow its name rather than own it
is the obvious change and was not attempted.

**The unexamined stages at debug scale**, all growing 5x or worse and none yet
looked at with the debug profile in hand: `traverse` 7.6 ms, `liveness` 9.0,
`atoms` 4.7, `unwind` 3.6, `synthetic` 3.9.

**The measurement gap that outranks all of it.** Every number in this repository
comes from one program whose link ld64 completes in 45 ms. Nobody complains
about a 45 ms link. The Rust linking complaint is about binaries where ld64
takes seconds, and no measurement here has ever touched one. Until a workload of
that size exists, every conclusion about whether blinker is worth using —
including the ones in this file — is an extrapolation.

## 133. One object of 1,059

`DESIGN-incremental-liveness.md` proposes rebuilding reachability only for the
part of the graph an edit touches. Before building it, the premise is worth
testing: **how much of the graph does an ordinary edit actually touch?**

A *reachability digest* per object — its atom boundaries, and the name each of
its relocations resolves through, and its defined symbols. Deliberately not a
hash of its bytes: an ordinary edit changes the bytes of every function it
touches while leaving the call graph exactly where it was, and that is the case
this exists to catch.

On the debug workload, a body edit:

```
  reachability: 1 of 1,059 objects' projection moved
```

**One.** Two rlibs were re-read and re-parsed; of the 1,059 objects in the link,
exactly one has a different graph. Everything `dead_strip` does — 19.3 ms of a
73 ms relink, the largest stage — is recomputed to accommodate one object.

Computing all 1,059 digests cost 5.80 ms, which would have eaten a third of the
prize. But a digest reads only its own object, so it belongs in the per-parse
memo from finding 127:

```
  digest   5.80 ms -> 0.49 ms
```

### What this settles about the design

The all-or-nothing version — "if no digest moved, reuse the whole strip" — will
not fire on an edit, because the answer is one and not zero. So the incremental
update has to be built properly, as the design says.

What changes is the confidence and the shape. The dirty set is not "a couple of
crates" or "the blast radius", it is **one object**, and the design's step 4 —
bounded re-derivation over the affected region — is re-deriving reachability
around a single object's atoms rather than around anything resembling the
program. The cost of getting the region wrong is now the difference between
one object and 1,059, which is also the argument for the verification mode: at
this ratio, a bug that quietly widens the region would still look fast.

The digest is also exactly the invalidation key the update needs. It does not
have to be invented separately — it is measured, memoised, and costs half a
millisecond.

## 134. Atom identity was a fact about the link, not about the atom

The design named per-object atom identity as the *enabling change* for
incremental liveness — a prerequisite, expected to be a pure refactor that
bought nothing on its own. It halved `dead_strip`.

An atom used to be a position in a flat `Vec` built fresh every link. That
number is not a property of the atom: give the first object one more atom and
every atom in the link is renumbered. Nothing derived from the numbering could
survive a link, so nothing was.

Now an atom is `(object, index within that object)`, and the flat numbering is
that pair plus the object's base. `ObjectAtoms` holds everything the traversal
reads about one object — its atoms, the edges leaving each of them, which are
roots, and how its unwind metadata points back at the code it describes — and
is a pure function of the parse, memoised beside its boundaries.

Same machine, back to back, on the debug workload:

```
                     before      after
  dead_strip        23.80 ms   12.89 ms
    atoms            5.08       4.27
    liveness        17.01       6.47
      group          1.81       0.18
      traverse      14.83       6.29
  link              97.0       90.2
```

Output byte-identical, verified by linking the same workload with both binaries
to the same path.

### Why a refactor was worth 11 ms

Because "pure function of one object" is not just a statement about caching. It
is a statement about what the *inner loop* is allowed to do. The old traversal,
per live atom, looked the atom's section up by id, matched its name against a
list of metadata names, found the object's relocation group in a map, binary
searched that group twice for the atom's offset range, and then for each
relocation resolved a symbol id, tested whether it was a definition, recovered
its section, and binary searched the section's atoms. All of that is the same
answer every time for the same object, and all of it was inside the loop
because there was nowhere to put it that outlived a link.

Once there is somewhere, it moves — and the loop becomes a slice index and a
name lookup. `group` fell from 1.81 ms to 0.18 not because grouping got faster
but because grouping is now part of the projection, done once for the one
object that changed. The 0.18 ms is the memo.

The same is true of the `relocations_for(section)` scan that collected edges out
of unsplit sections: a linear filter over *all* of an object's relocations, run
once per section of that object.

### And the same argument now applies to what is left

`atoms` is still 4.27 ms with 1,058 of 1,059 blocks served from the memo, so
almost none of it is projection. It is rebuilding the `owners` map — every
non-local definition in the link, ~77,000 of them, hashed by name into a table
that is thrown away at the end of the function — plus resolving opacity through
it. That is the next thing that is a fact about the link rather than about any
object.

### A caution recorded on the way

Two runs of the *same* binary produced different bytes, which looked like
non-determinism until the diff turned out to be a single byte: `'0'` vs `'1'`,
from the output path, which the debug map records. The comparison has to write
to the same path. A one-byte diff is the cheapest possible answer and it was
still nearly mistaken for a real one.

## 135. Three milliseconds of rehashing, and how to see it

Finding 134 left `atoms` at 4.27 ms with 1,058 of 1,059 blocks served from the
memo, which meant almost none of it was projection. Rather than reason about
which of the three parts it was, print them:

```
  atom parts: blocks 11.81  owners 3.76  opaque 0.01
```

Opacity — the part that looked expensive, because it resolves names across the
whole link — costs a hundredth of a millisecond. `owners` is the whole of it:
one map from every non-local definition in the link to the atom defining it,
77,000 entries, built from empty every time.

```rust
let mut owners = HashMap::with_capacity_and_hasher(
    blocks.iter().map(|b| b.owned.len()).sum(),
    Default::default(),
);
```

`owners` 3.76 ms -> 0.86. `atoms` 4.27 -> 2.28. `dead_strip` 12.89 -> 10.58.

The map was not slow because of hashing. It was slow because it grew into
77,000 entries from a default-sized table, which is seventeen doublings, each
one rehashing everything inserted so far — so most of the work was done on
entries that were already in the map. The block above already knows the answer;
it is one `sum()` over data that is in hand.

### The general form

This is the third time the same shape has paid: `Atoms::owners` here, the
symbol table's interner (131), and `Owners::rest` (which avoided 7,000 heap
allocations for one `usize` each). A container whose final size is a known
function of the input, built by repeated insertion from empty, is a linker
paying compound interest on its own growth.

Worth saying because it is invisible in a profile that attributes time to the
insert: every sample lands on `insert`, which is where the work *is*, and none
of them says the work was avoidable.

## 136. Two hundred thousand strings that already existed

`address_map` builds a map from every definition in the link to its output
address, and did it by cloning the name:

```rust
map.global.insert(symbol.name.clone(), address);
```

The names live in the parsed objects, which outlive the map by a wide margin —
the map is built and discarded inside one link. Borrowing them instead, plus
sizing the global map up front (135) and hoisting the per-object sub-map lookup
out of the per-symbol loop:

```
  address_map   2.93 ms -> 1.89 ms
```

Output byte-identical. This is the same argument that `Atoms::owners` was
changed by in finding 134, applied to the other half-dozen-line function that
was doing it — which is worth recording as a pattern rather than as two
separate fixes: **a lookup table built for the duration of one link should
borrow its keys, because everything it could key by is already in memory and
outlives it.**

## 137. The machine was the measurement

Halfway through the above, the numbers stopped making sense: `link` went from
86 ms to 176 ms across an A/B whose two halves differed by one struct field's
type, with a `max` of 723 ms. Load average was 11.8 — a browser at 99% of a
core, a `rustc` build, and a system extension, none of them mine.

Two things came out of it.

**Report minima, not medians, for stages.** Noise only ever adds time, so the
minimum across iterations is the closest available estimate of "the machine got
out of the way", and it is stable under interference where a median drifts with
whatever else is running. `relink.py --stage-stat min` does that now. The cost
is honest and worth stating: stage minima come from different iterations, so
they do not sum to the link minimum.

**Every number taken today under load was wrong, in the same direction.** The
same fixture, once the machine was quiet:

```
                 loaded median     quiet minimum
  link              86.6 ms          49.2 ms
  relocate          23.9              14.8
  emit              14.2               9.2
  dead_strip        10.6               5.1
```

Not a scaling factor — `dead_strip` halved while `emit` fell by a third — so
there is no rescuing the loaded numbers by dividing. They are discarded.

The one thing the loaded run got right was the *direction* of the A/B, because
both halves were equally polluted. That is the only claim a busy machine
supports, and it is the only claim that was made from it.

### And the picture that replaces it

Warm debug edit relink, quiet machine: **49.2 ms link, 58.6 ms wall.** ld64
links this program cold in 45.5 ms. The gap that the last session recorded as
2.6x is not 2.6x; it is close to parity, and most of what was believed about
where the remaining time goes was measured through the same fog.

## 138. blinker cannot link a real program

Asked whether the benchmarks cover long debug links, the answer was no — every
number in this file comes from blinker linking itself, a link ld64 finishes in
43.7 ms. So: point it at the largest Rust program on this machine, `pulsevm`,
552 inputs and a 189 MB debug binary.

The build succeeded. The link was refused:

```
blinker: undefined symbols:
_CFErrorCopyDescription
_CFErrorGetCode
_CFErrorGetDomain
```

Every pulsevm binary needs this:

```
-lc++ -lffi -liconv -lxml2 -lz -lz3 -lzstd
-framework CoreFoundation -framework SystemConfiguration -framework Security
```

blinker handles none of it. `discover_stub_library` finds exactly one file —
`<sdk>/usr/lib/libSystem.tbd` — and there is no `-framework` handling and no
general `-l<name>` search anywhere in the option parser. The honest capability
statement is:

> **blinker links pure-Rust programs whose only dynamic dependency is
> libSystem.**

That is a much narrower claim than "an incremental Mach-O linker", and it was
invisible for as long as the only program it ever linked was itself — because
blinker is exactly such a program. A benchmark that is also the only test case
cannot report the difference between "works" and "works on itself".

### What it would take

Not a small feature. Several things that the single-library assumption is
currently baked into:

- **Library search.** `-L` paths, `-l<name>` -> `lib<name>.tbd` / `.dylib` /
  `.a`, `-framework X` -> `X.framework/X.tbd` under the SDK, in the documented
  order.
- **Real dylibs, not just `.tbd`.** Homebrew's `libz3` is a Mach-O dylib with
  an export trie, not a text stub.
- **Per-library ordinals.** `library_ordinal` is currently the constant 1 with
  one `LC_LOAD_DYLIB`. The two-level namespace records *which* library each
  import came from, so this becomes one command and one ordinal per library.
- **Static archives from `-l`.** `-lc++` may resolve to an archive, which is
  the extraction path rather than the import path.

Recorded as the largest single gap between what this repository does and what
it claims. It is not a performance item, which is the reason it never came up:
every session so far has been about milliseconds on a fixture that dodges it.

## 139. The workload harness captured a 300-line build tool and called it
## rust-analyzer

With pulsevm out of reach, `rust-analyzer` is the substitute: 376 crates and no
native dependency at all. The capture reported:

```
  note: captured xtask, not rust-analyzer
  radbg: 319 files, 1067 objects, 68.7 MB
```

`xtask` is rust-analyzer's build helper. `largest_record` is supposed to
prevent exactly this — its docstring is three paragraphs about how picking "the
biggest link" silently substitutes a different program, and it takes a
preferred binary name for that reason. The preferred name still did not match,
because cargo calls the target `rust-analyzer` and rustc writes
`rust_analyzer-<hash>`:

```python
if wanted and stem == wanted[0]:      # "rust_analyzer" == "rust-analyzer"
```

One hyphen. The guard was present, correct in shape, and compared two strings
that name the same target in two different spellings — so it never fired, and
the fallback rule the guard exists to override picked the build tool.

It announced itself, which is the only reason it was caught: `note: captured
xtask, not rust-analyzer` is printed on exactly this path. A guard worth having
is worth having say so out loud when it declines.

Fixed by comparing canonical names with `-` and `_` folded together.

## 140. blinker links a real program

Finding 138 recorded that blinker could only link programs whose one dynamic
dependency is libSystem. It now resolves `-l<name>` and `-framework <name>`
through `-L`/`-F` and the SDK, and binds each import against the library that
actually exports it.

```
$ blinker --blinker-internal <rust-analyzer's 341 inputs> -framework CoreFoundation ...
$ ./rust-analyzer --version
rust-analyzer 0.0.0 (804ee7d 2026-08-01)

$ otool -L rust-analyzer
  /System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation
  /System/Library/Frameworks/CoreServices.framework/Versions/A/CoreServices
  /usr/lib/libiconv.2.dylib
  /usr/lib/libSystem.B.dylib
```

A working 187 MB binary, and the first program blinker has ever linked that it
did not compile itself.

### The part that would have failed silently

Mach-O's two-level namespace records, per imported symbol, the *ordinal of the
library it came from*, and dyld looks there and nowhere else. Resolving
`_CFRelease` against CoreFoundation while writing libSystem's ordinal produces
a binary that links, passes every structural check, and fails at launch — so
`StubExports` had to stop being a `BTreeSet<String>` and become a map from name
to owning library, carried from resolution all the way to the bind opcodes.
Where the ordinal is unknown it falls back to the first library rather than to
zero, because zero means flat-namespace lookup: the program would then search
every library, usually find the symbol, and work — hiding the bug.

One subtlety that is not obvious from the format: a symbol is attributed to the
`.tbd` file's *own* install name, not to whichever re-exported sub-document
defines it. `libSystem.tbd` is 40 documents; `_malloc` is written in
`libsystem_malloc.dylib` and must bind against libSystem, because libSystem is
what the image loads.

### Still missing

`.dylib` (needs the export trie) and `.a` from `-l` (the archive path). A
request resolving to either is dropped rather than guessed at, so the symbols
arrive as a named undefined-symbol error. That is why `pulsevm` — which wants
Homebrew's `libz3` and `libxml2` — still does not link. The `current_version`
written into `LC_LOAD_DYLIB` is libSystem's for every library; it is cosmetic,
and should be read from the `.tbd`.

## 141. Ten times slower, on the link that matters

With a real program linkable, the question finding 137 could not answer:

```
  rust-analyzer, debug, 341 inputs, cold
    ld64       319 ms      220 MB
    blinker  3,104 ms      187 MB      9.7x
```

Against blinker's own debug self-link — 80 inputs, the fixture every number in
this file was taken on — it is 1.1x. Seven times the inputs, and the ratio
moves by a factor of nine.

Finding 130 measured this scaling and called it "blinker grows 3.4x where ld64
grows 1.3x" from two points that were 40 ms apart. It was right about the
direction and far too optimistic about the magnitude, because both of its
points were small.

Everything measured before today was measured on the wrong workload. The
milliseconds were real and the changes were real, but "which stage matters" was
answered by a link that finishes in the time this one spends on any single
stage. Before any further optimisation, the profile has to be retaken here.

## 142. Incremental buys nothing on the link it was built for

The whole premise: a resident linker that holds its inputs should relink an
edit far faster than a cold linker can start. Measured on the long link, with
an edit to `crates/base-db/src/lib.rs` — near the bottom of rust-analyzer's
graph, so the blast radius is real:

```
  blinker, cold                3,276 ms
  blinker, warm incremental    3,312 ms
  ld64, cold                     336 ms
```

**The warm relink is not faster than the cold one.** It is 9.9x ld64.

### The layout half works perfectly

```
  placement: 9719/9722 contributions kept their address (100%), 3 moved
  of those, 0 belonged to inputs that did not change
  addresses: 197/506405 changed (0.04%)
  reachability: 1/5637 objects' projection moved
  session: 326 inputs held, 15 read
```

Every invariant this linker was designed around holds, at ten times the scale
they were established at. 15 of 341 inputs changed. One object of 5,637 has a
different call graph. Four hundredths of one percent of addresses moved.

And it takes 3.3 seconds, because knowing that almost nothing changed is not
the same as doing almost nothing.

### Where the 3.3 seconds goes

```
    read_and_parse   991 ms   30.2%     326 of 341 inputs were held
    relocate         929      28.3%
      apply          618      18.8%     26% of relocations reused
    resolve          439      13.4%     redone: 2 interfaces moved
    dead_strip       354      10.8%     one object's graph moved
      traverse       130
      atoms          100
      digest          97
    emit             243       7.4%
      emit_linkedit  166
    symbols          136       4.1%
    survey            71       2.2%
```

Three numbers do not fit and each is a separate defect:

- **`read_and_parse` is 991 ms with 326 of 341 inputs held.** The archives are
  held and their members are re-extracted anyway — "extraction recomputed" in
  the session line. Residency is holding the wrong granularity.
- **Relocation reuse is 26%, where the self-link gets 96%.** 15 inputs of 341
  changed, so ~96% is what the mechanism should deliver. Something is
  invalidating four objects for every one that actually moved.
- **`dead_strip` is 354 ms to accommodate one object of 5,637.** This is
  finding 133 again, at a scale where it costs a third of a second rather than
  five milliseconds.

### What this settles

The scaling law (130, 141) is not a story about constants. Every stage is
proportional to the *program* while every measured change is proportional to
the *edit*, and on an 80-input fixture that gap is invisible because the whole
link fits in 50 ms. At 341 inputs it is the entire result.

The design was right and the implementation of it stops at layout. Placement,
addresses and reachability are all incremental and all demonstrably correct at
this scale; reading, resolving, relocating, stripping, emitting and symbol
table construction are not incremental at all.

That is the work. It is now measurable, on a fixture that exists, against a
program blinker can link.

## 143. Counting inputs hid that they are not the same size

Finding 142 named three defects from one relink. Two of them were mine, not the
linker's, and the instrument that found that out took ten minutes to build:

```
  members: 2131 held, 0 renumbered, 3373 missing
```

The suspicion was that `ObjectId` — positional, assigned in extraction order —
renumbers every member after the first change, invalidating the member cache
the way a global atom index invalidated everything in finding 134. It is the
same shape of bug and it would have been a satisfying second instance.

**Zero renumbered.** The ids were never the problem.

What was missing was simply not there, and the reason is arithmetic:

```
  members in changed archives: 3388 of 5946  (57%)
     256  libhir_expand    256  libhir_def    256  libhir_ty
     256  libhir           256  libide_db     256  libide_assists
```

**15 of 341 inputs is 57% of the program.** The edit was to
`crates/base-db/src/lib.rs`, near the bottom of rust-analyzer's graph; cargo
recompiled every crate downstream, and those are the large ones. So:

- `read_and_parse` re-read 57% of the object code because 57% of it was new.
  There was no cache to hit.
- Relocation reuse of 26% is not against a ceiling of 96%. Only 43% of objects
  survived at all, so the ceiling was 43% and the gap is a third, not a
  quarter of what it looked like.

The third defect stands, and stands out more sharply now: **`dead_strip` spends
354 ms accommodating one changed object out of 5,637**, on a link where 3,388
objects were genuinely rewritten. The reachability digest — which hashes the
call graph and not the bytes — says the graph did not move even though most of
the bytes did. That is precisely the case it was built for.

### Three blast radii, and only one of them was being quoted

```
  by input file      15 of 341     ( 4%)
  by object        3388 of 5946    (57%)
  by call graph       1 of 5637    (0.02%)
```

The harness prints the first, because inputs are what a command line has. It is
the least informative of the three and it is the one every "blast radius" line
in this file has meant. A `.rlib` is not a unit of anything: rust-analyzer's
are 256 objects each and its leaf crates are one.

Stages that must re-read bytes are bounded by the middle number and there is
nothing to win there. Stages that depend only on the graph are bounded by the
last one, and they are the whole of the remaining opportunity.

### And the fixture was a worst case

Editing the bottom of the dependency graph is a real thing developers do, but
it is not the inner loop. The inner loop edits a leaf, cargo recompiles one
crate, and the blast radius is genuinely small. That measurement is the one
that says whether incremental linking is worth anything, and it had not been
taken — the harness's default edit target is chosen to *maximise* blast radius
("a crate near the bottom of the graph, so the edit has a blast radius"), which
is the right default for stressing correctness and the wrong one for this.

## 144. rustc renames every object of a recompiled crate, and the session went
## cold on it

Trying to measure the developer inner loop — edit the binary crate, relink —
the harness refused: "the rebuild produced identical inputs — the edit changed
nothing". It was right, and for a reason worth the whole detour:

```
  rust_analyzer-f5cc78900386defc.001z4lyiuuj1qmjjq7sk5cs4o.02n9mi4.rcgu.o   before
  rust_analyzer-f5cc78900386defc.001z4lyiuuj1qmjjq7sk5cs4o.0l3l1uh.rcgu.o   after
```

The codegen unit's own hash is identical. The last component changed, and it
changed to the *same* new value for all 132 objects — it identifies the build,
not the content. Checking the bytes:

```
  matched by content-identity: 341 of 341
  rcgu objects: 132 byte-identical, 0 actually changed
```

**Every object of the recompiled crate was renamed. None of them changed.**
This is what a debug rebuild does, every time, because cargo turns on
incremental compilation and rustc names its objects per build session.

### What that cost

`Session::begin` discarded the entire session whenever the input list differed:

```rust
if self.inputs != inputs { self.entries.clear(); self.members.clear(); ... }
```

So the normal case — a developer edits a file and rebuilds — presented a
changed input list, and a resident linker went cold. Isolated, on the same
directory, with only the 132 names changed and not one byte:

```
  cold                            3755 ms   held=0   read=341
  132 renamed / same bytes        3645 ms   held=0   read=341
```

A rename of a third of the inputs cost a full cold link.

### Why it was written that way, and where the guard belongs

The reason is real: object ids are positional, and a held parse carries the id
it was parsed with. Discarding everything made that safe by making it
impossible.

It is the same mistake as finding 134 in a different place — *identity taken
from position* — and it has the same fix. The session now keeps the per-path
facts for paths that survived, drops what was derived from the list as a whole
(the extraction order, the import set), and `load_objects` serves a held parse
only under the id this link would assign it.

```
                                  before     after
  132 renamed / same bytes        3645 ms   2934 ms   held 0 -> 209
  renamed back                    3143      2549      held 0 -> 209
```

### The test that had encoded the bug as the contract

`a_changed_input_list_starts_over` asserted the wipe. Its *name* said "starts
over" and its assertion message said "a renumbered link served an object
carrying its old id" — the second is the property worth having and the first
was one way to get it. Rewritten to assert what actually matters, plus two link
level tests: one that prepends an input so every held id is off by one and
requires the result to equal a cold link, and one that appends an input and
requires the session to keep the others. The first fails without the id check —
verified by removing it.

### Still 2.5 seconds for nothing

Holding 209 of 341 inputs is not the answer, it is the removal of a cliff. The
132 renamed objects are byte-identical and are still read, parsed, and
relocated from scratch, because they are identified by path. blinker already
computes a content hash for exactly these inputs — `InputKey::probe` does it
because rustc's paths do not identify their contents — so the parse cache could
be keyed by content and a renamed-but-identical object would be a hit.

That is the next thing, and it is worth stating plainly what it means: **for
the inner loop this linker exists to serve, a third of the work it does is on
inputs it has already parsed, unchanged, under a different name.**

## 145. Serving a renamed object from its content, and the invalidation behind it

Finding 144 removed the cliff where a renamed input list threw the session
away. It left 2.5 seconds of work on 132 objects that were byte-identical to
ones already parsed, because they were identified by path.

`InputKey::probe` already hashes exactly these files — an `.rlib` path is
content-addressed and a `.rcgu.o` path is not, so rustc's objects were being
hashed anyway. So the parse cache gets a second index, keyed by that hash, and
a renamed object is a hit.

Two things had to come with it, and neither is incidental.

**The path had to leave the shared parse.** `ParsedObject.metadata.path` is the
path it was *first* parsed from, and reusing a parse across a rename makes that
stale. An `OSO` stab names a file a debugger will open; a contribution's
identity is hashed from its input's name. `LoadedObject` now carries the path
this link read it from, and the five places that name the file to a consumer
use that. Without it, a contribution's identity would depend on whether the
session happened to be holding it — a warm link and a cold link would disagree
about what the same bytes are called.

**The index had to be pruned by use, not by the input list.** Pruning by list
is exactly what it exists to survive. It follows `forget_unused_memos`: a
parse lives as long as something keeps linking it, and two links that do not
touch it drop it.

### And then the number barely moved

```
  132 renamed / same bytes    2902 ms   held=341 read=0   parse=980
```

Every input from memory, nothing read — and `read_and_parse` was still a
second. It is not I/O. `Session::begin` discarded the extraction order and the
import set whenever the input list changed, so the archive-member frontier ran
again over 5,600 objects and resolution redid the whole symbol table.

That invalidation was wrong in the same way the wipe was. The extraction order
is `(archive position, member)`, so what it depends on is the *archives* and
the interfaces — neither of which a rename of the loose objects touches. Stored
with the archive list it is indexed against, and guarded by the interface check
that was already there, it survives:

```
                       cold      rename    rename    rename
  link                 3861 ms   1513      1420      1360
    read_and_parse      1342       35        32        33
    resolve              602        0         0         0
    dead_strip           520      228       231       231
    relocate             391      391       345       396
    emit                 359      311       310       304
```

Against where the day started, on a rename of a third of the inputs with not
one byte changed:

```
  3645 ms  ->  1360 ms
```

Output verified by running it: `rust-analyzer 0.0.0 (804ee7d 2026-08-01)`.

### What is left, and it is now the whole of it

A rename with zero content change should cost what the no-op path costs — 47
ms. It costs 1,360, and the profile no longer has a dominant term: `dead_strip`
231, `relocate` 391, `emit` 311. None of them consults whether anything
changed. That is the same list finding 142 ended with, minus the two entries
that turned out to be about reading files.

## 146. A stable sort of 1.7 million symbols, sorted by three fields that
## already order it totally

With the rename handled, the zero-change relink is 1,360 ms with no dominant
term. Splitting the symbol stage:

```
  symbols: placed 15  output 182  debug_map 52   (1689759 entries)
```

`output_symbols` is nearly all of it, and it does two things: build one
`OutputSymbol` per definition, then sort them. The sort was `sort_by`, which is
stable — it allocates a second buffer of 340,000 entries and merges into it.

The comparator is `name`, then `value`, then `section`. Two entries that
compare equal agree on all three, and nothing else about them reaches the
output, so the order among them is not observable and stability is buying
nothing.

```
  output_symbols   182 ms -> 95 ms
```

One word. Verified byte-identical on the 187 MB rust-analyzer image — which is
the check this needed, because "the tie-break is total" is an argument and a
187 MB binary with 1.7 million symbols is evidence.

### The number underneath it

1,689,759 symbol table entries for a program with 506,405 addresses. Most of
them are the debug map: four stabs per function — `BNSYM`, `FUN`, `FUN`,
`ENSYM` — which is what `ld` emits and what a debugger reads. It is worth
writing down because every future measurement of this stage is really a
measurement of that ratio, and "the symbol table" sounds like it should be
proportional to the symbols.

## 147. Eighty-two megabytes of string table, grown from one byte

Inside `emit_linkedit`, on the rust-analyzer image:

```
  symtab: sort 8  intern 135  counts 3   (1689759 syms, 82153095 string bytes)
```

The interning loop is nearly all of it. Two of finding 135's shape, and a third
of a different kind:

- **The string table** starts as `vec![0u8]` and reaches 82 MB. Twenty-seven
  doublings, copying about 160 MB on the way. The bound is the total length of
  every name — one pass over data already in hand.
- **The intern map** is probed once per symbol and ends up holding one entry
  per distinct name, growing into 1.7 million inserts from empty.
- **The group counts** were three separate `filter().count()` scans of all 1.7
  million entries to produce three numbers that one pass produces.

Minima over eight links each:

```
              before   after
  intern       135 ms   98 ms
  counts         3       1
  sort           8       9
```

Output byte-identical on the 187 MB image.

### What it does not fix, and that is the interesting part

Sizing the containers is worth 27%, not 70%. The rest is inherent to the shape:
1.7 million hash lookups of names averaging 48 bytes, over a table too large to
sit in any cache. That work does not go away by allocating better — it goes
away by not doing it, which means the encoded symbol table has to survive a
link the way parses now do.

That is the same conclusion the other remaining stages reach. On a relink where
nothing changed at all, `dead_strip` is 231 ms, `emit_linkedit` 145, `symbols`
95, `address_table` 136, `unwind` 112. Every one of them is a complete
recomputation, every one is correct, and none of them asks what moved.

## 148. Where this leaves the long link

A relink of rust-analyzer where a third of the input files were renamed and not
one byte of any of them changed. Minima over eight links through a resident
linker:

```
                     start of the day     now
  link                    3645 ms         970 ms
```

The whole of that came from removing wrong invalidation rather than from making
anything faster: the session no longer throws itself away when the input list
changes (144), a renamed object is served from its content (145), and the
extraction order and import set are invalidated by interfaces instead of paths
(145). Two container-sizing fixes and a sort account for about 130 ms of it.

What is left, in order:

```
    relocate         294 ms   30%     address_table 130, unwind 97, address_map 36
    emit             201      21%     emit_linkedit 144
    dead_strip       168      17%     traverse 135, atoms 24
    symbols           97      10%
    survey            69       7%
    layout            34       4%
    read_and_parse    25       3%
```

`read_and_parse` was 991 ms and is now 25. Nothing else has moved, because
nothing else was invalidating wrongly — they were all simply recomputing, which
they still are. Every stage in that list is a complete pass over the program to
produce an answer identical to the one the previous link produced.

ld64 links this program cold in 336 ms. blinker now takes 970 ms to do nothing.
The remaining work is not a search for waste; it is that six stages need to
learn what changed, and the measurements that say what each of them may skip —
one object of 5,637 for liveness (133), three contributions of 9,722 for
placement, 197 addresses of 506,405 — are already recorded.

## 149. The frontier asked every archive for every name

A realistic inner-loop edit — `crates/ide/src/lib.rs`, a library crate only the
binary depends on — with everything above in place:

```
  blast radius: 2 of 341 inputs, 512 of 6079 objects (8%)
  session: 339 inputs held, 2 read
  link 2523 ms
    read_and_parse   876 ms   35%
```

876 ms to read two files. Splitting the extraction loop:

```
  extract: probe 517  absorb 285  parse 0   (5637 objects)
```

`parse 0`. Every archive member came out of memory. The stage that is named
after reading and parsing did neither — it spent its time deciding *which*
members to pull.

`probe` is the loop that answers that:

```rust
for name in &wanted {
    for (archive_index, (_, index, _)) in archives.iter().enumerate() {
        let Some(member_id) = index.member_defining(name) else { continue };
        ...
        break;
    }
}
```

Every name, against every archive, and `member_defining` is a binary search
with string comparisons. 341 archives and tens of thousands of names is tens of
millions of them.

One merged table, built once — first definition wins, which is what the nested
loop did twice over (first entry within an archive, first archive that answers,
so inserting only when absent gives the same answer):

```
  probe            517 ms -> 13 ms
  read_and_parse   876    -> 401
  link            2523    -> 1993
```

Output byte-identical on the 187 MB image.

### What it says about the shape of this linker

This is not an incremental-linking problem and it never was. It is a linear
search wearing a loop, in the one stage a resident linker had already made
almost free — `parse 0` says the caching worked perfectly and the stage still
cost 876 ms.

Two things had to happen before it could be seen. The workload had to be large
enough that 341 archives is a real multiplier, and the session had to be good
enough that reading and parsing were no longer hiding it. On the self-link, 80
inputs and a cold parse, this loop is invisible.

`absorb` is now the largest part of extraction at 285 ms: `Frontier` holds
owned `String`s for every defined name in the program and grows both of its
sets from empty.

## 150. The frontier's `BTreeSet` was not the cost

With the archive probe fixed (149), `absorb` is the largest part of extraction
at 285 ms. The obvious suspect: `wanted` is a `BTreeSet<String>`, ordered so
that the extraction order — and every member's object id — cannot depend on a
hash seed, and touched once per undefined symbol in the program.

Replacing it with a `HashSet` and sorting the per-round list instead, which
gives the identical order:

```
  read_and_parse   401 ms -> 394 ms
```

Nothing, against a noise floor larger than the difference. Reverted: it adds a
sort and a paragraph explaining why the sort is safe, in exchange for a number
that did not move.

Recorded because the reasoning was good and wrong, which is the useful kind.
`absorb` walks every symbol of every object *twice* — definitions first, so a
symbol defined later in an object satisfies a reference made earlier in it —
and clones a `String` for every name it has not seen. At 5,637 objects that is
the cost, and the ordered set was never more than a small part of it.

The fix would be to stop cloning, and that is blocked on something structural:
the names live in the parsed objects, but the frontier is built while the
object list is still growing, so it cannot borrow from a `Vec` that is being
pushed to. Every other place this pattern was fixed today (134, 136) had a
finished collection to borrow from.

## 151. Where the realistic edit stands

The fixture that matters: `crates/ide/src/lib.rs` edited — a library crate only
the binary depends on, which is what an inner loop looks like. Minima over six
relinks through a resident linker.

```
  blast radius   2 of 341 inputs, 512 of 6079 objects (8%)
  session        339 inputs held, 2 read
  placement      23736/23739 contributions kept their address
  addresses      38 of 506404 changed (0.01%)
  reachability   1 of 5637 objects' projection moved

                          before today's last stretch      now
  link                            2868 ms                 2078 ms
    read_and_parse                 934                     409
    resolve                        495                     454
    relocate                       474                     463
    emit                           227                     212
    dead_strip                     217                     203
```

ld64 links this program cold in 336 ms.

Nearly all of the 790 ms came from one loop (149). The rest of the profile is
unchanged, and unchanged for the same reason it has been all day: every stage
still computes its answer from the whole program.

What each of them could skip, measured:

```
    relocate       463 ms    38 addresses of 506,404 moved; 83% of
                             relocations are already reused
    resolve        454       2 interfaces of 341 moved
    read_and_parse 409       parse measures 0; it is `Frontier::absorb`
                             walking every symbol twice and cloning names
    emit           212       the image is 187 MB and 3 contributions moved
    dead_strip     203       1 object of 5,637
```

Two stages have no incremental story written down yet — `resolve` and
`symbols`/`emit_linkedit` — and `DESIGN-incremental-liveness.md` covers the
third. None of them is blocked on a measurement; the numbers above are all the
invalidation keys they need, and every one of them already exists in the
session.

## 152. Resolution is interning, and interning stored every name twice

`resolve` is 424 ms on the realistic edit and had never been looked at inside.
It splits:

```
  resolve: imports 180  table 250
```

`resolve_symbols` builds the global table by offering every symbol of every
object to it, and each offer does an intern plus three or four map operations
keyed by the interned id. The obvious target was the maps — `resolved`,
`rules`, `candidates`, `undefined`, four `HashMap`s keyed by a dense `u32` that
could be `Vec`s.

Measuring first, by replacing the loop body with nothing but the intern:

```
  intern only:  table 250 ms
  full path:    table 250 ms
```

The maps are close to free. That is not quite the clean result it looks like —
the intern-only run interned *every* symbol, including locals, which the real
path skips before it ever interns — so the two are not the same amount of
interning. What it does rule out is a refactor of the four maps, which was
where an afternoon was about to go.

### What interning was doing

```
  intern: 981253 calls, 477532 distinct (51% hits)
```

Half the calls are misses, and each miss allocated the name **twice** — once
into `names: Vec<String>` and once as the key of `lookup: HashMap<String, _>`.
955,064 allocations to store 477,532 names.

Sharing one allocation between the two, as `Arc<str>`:

```
  table   250 ms -> 225 ms
  link   1990    -> 1863
```

Output byte-identical.

### And two `Vec`s per name that a successful link never reads

`candidates` records every definition offered for a name so a duplicate-symbol
error can list the competitors; `undefined` records which objects referenced a
name so a missing-symbol error can say who wanted it. Both were a `Vec` per
name — a heap allocation for every distinct name in the program, to hold one
element, on the way to an error that does not happen.

Both now keep the first inline and the rest in a `Vec` that stays empty, which
is the `Owners` shape from the atom code. `table` 293 -> 250 before the interner
change above.

## 153. The address table is a cryptographic hash, 506,405 times

`address_table` is 133 ms. The shape suggested the sort — half a million pairs.

```
  address_table: hash+collect 127  sort+dedup 8   (506405 entries)
```

It is the hashing, and the sort is nothing. `dep_hash` is BLAKE3.

Rewriting it as one `blake3::hash` over a stack buffer instead of three
`update` calls into an incremental hasher — byte-for-byte the same input, so
every hash it has ever produced is unchanged and no cache is invalidated:

```
  hash+collect   127 ms -> 126 ms
```

Nothing. The cost is BLAKE3's compression on a 50-byte input, not the builder
around it. Reverted.

### Why the obvious fix is refused

`blinker_hashing::FastHasher` would be twenty times faster and it is already in
this codebase. It is fxhash — multiply-xor-rotate, one round per word, no
finalisation — and mangled Rust symbols share long prefixes and differ in the
middle, which is the input shape fxhash is weakest on.

This hash decides whether an object may reuse a previous link's patched bytes.
A collision is two different names agreeing that an address did not move, which
is a wrong binary produced silently. 127 ms is not worth that, and the right
way to spend it is to stop calling the function half a million times rather
than to make each call cheaper.

### An inconsistency worth naming

`dep_hash` is cryptographic. `interface_digest` and `reachability_digest` —
which gate extraction replay and, soon, liveness reuse — are fxhash. All three
are 64-bit change-detection keys whose failure mode is a silently wrong binary.
Either one is over-built or two are under-built, and nothing in the repository
says which. Recorded rather than resolved: it is a decision about what this
linker is willing to be wrong about, not a measurement.

## 154. Liveness reuse, and a digest taken from the projection instead of about it

The first half of incremental liveness, and the half that has to exist before
the second can be trusted.

### The key had a hole in it

`reachability_digest` walked the object and hashed what it believed reachability
reads: section names, atom boundaries, relocation targets by name, defined
symbols. That is a *claim* about the projection, maintained by hand, and it was
already wrong — opacity is decided by whether a relocation stores a non-zero
inline addend, which is read out of the object's bytes and was not hashed. An
edit that changed an addend from zero to non-zero would have moved the answer
without moving the digest.

The digest is now taken from `ObjectAtoms` itself — the atoms, the edge lists,
the roots, the unwind edges, the opaque sets. "The digest did not move" and
"this object contributes what it contributed last time" are now the same
statement rather than two that have to be kept in agreement.

### The reuse

If every object's projection digest is unchanged, then the owners map, the
opaque set, the live set and the compaction are all unchanged, and the previous
`Strip` is the answer. Held in the session against the digest vector that
produced it.

```
  link 0 (cold)   dead_strip 520.5 ms   atoms 171  liveness 197  strip_build 3.5
  link 1          dead_strip  34.4      atoms  25  liveness   0  strip_build 0
  link 2          dead_strip  33.8      atoms  25  liveness   0  strip_build 0
```

### It does not fire on an edit, which was known

Finding 133 measured the dirty set at one object of 5,637 and said this
all-or-nothing version would not fire. It does not. What it is, is the skeleton
the bounded re-derivation slots into, with the invalidation key and the
verification already in place.

### The verification

`BLINKER_VERIFY_LIVENESS=1` makes every link compute the answer twice and
assert the held one equals a fresh one. It runs clean on the rust-analyzer
workload. This exists because the failure mode is silent — an atom stripped
that is still reachable produces a binary that links, runs, and crashes
somewhere unrelated — so the reuse is a claim that gets checked rather than
argued.

Two tests, on a fixture small enough to run the program: the reused answer must
equal a cold link byte for byte *and* the binary must still print the right
number; and an edit that adds a function and calls it must not be served the
old answer. The second failed first time on my arithmetic rather than on the
linker, which is the right way round.

## 155. Two digests of the same thing

With the projection carrying its own digest (154), the older
`reachability_digest` was a second hash of the same object computed before the
projection existed — and it was the one with the hole. Deleted, along with the
per-parse memo slot that held it, and `reach_moved` now comes from the
projections:

```
  digest       21 ms -> 0.05 ms
  dead_strip  203    -> 190
```

`reachability: 1/5637 objects' projection moved` is unchanged, which is the
check that mattered: the surviving digest is at least as precise as the one it
replaced.

The 21 ms was not the hashing — that was memoised per parse. It was collecting
5,637 memo lookups keyed by `Arc` pointer, to build a vector the projections
already had.

## 156. The traversal was resolving the same name three times over

Before building the bounded re-derivation, a measurement of what it would be
worth. `traverse` is 133 ms, and 44% of the edges it walks resolve by *name* —
a symbol dereference and a string hash into the owners map, per edge walked.
How many are distinct:

```
  edges: 1503616 local, 1195652 by name, 390606 distinct per object (3.1x reuse)
```

An object refers to the same name 3.1 times on average. `Edge::Name` now holds
an index into a per-object deduplicated list, and each *distinct* name is
resolved once per link rather than once per edge:

```
  traverse    133 ms -> 21 ms
  atoms        44    -> 116        (the resolution now happens here, once)
  dead_strip  190    -> 151
```

Output byte-identical on the 187 MB image; 62 suites green, and green again
with `BLINKER_VERIFY_LIVENESS=1` so every link computed liveness twice and
compared.

### What this does to the plan

The bounded re-derivation was to be worth 133 ms. **The traversal is now 21 ms**,
so it is worth 21. The cost did not disappear, it moved to `resolve_names`,
which is a global resolution against the owners map — a different problem, and
one the all-or-nothing reuse already skips entirely.

Two hours of careful fixed-point work, with the only silent failure mode in the
linker, for 21 ms. That is no longer the right next thing, and the way to find
that out was to make the cheap version first and re-measure rather than to
build the expensive one against a number taken before it.

### The verification mode caught its own flaw

`a_reused_dead_strip_answer_equals_a_cold_one` passed normally and failed under
`BLINKER_VERIFY_LIVENESS=1`. Not a liveness bug: `reused_strip` was set on the
early-return path, so turning verification on — which deliberately does the
work anyway — made the flag read false. The flag meant "the shortcut was
taken" when the useful question is "the held answer was valid".

A diagnostic that changes with the mode that checks it makes every test of it a
test of the mode. Now set where the decision is made, and both runs of the
suite agree.

## 157. A name interned once per link is a name interned once per name

Three separate passes over every symbol of every object were each hashing the
name text, on every link, for a link whose inputs had not changed:

- `Frontier::absorb`, in `read_and_parse` — two passes, and a `String` clone
  per name it had not already seen.
- `undefined_references`, in `resolve` — two more, into borrowed `&str` sets.
- `resolve_symbols`, also in `resolve` — `SymbolNames::intern` per symbol,
  981,253 calls of which about half miss.
- `Atoms::build`, in `dead_strip` — the owners map, keyed by `&str`, built from
  every non-local definition and probed once per distinct referenced name.

Four data structures, four hashers, one question: *which name is this?* A
resident linker answers it for the same 187 MB of inputs every time the
developer saves a file.

### The interner outlives the link

`Session` now holds the `SymbolNames` table across links, and each parse
carries a `Vec<SymbolNameId>` beside it — the object's names, in `SymbolId`
order, interned when it was first read. Everything above is keyed by that id.
A held object costs an `Arc` clone; only genuinely new bytes are hashed.

The table can never be renumbered, because the ids in those memoised vectors
are only meaningful against it. So it grows monotonically, bounded not by the
program but by how many *new* names later links introduce — the recompiled
crates' symbols per rebuild, which is what a rebuild changes anyway.

### The order the ids come out in is not an order

An id is the order this process happened to intern something, which differs
between a session's first link and its next. Two places had been relying on a
name ordering:

- `Frontier::wanted` was a `BTreeSet<String>`, and its order decides which
  archive member is pulled first — and therefore what `ObjectId` it gets, and
  therefore the output bytes. It is now a hash set of ids, sorted by name text
  at the one point it is read.
- `SymbolTable::undefined_symbols` sorted by id under a comment saying
  "diagnostics must not vary between runs". True when ids were per-link; false
  the moment the table is held. Sorted by text now.

`an_interner_warmed_by_another_program_does_not_reorder_the_link` links a
different program through the session first, arranged so `_other` interns ahead
of `_helper` — the reverse of their name order — and then requires the real
link to equal a cold one. **Verified to fail when the frontier sorts by id**;
the first two versions of that fixture did not, because within one object the
symbol table is already sorted and across a single link ids come out in name
order anyway. A test that cannot fail on the bug it names is worse than none.

### Measured

Same machine, same hour, debug rust-analyzer: 341 inputs, 6,079 objects,
187 MB output. Realistic edit — `crates/ide/src/lib.rs`, 2 inputs, 8% of
objects — through a resident linker, per-stage minima over 6:

```
                  before    after
  read_and_parse   420.6    141.7
  resolve          437.1     61.8
  atoms            118.7     29.5
  relocate         467.0    465.3      untouched
  symbols          102.9    103.1      untouched
  survey            73.2     73.3      untouched
  link            1994.6   1265.0      -37%
```

The three untouched stages moving by less than half a millisecond is what says
the machine was in the same state for both.

Cold, where the interner starts empty and there is nothing to reuse:
**2446 -> 1983 ms, -19%** — one interning pass replacing an interning pass and
two full string-hashing passes.

Output byte-identical on the 187 MB image, cold against warm-through-a-daemon;
62 suites, 647 tests green, and green again under `BLINKER_VERIFY_LIVENESS=1`.

## 158. The name was hashed with the scope, so it was hashed half a million times

`dep_hash(scope, table, name)` is `blake3(scope || table || name)`, truncated
to 64 bits. Putting the scope first is the natural way to write it and it makes
the expensive part — BLAKE3 over a Rust mangled name that routinely runs past
a hundred bytes — depend on the cheap part:

```
  address_table: hash+collect 149 ms   sort+dedup 10 ms   (506,405 entries)
```

Every one of those names had been hashed on the previous link too, and the
previous link's inputs differ from this one's by 8% of objects.

`dependency_hashes` pays it again, per object, for every symbol its relocations
read — inside `apply`.

### Split the hash where the reuse boundary is

```rust
pub fn dep_hash(scope: u32, table: Table, name: &str) -> NameHash {
    combine(name_digest(name), scope, table)
}
```

`name_digest` is BLAKE3 of the name alone, so it is a pure function of the
name and is held beside the interning table from finding 157 — one `u64` per
`SymbolNameId`, computed when the object introducing it was first parsed.
`combine` folds in the scope and table with a splitmix finaliser, which is a
bijection for any fixed `(scope, table)`: two triples collide exactly when
their names' 64-bit digests collide under the xor, which is the birthday bound
the concatenated hash already had.

This is not finding 153 being reversed. BLAKE3 still runs over the name text —
the refusal there was to hash the *name* with fxhash, and that still stands.
What changed is only where the scope enters.

`AddressMap` now carries each entry's `SymbolNameId` beside its address, since
finding the id by name would be the string hash the id exists to avoid.

### Measured

Same edit, same machine, resident linker, per-stage minima over 6:

```
                  before    after
  address_table    133.9     11.1
  apply            146.7     99.4      the dependency lists
  relocate         465.3    286.4
  link            1265.0   1027.8      -19%
```

Cold is unchanged at ~2.0 s, correctly: a cold link hashes every name once
either way.

`SCHEMA` bumped to 4 — the hashes a cache stores are now different numbers,
and a stale layout read as a current one is the cache's one wrong-binary
failure mode.

Output byte-identical on the 187 MB image, cold against warm-through-a-daemon;
62 suites, 647 tests green, and green again under `BLINKER_VERIFY_LIVENESS=1`.

### An aside that looked like a bug

Comparing a `--blinker-cache` link against one without produced different
bytes, which reads as "reuse changed the output" — the one thing that must
never happen. It is not: `--blinker-cache` also turns on `with_stable_layout`,
deliberately, so that a cold link and a cached one lay out the same way. The
flag changes two things and only one of them is the cache. Worth knowing
before the next person diffs those two runs.

## 159. The symbol table was allocated a name at a time, then hashed a name at a time

`emit_linkedit` and `symbols` together came to 281 ms of a 1,028 ms link.
Instrumented:

```
  symbols: placed 8 ms (379,857 placed)  output_symbols 65  debug_map 46
  symtab.build: group-sort 16   strings 146   (1,689,759 symbols, 82 MB of names)
```

1,689,759 symbols out of 379,857 definitions, because the debug map names every
function again — as a `FUN` stab, and often as `GSYM` or `STSYM` too.

### Two costs, both paid per symbol rather than per name

**`OutputSymbol.name` was a `String`.** Every one of those 1.69 million entries
allocated and copied a name that already existed, in the parsed object it came
out of, which outlives the link — to hand the same bytes to a string table that
copies them again. It is now `Cow<'a, str>`: borrowed for the names that exist,
owned for the few thousand the debug map synthesises (a compilation unit's
directory, file and object path).

**The string table deduplicated by text.** One hash of a mangled name per
symbol, 1.69 million times, plus a `memcmp` on every hit — and the hits are the
common case, since the debug map's repeat is exactly what the dedup is for.
`OutputSymbol` now carries a `key`: an opaque identity for the name that the
caller supplies, which for this linker is the interned id from finding 157.
Keyed names deduplicate through a `u32` map; `UNKEYED` falls back to the text.

The contract is *equal keys mean equal names*, and it is the caller's to keep —
a wrong key points a symbol at another symbol's string. What is *not* required
is the converse: a name that appears both keyed and unkeyed simply gets two
copies in the string table, which is a size question and not a correctness one.
`a_keyed_name_and_a_text_matched_one_both_read_back` pins the property that
matters — every symbol's offset points at its own name — across all four paths.

### Measured

```
                  before    after
  output_symbols    65        —       (folded into the numbers below)
  symbols           94.4     77.7
  emit_linkedit    126.8     37.4
  emit             173.5     87.4
  link            1027.8    881.7     -14%
```

Output byte-identical on the 187 MB image — the same SHA-256 as before the
change — which also says the keyed and unkeyed name sets do not overlap on a
real Rust link. 62 suites, 648 tests green, and green under
`BLINKER_VERIFY_LIVENESS=1`.

### The shape, for the third time today

Findings 157, 158 and this one are one mistake made in three places: a question
about a *name* answered once per *occurrence of that name*. Interning, hashing,
allocating. In each case the fix was to move the answer to where the name is
first seen and hand out a subscript. Worth looking for a fourth.

## 160. The fourth one: the address lookup was keyed by the name's text

`unwind` was 105 ms of an 880 ms link, and the code already said where it went:
"`eh_frame_fde_offsets` was 1.00 ms of `fill_unwind_info`'s 1.25". Re-measured
at scale, that ratio held — 95 ms of 110.

### The first fix measured nothing

Inside that function, per `__eh_frame` section, was

```rust
let mut relocations: Vec<&InputRelocation> = object.parsed.relocations
    .iter().filter(|r| r.section == section.id).collect();
```

— a scan of the object's *entire* relocation list to read one section's, which
is quadratic in an object's size, and `relocations_for` did the same. The
parser walks sections in order and appends, so the list is already grouped by
section and the scan can be a binary search. That is now what it is, with the
invariant documented on the accessor and checked in debug builds against the
scan it replaces, so a `ParsedObject` assembled some other way cannot quietly
return a subset of a section's relocations.

**It measured zero**: 102.7 ms before, 104.3 after. Kept anyway — it removes a
quadratic for a `partition_point`, and 3.9 million relocations over 5,637
objects is the wrong shape to leave — but recorded as measuring nothing so
nobody reads a win into it. The lesson is the ordinary one: the hypothesis was
plausible, the arithmetic (5,637 x 690 steps) said 4 ms, and I did not do the
arithmetic first.

### What it actually was

Instrumented per call site:

```
  fde: 265,308 fdes   remap 6 ms   target_address 93 ms   insert 11 ms
```

`target_address` resolves a relocation's symbol through `AddressMap`, which was
`HashMap<&str, u64>` for globals and the same per object for locals. Two hashes
of a mangled Rust name and a `memcmp` per question — and the same lookup runs
once per relocation in `apply`.

`AddressMap` is now keyed by `SymbolNameId`. The asker always holds the symbol,
so it always holds the id; `target_address` takes the object's id vector and
subscripts. The few callers that genuinely start from a name — the GOT and
thread-pointer tables, a few thousand entries — pay one string hash through the
interning table, which is where that cost belongs.

`address_table` got simpler on the way past: it had been carrying an
`Address { name, value }` pair so it could find each entry's id, and the map
now *is* id-keyed, so it iterates it directly.

### Measured

```
                  before    after
  unwind           104.3     67.5
  apply             96.6     59.6
  address_map       36.2     11.4
  address_table     10.9     10.7
  relocate         299.0    188.4
  link             927.0    838.7
```

Output byte-identical on the 187 MB image. 62 suites, 648 tests green, and
green under `BLINKER_VERIFY_LIVENESS=1`.

### Four times now

157 interned the names once. 158 hashed them once. 159 stopped copying them and
deduplicated by id. This one stopped looking them up by text. Every one was the
same sentence — *a question about a name, asked once per occurrence of the
name* — and every one was invisible until the workload was large enough for the
occurrences to outnumber the names by two orders of magnitude.

Together, on the same edit and the same machine: **1,994.6 ms -> 838.7 ms**.

## 161. Fifteen cores, and everything after `read_and_parse` ran on one

Reading and parsing the inputs has been parallel since early on. Nothing after
it was. On the debug rust-analyzer link that left roughly 300 ms of an 830 ms
link — surveying relocations, placing symbols, building the unwind table,
computing addresses — running on one core of fifteen, over 5,637 objects that
do not read each other.

### Chunks, not work stealing

`load_objects` hands inputs out through an atomic cursor. That balances itself,
but it gives each worker an arbitrary *set* of inputs, which is fine there
because every result is filed under its own index.

It is not fine for a pass whose results are concatenated. Which GOT slot a
symbol gets is decided by the order the survey saw it, and that decides every
address after it. So `parallel::map_chunks` cuts the objects into contiguous
chunks, hands out *chunk indices* through the cursor, and merges in chunk
order — the order fixed before any thread starts. Four chunks per thread keeps
it balanced without making the merge the cost.

Each pass then has to state what its merge preserves:

- **survey** — each chunk reports what it saw first, in its own order; the
  global "have I seen this name" is answered once, walking chunks in order. A
  name's place is still where the first object wanting it sits.
- **address_map** — globals merge in chunk order, because two objects may
  define one name and the later one overwrote sequentially. Locals are keyed by
  object, so their keys are disjoint.
- **fde offsets** — merged in order for the same reason.
- **placed symbols, debug map, compact unwind** — concatenated in object order.

### The sort had to be merged, not just parallelised

`output_symbols` sorts 379,857 entries by name. Sorting the chunks in parallel
took it to 4.9 ms — and then a k-way heap merge took **41 ms**, more than the
sequential sort had. A heap pays a sift per element on one core.

Replaced with a tree of two-way merges: the same `n log k` comparisons, but one
comparison per element instead of a sift, and the early rounds — nearly all the
work — run on every core. Ties take the earlier run at every level, which
composes to exactly the order a single sort of the concatenation gives.

### Measured

Same edit, resident linker, per-stage minima:

```
                  before    after
  survey            71.3      7.6      9.4x
  symbols           74.7     29.9
  unwind            61.7     19.1
  address_map       11.0      6.8
  relocate         177.0    126.0
  link             838.7    603.3     -28%
```

Output byte-identical on the 187 MB image at every step — which is the only
thing that says the chunk boundaries did not reach the output. 62 suites, 648
tests green.

One incidental fix on the way past: `survey_relocations` built a
`std::collections::HashSet<SectionId>` — SipHash, not the fast hasher — once
per object, 5,637 times, to hold at most one element. It is a `Vec` now.

## 162. The link built a symbol table to find errors, then dropped the errors

`resolve_symbols` walked every symbol of every object into a full
`SymbolTable` — `resolved`, `candidates`, `rules` and `undefined`, four maps
over half a million names — and then:

```rust
let undefined = table.undefined_symbols();
let outcome = if undefined.is_empty() { Ok(()) } else { Err(...) };
*names = table.into_names();
outcome                     // the table is dropped here
```

Instrumented on a debug rust-analyzer link: **93.3 ms**, against 12.8 ms for
the `resolve_imports` call beside it.

### Two things were wrong, and the second is not a performance problem

**The undefined check was already done.** `resolve_imports` computes the
undefined set and errors on anything the stub libraries do not provide, before
this runs. It is the stricter of the two — it counts a weak-undefined
reference as undefined where the table would allow it — so it always fires
first. The 93 ms re-derived an answer that had already been given.

**The duplicate check was never done at all.** `SymbolTable` collects
`SymbolError::Duplicate` for a name defined strongly twice — the rule the
symbols module's own documentation calls the dangerous one, tested since the
crate was written — and nothing in the linker has ever called `errors()`.
`grep` finds no caller outside that crate's tests. So a program with two strong
definitions of one name linked, ran, and called whichever definition arrived
first, with nothing in the build log to say so.

That is the failure mode the whole module was designed around, and it was
unreachable because the link never asked the question.

### What replaced it

`duplicate_definitions` — one pass, counting strong non-local definitions into
a `Vec<u8>` indexed by name id. No hashing at all: the ids are dense from zero,
so the count is a subscript. Anything above one is a duplicate.

`explain_duplicates` builds the full table, but only for the names that already
failed, and only on the path that is about to return an error. A diagnostic
that names every competing definition is worth 93 ms when there *is* an error;
it is worth nothing on the 100% of links where there is not.

`two_strong_definitions_of_one_name_are_refused` compiles three objects, two of
which define `_shared`, and requires the link to fail and to name both files.
It fails against the previous code, which linked them happily.

### Measured

```
  resolve stage    71.2 ms -> ~10 ms
```

rust-analyzer has no duplicate strong definitions, so the new check fires on
nothing and the 187 MB output is byte-identical. 62 suites, 649 tests green.

### The shape

Finding 135 is "a container whose final size is a known function of the input,
built by repeated insertion from empty". This is its sibling: **a container
built to be searched once, for something that is almost never there.** The fix
is the same shape as an exception path — do the cheap test always, pay for the
explanation only when you owe one.

## 163. There was no hidden fifty milliseconds

The stage table reported 36–50 ms "unmeasured", and a review of the data flow
pointed at three quadratic `got.iter().any(...)` scans in the untimed region as
the likely cause. Both were wrong, and finding out cost a `gap!` macro and two
runs.

The GOT assembly, with the loops still in it: **0.3 ms**, 2,520 entries against
265 imports. Quadratic in shape, negligible in size — worth fixing for the
shape, worth nothing for the clock.

Bracketing every untimed region of `link_inner` accounts for the rest:

```
  read_and_parse             145.2 ms
  dead_strip                  67.0
  prepare                      1.2
  resolve                      9.5
  survey                       8.6
  got + prepare (part two)    12.5
  probe layout + cache load   50.6      <- the "unmeasured" time
  relocate                   134.9
  symbols                     32.6
  emit                        82.9
  accounting                  10.7
  image bytes into the cache   3.6
```

The "unmeasured" number was an artefact of the harness's stage list, not work
hiding from it: the probe layout is a real stage that the table reports
separately, and the arithmetic double-counted the gap around it.

`gap!` is kept, env-gated on `BLINKER_GAP_PARTS`, because the question it
answers — *is there work between the stages?* — is one worth being able to ask
in one command rather than by argument.

### What the profile looks like now

Nothing is above 25% and nothing is hiding. That is a different situation from
the one this session started in, where `resolve` and `read_and_parse` were each
a fifth of the link and one nested loop was worth 790 ms. From here on, every
remaining win is either structural — making a stage's work proportional to what
changed — or it is nothing.

## 164. The interner was the one name map that never got the fast hasher

`blinker_hashing`'s module documentation has a section headed **"Names too"**,
written after a profile showed 1136 samples still in `SipHasher::write` once
the `(object, section)` maps had been converted. Every name-keyed map in the
linker was moved over.

Except the interner, which is *the* name map — probed 981,253 times on a debug
rust-analyzer link, once per symbol of every object. `crates/symbols` did not
depend on `blinker_hashing` at all, so `HashMap<Arc<str>, SymbolNameId>` had
`std`'s SipHash the whole time.

```
  intern, 981,253 symbols     481 ms
  after one line and one dep  394 ms
```

### Then the same thing finding 135 keeps being about

`HashMap` does not store the hash it computed, so every growth rehashes every
key already in it — and rehashing a key here meant chasing an `Arc` pointer
into scattered memory and hashing sixty bytes of mangled name again. Growing to
477,532 entries from empty is nineteen doublings; the sum is about a million
string hashes nobody asked for.

`SymbolNames` now holds every name end to end in one buffer, an id is a span in
it, and the index is keyed by the name's **hash** rather than its text:

```rust
arena:  Vec<u8>,                        // all names, concatenated
spans:  Vec<(u32, u32)>,                // id -> where its bytes are
lookup: FastMap<u64, Few<SymbolNameId>> // hash -> the ids that hash there
```

A `u64` key rehashes in two instructions and dereferences nothing, so growth is
nearly free. Comparing a candidate reads a contiguous slice instead of
following a pointer. And 477,532 `Arc` allocations become zero.

Names that hash alike are kept together and told apart by their text, so this
is a lookup structure and not a claim that the hash is unique — the distinction
finding 153 turns on.

```
  intern, 981,253 symbols     394 ms -> 256 ms
```

### The digests moved to where they could be spread

`catch_up` hashes each new name with BLAKE3 for the address table — 147 ms
cold — and it ran inside `interned()`, once per object, in increments of about
a hundred and seventy names. Far too few to start a thread for.

It runs once per link now, from `digests()`, where the increment is the whole
link's new names: 477,532 cold and about 90,000 on an edit. That is enough to
hand to `map_chunks`, and it is the same deferral in kind as the one in finding
162 — do the work where its size makes the right technique obvious, not where
the data happens to arrive.

### Measured

Back to back on one machine, cold link of debug rust-analyzer:

```
  cold   2786 ms -> 1916 ms    -31%
```

Warm is unchanged, correctly: a held object's names were never re-interned in
the first place. Output byte-identical on the 187 MB image; 62 suites, 649
tests green.

### Why it took until now to see

A cold link's interning was invisible while `resolve` was 437 ms and
`read_and_parse` was 991: it *was* those numbers, attributed to the stages that
called it. It only became a name of its own once both had been taken apart, and
the instrumentation that found it took two minutes to write. The lesson is not
"profile more" — it is that a cost hides inside whichever stage is currently
the biggest, and it keeps hiding until that stage stops being the biggest.

## 165. The projections were memoised but never built in parallel

`ObjectAtoms` is a pure function of one object — that is the whole reason it can
be held across links (finding 133). A cold link holds none of them and builds
all 5,637, one after another, on one core.

`Session::atoms(parse, compute)` made that hard to see: the memo lookup and the
work were the same call, and the session cannot cross a thread boundary. Split
into `held_atoms` (ask) and `store_atoms` (file), the work in between is a
`map_chunks` over the objects that missed.

```
  cold   1115 ms -> 987 ms
```

On a warm link it changes nothing measurable, correctly — one projection of
5,637 misses, and starting fifteen threads to build it would be the cost.

### On the numbers in findings 163 and 164

Those cold figures — 2786 ms before the interner work, 1916 after — were taken
on a machine carrying a load average of 14 to 26 from unrelated processes. The
same binaries measure 1115 and about 1000 on a quiet one. The *ratio* held up
(−31% then, and the A/B here was back to back), but the absolutes were inflated
by about 70%, and this file should not be read as if they were not.

Quoting a number without the machine's state is how a measurement stops being
one.

## 166. The relocation pass writes to disjoint bytes, and the compiler could not see it

`apply_relocations` is 252.7 ms of a 1047 ms cold link — the largest single
sub-stage, and the last one still running on one core.

It is embarrassingly parallel and always was. A relocation patches at
`chunk_offset + field`: the chunk is *that object's contribution*, and the
field is bounded by the input section's size. Two objects never touch the same
byte. What stopped it was that all of them wrote through one `Vec<u8>` per
output section, and no amount of that being true lets `&mut` be handed to
fifteen threads.

### Cut the buffers where the layout already cuts them

`carve` splits every output section's buffer into its contributions, in offset
order, by repeated `split_at_mut`. Each object gets a small `ObjectBytes`
holding one `&mut [u8]` per contribution it owns. The property the layout
guarantees becomes one the borrow checker holds, and nothing is copied — the
slices are the same bytes.

It also simplifies the write: the field's offset within the slice is just
`field`, because the slice starts where `chunk_offset` pointed.

`carve` returns `None` if any section's contributions overlap or run past its
end, and the caller then has no slices to hand out. That has never happened. It
is checked because the alternative is trusting the layout to be an invariant it
merely is.

### Landed in two steps, because this is the code that must not be wrong

First the carving, with the loop still sequential — output byte-identical,
which says the slices cover exactly what the whole buffers did. Only then the
threads. A single commit doing both would have left "the bytes changed" with
two possible causes.

### What the merge has to preserve

Each chunk accumulates its own binds, rebases and records, and an
`ObjectRecord`'s `binds`/`rebases` ranges are *chunk-local* — they mean nothing
except against the vector they are appended to. The merge sorts the chunks back
into object order and rebases every range as it concatenates.

Chunks are handed out through a queue rather than split evenly. An object whose
bytes came from the cache costs nothing and one that is relocated costs
everything, and on an edit the expensive ones are the objects of the crate that
changed — which are consecutive. A static split would put all of them on one
thread.

An error is no longer returned by whichever thread found it first: the chunks
are ordered before the first `?`, so a link with two bad relocations reports the
same one every time.

### Measured

```
  apply       252.7 ms -> 27.6 ms    9.2x
  relocate    312.9    -> 83.0
  cold link      987   -> 820
```

Output byte-identical on the 187 MB image; 62 suites, 649 tests green.

## 167. Sharding the interner measured zero, and the reason is worth keeping

Cold `read_and_parse` is 340 ms of an 820 ms link, and interning is 128 of it —
sequential, while everything around it now runs on fifteen cores. The obvious
move is to shard the table so threads can fill it without a lock.

Built it: 64 shards, id encoding the shard as `index * SHARDS + shard`,
digests moved into the shard beside the spans. Then measured it back to back
against the single table, twice, on the same machine:

```
  sharded      836 ms
  one table    827 ms
```

Zero. **Reverted.**

### What the detour taught, which is why it is written down

**The first number was 1003 ms, and that was a real bug.** The shard was taken
from the *top* eight bits of the hash and the same hash was handed to the
shard's table. `hashbrown` keeps a seven-bit tag from the top of the key, so
every name in a shard carried the same tag, every probe matched every entry's
tag, and each lookup degenerated into comparing the text of the whole bucket.
Taking the shard from the low bits and the key from what is left above them cost
one line and 84 ms. Two hash functions derived from one hash must not want the
same bits.

**The second number was 934 ms at every shard count**, including eight — which
should have been indistinguishable from one. That ruled sharding out as the
cause and pointed at what else the rewrite had changed: the BLAKE3 digests had
moved *into* `intern`, one name at a time, undoing the parallel batch of
finding 164. 114 ms, and nothing to do with shards.

**The third number was the honest one**, and it says the cache theory was
wrong. Sixty-four tables of seven thousand names are 120 KB each and should be
L2-resident where one table of 477,532 is 8 MB and never is. It made no
difference, because the *table* was never the working set — the arena is, and
comparing a candidate name touches 60 bytes of it wherever it happens to live.
Splitting the index does not split the text it points at.

### The rule this is an instance of

Machinery whose only justification is a change not yet made does not earn its
place. Parallel interning is worth about 110 ms and it does need a shardable
table — but the table is worth building when the parallel phase is built, not
before, and on today's evidence the two have to land together or not at all.

Three measurements, two of them measuring my own mistakes rather than the
design. That is the normal ratio, and the alternative — keeping it because the
argument was good — is how a codebase fills up with machinery nobody can
justify later.

## 168. `read_and_parse` was not parsing

The 340 ms `read_and_parse` of a cold rust-analyzer link had been carried for a
while as "extraction rounds 241, of which member parse ~102". That number was
never measured; it was inferred from the fact that the rounds are where members
get parsed. Bracketing the loop says otherwise:

```
want    10.9 ms   resolve the wanted names, copy them out, sort by text
pick    25.3 ms   `defining.get` per wanted name
parse   23.9 ms   parse the round's members, on fifteen cores
store   49.4 ms   `session.store_member`
intern 116.7 ms   `session.interned`
absorb   7.5 ms   `frontier.absorb`
```

Parsing is 24 ms — a tenth of the stage and within a few milliseconds of what a
standalone benchmark of the same 5,504 members predicts. The stage named "read
and parse" spends 93% of itself on everything except reading and parsing.

This matters beyond the arithmetic. The whole of the previous session's plan for
this stage — intern names at parse time, drop `InputSymbol.name: String`, reach
for a faster parser — was aimed at the 102 ms. A throwaway A/B on the allocation
half says what that plan was worth: removing the `String` per symbol takes the
serial parse of 213k symbols from 16.5 ms to 11.8, and the *parallel* parse from
2.30 ms to 1.39. Scaled to the real link, about 4 ms of wall clock for a
forty-call-site refactor, because the parse has been on fifteen cores for a long
time and 976,000 allocations spread over fifteen cores are not 976,000
allocations. A number that sounds appalling and costs nothing.

### What was actually there

`store` was 49 ms, of which 46 was `interface_digest` — a walk of every global
symbol of every member, hashing its name. It is a pure function of the parse,
computed one member at a time, on the thread that had *just finished parsing all
of them in parallel*. Working the round's digests out together and reading them
back out of the memo is 60 ms off the stage (367 → 307 on an interleaved A/B)
and does not change a byte of the output.

The seeding is not load-bearing: a parse whose digest is not held is digested
where it is asked for, exactly as before. That is the property worth keeping —
the fast path is an optimisation of the slow one, not a replacement that the
slow one has to stay consistent with.

## 169. Interning is not hashing, and precomputing the hash paid for the wrong reason

Interning was 117 ms of the cold link and the obvious read of that is "976,000
names, sixty bytes each, hashed one at a time on one core while fifteen sit
idle". Finding 167 had already built and reverted a sharded table on that
premise. So this time the split was measured before anything was designed:
hash every name of a round in parallel, hand the hash to `intern_hashed`, and
time the two halves.

```
hash    7-10 ms   every name of the link, on fifteen cores
probe  89-109 ms  the table walk that follows
```

Hashing is 7% of interning. The 110 ms is not computation at all — it is three
*dependent* cache misses per name: the bucket in a 477,532-entry index, the span
it names, and the arena text the span points at, each address unknown until the
previous load returns. That is also the missing half of finding 167's autopsy:
sharding the index could not help because the index was never the cost.

### The change was still worth 50 ms, for a reason that was not the plan

Interleaved, twelve runs each: 734 ms against 791 ms. Fifty milliseconds from
moving seven milliseconds of work. The hashing was never the point.

What changed is the *shape of the serial loop*. It used to be

```text
load the name pointer -> read 60 bytes -> hash them -> probe the table
```

— one dependency chain per symbol, and the probe's address is not known until
the hash finishes, so the machine cannot start the next miss until this one has
landed. With the hashes already sitting in a contiguous `Vec<u64>`, the bucket
address for symbol *i+k* is available immediately and the out-of-order window
fills with overlapping misses. Same instructions, same order, same cache
misses — issued concurrently instead of one after another.

### The rule this is an instance of

A serial loop over a large structure is limited by its *dependency chain*, not
its instruction count, and precomputing the head of that chain into a dense
array is a way to break it that has nothing to do with the parallelism it looks
like. It is why the win survived being 15× larger than the work that was moved,
and why measuring the halves before designing was worth more than the design.

## 170. The interner's probe was a barrier, and lifting it was the other half

Finding 169 established that interning 976,000 names is 7 ms of hashing and 110
ms of waiting: three dependent loads per name — the bucket, the span it names,
the arena text the span points at. It moved the hashing out and got 50 ms, not
from the parallelism but from letting the probes overlap.

The probe itself stayed serial for a reason that turned out not to be one.
`intern_hashed` *may* insert, and an iteration that may insert is a barrier: the
next name's chain cannot start until this one has finished with the table. So
the loop ran one name in flight, a million times over.

But the question "does the table already hold this name?" touches nothing. Split
out as `get_hashed`, a whole round's worth goes to every core at once, and what
comes back is an id for every name the table already had. Only names that were
new are left to file away in order — and *those* have to be serial, because
which id each one gets depends on how many came before it.

```
before   976,000 serial probes + 477,532 serial inserts
after    976,000 parallel probes + 477,532 serial probes + 477,532 serial inserts
```

Predicted about 35 ms; measured -33.8, -52.2 and -33.1 ms over three interleaved
runs, against a noise floor of -15.4, -3.7 and +6.6 on the same loaded machine.
The prediction agreeing with the measurement is what makes three noisy runs
worth believing.

### Why this one gets better warm, not worse

A cold link is the *bad* case for this split: half of what the cores answer is
"no", and the serial phase does that half again. A warm link's names are almost
all in the table already, so the serial phase is nearly empty and the whole
probe is parallel. The usual shape of an optimisation is the opposite.

## 171. The warm link is proportional to the program, not to the edit

A resident relink of debug rust-analyzer after a one-line edit: 341 inputs, 326
of them held, **one** of 5,637 objects' reachability projections moved, 9,719 of
9,722 contributions kept their address. Everything the incremental machinery
claims is true. The link still takes 600 ms against `ld-prime`'s 332 ms cold.

Bracketing it says where:

```
load: archive indexes    65 ms    the parallel input load, all 341 of them
load: defining map       22 ms    name -> defining member, rebuilt from held indexes
extraction preamble +    74 ms    of which the rounds themselves are ~50 us:
  rounds                          the cost is `frontier.absorb` over 5,637 objects
dead_strip               66 ms    1 of 5,637 projections moved
relocate                104 ms
emit                     88 ms
probe layout + cache     41 ms
symbols                  27 ms
```

The extraction *rounds* — the thing the session's extraction replay exists to
skip — cost fifty microseconds. Everything expensive is a pass over the whole
program that happens to be re-derived from held data rather than re-read from
disk. Holding the inputs removed the I/O and the parsing; it did not remove the
work that is proportional to what was parsed.

`dead_strip` is the clearest case, because the machinery is all there and does
not fire. `Session::strip` returns the previous link's answer only when *every*
projection digest is unchanged — so it holds on a no-op relink and never on a
real one, since an edit changes at least one object by definition. One object in
5,637 rebuilds the atom index, the owners map, the resolved-name table, the full
graph traversal and the compacted strip map. The incremental answer needs a live
set with incoming counts that can be updated for the objects that moved; the
all-or-nothing check is a placeholder that measures as one on the only workload
that matters.

### What this corrects

An outside review of the project read the README — which still said 2.92x, "no
debug map", and "the daemon is not implemented" — and concluded the gap was
owned `String`s in the symbol pipeline. Findings 168 and 169 had already
measured that half at about 4 ms. The review's *structural* claim was right and
its mechanism was wrong, and the stale README is why. Numbers left lying around
are read as current.

## 172. Two suggestions from the same paragraph: one already true, one a wash

An outside review proposed replacing `OutputSymbol.name: String` with

```rust
enum NameRef<'a> { Empty, Borrowed(&'a str), Owned(Box<str>) }
```

and, in the next breath, replacing the stable sort that puts the symbol table
into `LC_DYSYMTAB` group order with three vectors concatenated. Both were
answered by measurement rather than argument, and they came out differently.

### The borrowed name was already there, and the enum is the same size

Finding 159 moved that field to `Cow<'a, str>` for exactly the stated reason.
What was left of the suggestion was the shape: `Box<str>` in the owned arm
instead of `String`, saving eight bytes in a struct built 1.7 million times.

```text
Cow<str>   24        String    24
NameRef    24        Box<str>  16
```

`OutputSymbol` is 48 bytes either way. `Cow`'s niche optimisation already packs
it into the space `String` alone occupies, so the proposed enum is byte-for-byte
the same. A representation argument that is obviously right can still be worth
nothing, and `size_of` settles it in less time than reasoning about it does.

### The sort was real, and removing it bought its own cost back

`sort_by_key(|s| s.group)` over 1,689,759 entries of 48 bytes — 81 MB
rearranged to order a three-valued key — measured **10.8 to 17.1 ms of every
link**. Walking the symbols once per group visits them in precisely the order
the stable sort produced, so the entries and the string-table offsets come out
identical, and the group counts fall out of the same walk.

```text
build()   with sort   min 31.1 ms   median 43.2 ms
          no sort     min 32.0 ms   median 32.8 ms   (n=12 each)
```

**The minimum did not move.** The sort's 11-17 ms came back as two extra
streaming passes over the same 81 MB. What did change is the median, by 10.4 ms,
and the transient allocation: a stable sort of 1.7 million elements asks for a
merge buffer, and that is what makes the bad runs bad.

Kept, but not as a 10 ms win — as one less large allocation and one less 81 MB
move, at equal best-case cost. Claiming the median here would be claiming the
machine's load as a result.

### The rule

Two suggestions, one paragraph, one reviewer, equal confidence in both. One was
already implemented and the remaining delta was zero by construction; the other
named a real 17 ms and delivered none of it. Neither outcome was predictable
from the argument, and both took under an hour to settle by measuring.

## 173. The same shape again, in the archive frontier

Finding 171 bracketed a warm relink and found the extraction rounds costing
86 ms — on a link where every member but one archive's came out of the session.
Broken down:

```
want   10.9 ms   resolve the wanted ids to text, copy them out, sort
pick   26.8 ms   `defining.get` for each
parse  13.5 ms   the members of the one rlib that changed
store  24.9 ms
absorb  9.1 ms
```

`pick` is sixty-two thousand lookups at 450 ns each. Not because there are many
— because each one hashes sixty bytes of mangled name, misses into a
half-million-entry table, and chases a pointer to the `String` the entry holds
to compare the text. Three dependent loads, one name in flight at a time. It is
finding 170's shape exactly, in a different table, found by looking for it.

And the same fix works, more simply than it did in the interner: `defining` is
**read-only** for the whole round, so there is no insert to serialise against
and no second phase to file the misses. The round's names go to every core in
one `map_chunks` and the answers come back in `wanted` order — which is the
order members are pulled in, and therefore what id each one gets, so preserving
it is the whole safety argument.

```
pick   before  26.5 27.0 27.7 31.9 26.3 24.6   median 26.8 ms
       after    7.2  9.2 14.1  9.7  7.3  7.3   median  8.2 ms
```

Nineteen milliseconds a warm link, output byte-identical.

### Worth noting about how it was found

The measurement that produced finding 171's table was misread first: the
counters are microseconds accumulated per link, and reading them as cumulative
across links made the extraction rounds look like 50 microseconds of a 600 ms
link — a stage already perfect and not worth touching. They were 86 ms. The
numbers were right and the sentence about them was wrong, which is the failure
mode instrumentation is *least* protected against: a wrong reading of a correct
measurement looks exactly like a correct one.

## 174. A re-read archive threw away 3,373 of 5,637 parses, and all but one were unchanged

Finding 171 said the warm link was proportional to the program rather than to
the edit, and named dead stripping as the clearest case. It was not the biggest
one. `Atoms::build` was asked for 5,637 projections and found **3,373 of them
missing from the memo** — on a link where one object's projection had moved.

The 3,373 are exactly the objects the report already called not-reused. They
were missing because `store_archive` dropped every member of a re-read archive,
on the argument that a re-read archive's contents are gone. That is true of the
bytes and false of what was parsed out of them:

```
store_archive libhir-853dac6d2ed02627.rlib      drops 256 members
store_archive libhir_ty-2b5c28c55eb666cd.rlib   drops 256 members
store_archive libhir_def-0e34e4ac5d6b156a.rlib  drops 256 members
...15 archives, 3,373 members
```

Those fifteen are the crates downstream of the edited one, so rustc did
recompile them all. What it produced is the point:

```
libhir      256 vs 256 objects; 256/256 identical at the SAME index
libhir_def  256 vs 256 objects; 256/256 identical at the SAME index
libbase_db  225 vs 225 objects; 224/225 identical   <- the edited crate itself
```

Byte-for-byte the same objects, at the same index, under different names —
`hir-853dac...98hqlx8xschvz3oynqv2fng6t.195puae.rcgu.o` became
`...98hqlx8xschvz3oynqv2fng6t.1i137y1.rcgu.o`. The trailing component is
rustc's per-build session id. This is finding 144 exactly, the phenomenon the
content index was built for — happening *inside* archives, where nobody had
applied it. Even the crate that was actually edited changed one codegen unit of
225.

So the members are kept, and served only after proving the new archive holds
the same bytes at that index. A `memcmp` rather than a digest: both sides are
already mapped, there is nothing to gain by hashing 400 MB to avoid comparing
it, and a comparison cannot collide.

**Warm relink of debug rust-analyzer: 610 ms to 515 ms.**

### The half that was nearly a silent wrong answer

A held parse carries the member name it was parsed under, and that name reaches
the output: the `OSO` stab names the object file a debugger will open. Serving
the old parse under the new name would have emitted a member that no longer
exists — a debug map pointing at nothing, in a binary that links and runs.

`identity.rs` already had the rule written down for the *path*: take it from the
link, never from the parse, "so a warm link and a cold one would disagree about
what the same bytes are called". The member name needed the same rule and did
not have it, because until now a held parse could never outlive its archive.

The regression test compiles with `-g`, and the first version of it did not.
Without debug information there is no debug map, so it passed with the name
taken from either place — a test of the thing that cannot fail. It now fails
against exactly that sabotage, which is also what proves the reuse is happening
at all.

## 175. Two changes to one line: one was a memory fix, the other was the speed fix

`store_archive` was 26 ms of a warm link, all of it for fifteen re-read rlibs —
1.7 ms each. It held each archive's external symbol table so the next link could
compare against it, which meant cloning every symbol name of every re-read
archive to build the copy, comparing the two, and keeping the new one.

Replacing the stored table with a digest of it is the obvious fix and it
**measured nothing**: 25-37 ms before, 25-37 ms after. The clone was never the
cost. What both versions have in common is a pass over every name — hashing it,
or copying it, plus `is_module_unique`'s substring scan for `.llvm.` — and
fifteen large rlibs is on the order of ninety megabytes of names.

The digest is a pure function of the index, and the index is built on a worker
thread, in parallel, immediately before. Moving the digest there costs nothing
that was not already being paid in parallel.

```
read_and_parse   145 ms -> 131 ms
```

### Keeping the change that measured zero

By finding 167's rule the digest should have been reverted. It was kept, for a
reason that has nothing to do with time: the table it replaces was **held for
the life of the session** — 208 archives' worth of symbol names, around forty
megabytes of `String`, retained so that a comparison could be made against it
once per link. A `u64` per archive answers the same question.

That is a different justification and it is worth stating as one rather than
letting a speed number that does not exist stand in for it. A resident linker
that holds forty megabytes it does not need is a real cost; it is just not a
cost the stopwatch was measuring.

## 176. The proof that replaced the work became the work

Finding 174 stopped re-parsing 3,373 unchanged archive members by proving each
one unchanged instead — a `memcmp` of the member against the bytes it was parsed
from. The re-parsing collapsed exactly as intended:

```
members re-parsed per warm link   3,373  ->  1
parse                              13.5 ms -> 0.6 ms
store                              24.9 ms -> 0.2 ms
```

And the round got *slower* somewhere else, because a round is 5,504 members and
proving all of them is comparing the whole archive set: 800 MB, on one thread,
about 80 ms. Cheaper than parsing them, which is why the change was still worth
80 ms overall — and it had quietly become the largest single item in the stage.

Nothing about it is sequential. `Session::member` takes `&self`; a member's
answer does not depend on any other member's; the only thing the chunking has
to preserve is the position each answer belongs at. On every core:

```
extraction rounds   60 ms -> 37 ms
```

### What to take from it

An optimisation that replaces work with a *proof* that the work is unnecessary
has to be costed like work, because that is what it is. The proof was 800 MB of
comparison standing in for 3,373 parses, and both of those numbers are
proportional to the whole program rather than to the edit — the second one was
just smaller. It showed up because the instrumentation from finding 171 was
still in the tree and got re-run after the change rather than before it.

## 177. The image was hashed twice, and the second pass was the whole signature

`content_uuid`'s own doc explains that it is built "from the same page hashes
the signature uses rather than from a second pass over the whole image" — a
finding in its own right, worth 4 ms when it was made. It then calls
`page_hashes` itself, and `sign_reusing` calls `page_hashes` again a few lines
later. Both hash 187 MB. The profile said so plainly and had for a long time:

```
emit_uuid   12.56 ms
emit_sign   11.65 ms
```

Two numbers that close together, for two things that hash the same bytes, is
the shape of the same work done twice.

They cannot simply share, because the UUID is stamped into the image *between*
them — so the second pass is hashing a genuinely different image. Different by
sixteen bytes, in one page, of about forty-five thousand. So the hashes are
computed once, the page the UUID landed in is re-hashed, and the signature is
built from the result.

```
emit_sign   11.65 ms -> 0.06 ms
```

Output byte-identical, and `codesign --verify` still accepts the image — which
is the check that matters here, because a stale page hash is not a wrong number
in a report, it is a binary the kernel refuses to run.

### Why it survived so long

The doc comment describes the design that was intended, and the design is
right: derive the UUID from the page hashes instead of a second full pass. What
it does not say is who computes them, and the answer turned out to be "both
callers, separately". A comment that describes an optimisation is not evidence
the optimisation is still in force, and this one was load-bearing enough to read
as one.

## 178. Four passes over the same three stages

The instruction was to make `read_and_parse`, `emit` and `relocate` smaller.
Each yielded to a different fix, and none of them was the one the stage's name
suggests.

**`read_and_parse`, the extraction rounds.** Finding 176's parallel `memcmp`
took the rounds from 60 ms to 37. What was left was `want` at 13-23 ms: sixty-two
thousand `String`s allocated per link so that the interning table's borrow could
end before the session was needed mutably again. Scoping the borrow to the block
that reads it removes the copies and sorts `&str` instead.

```
read_and_parse   127 ms -> 99 ms
```

**`emit`, the two hashes.** Finding 177: the image was hashed twice, and the
second pass was the entire signature. 11.65 ms -> 0.06 ms.

**`emit`, the keyed dedup.** The string table deduplicates names by the caller's
interning id, through a `FastMap<u32, u32>` sized for 1.7 million symbols — 13 MB
of table, probed 759,597 times, a cache miss each. An interning id is a *dense
integer from zero*, so the index wants to be a vector: four bytes per distinct
name, 2 MB, resident in L2, and the probe is a bounds check and a load.

```
emit_linkedit    38 ms -> 32 ms
```

The map was chosen deliberately and its comment argues, correctly, that hashing
four bytes beats hashing a hundred. That was the right comparison against the
*text* index next to it and the wrong one against not hashing at all — the
question a map answers is "is this key present", and for a dense key that is an
array subscript.

**`relocate`.** Bracketed and left alone. `unwind` is 20 ms of it — 11 finding
where each function's FDE landed, 6.7 encoding, 2.9 collecting — and `apply` is
already on every core (166). Nothing here has the shape the other three had, and
guessing at it would be inventing work rather than removing it.

## 179. `relocate` was not the problem; the reuse plan was refusing to fire

Finding 178 bracketed `relocate` and said so: `unwind` and `apply` had no
obvious shape, and guessing would be inventing work. The way back in was not a
profiler. It was one line of the relink report:

```
addresses: 197/506405 changed (0.04%)
reused 2264/5637 objects, 1002916/3887921 relocations (26%)
```

Nothing about a program whose addressing is 99.96% unchanged explains reusing
26% of its relocations. `apply` was 24 ms because three quarters of the work it
was doing had already been done by the previous link.

Counting the *reasons* the plan turned objects down, rather than the rate:

```
plan_reuse: 2264 of 5637 kept
  turned down  no-entry 3  key 3370  ranges 0  deps 0
```

`deps 0` is the load-bearing number. Not one object was rejected for reading an
address that moved — which is the condition the cache exists to check, and the
only one whose failure means the bytes are actually stale. Every rejection came
from the identity test in front of it.

### A member has no file of its own

The identity test is an `InputKey`: a content hash, or for a content-addressed
path a `(path, mtime, size)`. It answers "is this the input the cached entry was
built from", and it answers it *about a file*. An archive member is not a file.
The key it gets is its archive's, and an rlib is rewritten whenever anything in
the crate is recompiled — so a crate downstream of an edit gets a new archive
key and every member inside it looks changed.

The fifteen rlibs that failed hold 3,370 of the link's 5,637 objects. All 3,370
were relocated again to produce, byte for byte, what the cache was already
holding.

This is finding 174's phenomenon at the next stage down. There, a re-read
archive threw away parses whose bytes had not changed; here, the same archive
throws away *relocations* whose inputs had not changed. Both had the same cause
— reasoning about the archive when the question was about the member — and 174
had already built the answer to it: `Session::member` serves a held parse only
after proving the new archive holds the same bytes at that index. That proof is
strictly stronger than the key it stands in for. A `LoadedObject` now carries
whether it was served that way, and the plan asks the flag instead of the key.

```
apply       24.5 ms -> 3.3 ms      relocations reused  26% -> 98%
relocate    92.6 ms -> 77.7 ms     link  ~426 ms -> ~406 ms
```

`cache_plan` grew 4.8 -> 10.4 ms, which is the honest cost of the change: it now
inserts 5,587 entries where it inserted 2,264.

### The test passed before it should have

The first version asserted `reused_objects > 0`, and it passed against a
deliberate sabotage of the flag. The fixture links three objects — `main.o` and
two archive members — and `main.o` is a file, so its key is a content hash and
it reuses correctly no matter what the members do. An assertion that only counts
reuse is satisfied by the one object that was never in question. `assert_eq!(3)`
fails at 1, which is the number the bug produces.

Then it passed against the sabotage *again*, and that one was not the test's
fault: restoring `lib.rs` from a `sed -i.bak` backup restores its old mtime, so
cargo saw nothing newer than the build and re-ran the sabotaged binary. A
verification that silently tests the previous build is worse than no
verification, and it looks identical to a passing one. `touch` the file after
any restore-by-move.

### What this says about where to look

Three stages had been profiled down in a row, and this was worth more than any
of them — from a counter that had been printing the whole time. A stage timer
says how long something took; it cannot say that the work was avoidable. The
reuse rate could, and it was sitting at 26% next to an address-change rate of
0.04% for as long as both had been printed.

## 180. The scan that finding 179 unlocked, and a regression that was not there

Finding 179 took the reuse plan from 2,264 objects to 5,587, and inherited the
work it had been skipping. `is_reusable` rejects on the input key *before* the
dependency scan, so 3,370 objects had been costing one comparison each; now
every one of them scans its whole dependency list. `cache_plan` went 4.8 ->
10.4 ms, which for a while was larger than the `apply` it had just made cheap.

The scan is 3.9 million `NameHash`es — 30 MB — probed against a set of 197.
Nothing in it writes anything shared. It is finding 176's shape exactly, one
stage further along: the proof that replaced the work became the work. Probing
the input keys first (341 distinct files, once each) leaves a decision that
touches only its arguments, so it runs on every core and the answers merge in
chunk order.

```
cache_plan  10.5 ms -> 2.4 ms      relocate  77 ms -> 66 ms
```

The same pass also stopped building two half-million-entry `LinkCache`es per
link. `changed_addresses` was a method, both callers had the address table and
no cache to call it on, so both wrapped the table in a throwaway cache —
copying 506,405 entries to reach a function that only reads them.

### The regression that was noise

Six iterations of the small workload put the change 1 ms behind:

```
[1 head]  12.2 ms    [1 final]  13.3 ms
[2 head]  13.0 ms    [2 final]  13.4 ms
```

which is a coherent story — 238 objects and 88,000 dependencies is less work
than spawning fifteen threads — and the fix for it was already drafted: a
work-size threshold below which the pass stays serial. Twelve iterations
instead of six:

```
[1 head]  12.3    [1 final]  12.2
[2 head]  12.1    [2 final]  12.0
[3 head]  13.1    [3 final]  12.4
[4 head]  12.0    [4 final]  12.0
```

There was nothing to fix. The threshold would have been a constant tuned to a
number that did not exist, and it would have looked justified forever after,
because a serial path on a small link is not obviously wrong.

This is the same trap as `scripts/ab.py`'s noise floor (whose two-copies-of-one-
binary spread ran from +6.6 to -21.5 ms) with one difference: the wrong reading
here came with a *mechanism*. A plausible explanation for a measurement is not
evidence for it, and it is most dangerous when it arrives first.

## 181. Three stages named, and only two of them moved

The instruction was `read_and_parse`, `emit` and `dead_strip`. Two moved and
one did not, and the one that did not is the more useful entry.

### `read_and_parse` 99 -> 77 ms

**A sort for a search nobody performs.** `index_archive` sorted each archive's
symbol table by name so `member_defining` could binary-search it. Nothing calls
`member_defining` any more: finding 78 replaced the per-archive search with one
index built across every archive at once, and that index reads the table by
scanning it. The sort stayed.

```
index  200601904 bytes   73095 symbols   258 members   parse 12.5   sort 12.9
```

Half the cost of indexing rust-analyzer's largest rlib, ordering a table for a
lookup that stopped happening. And it is on the critical path in the worst
possible way: fifteen archives are re-read on an edit, one per core, so the
stage's wall clock is whatever the largest one costs alone.

The ordering that resolution actually depends on survives without it. The
extraction round takes the *first* entry for a name, and an archive's symbol
table already lists members in archive order, so the first occurrence names the
same member the stable sort put first. Every output hash was unchanged, which
is what says the two orders agree rather than that they happen to.

**Hashing the names to build a name index.** The replacement index inherited
what the binary search had cost: two million entries of sixty-odd bytes is
120 MB of mangled name hashed, on one thread, to answer sixty-two thousand
questions. Hashing touches nothing shared; filing the answer must be serial. So
the same split the interner makes (175): hash on every core, key the table by
the `u64`, keep the text and compare it rather than trusting the hash.

```
read_and_parse   85 ms -> 74 ms
```

The collision branch is the interesting part of that. Two distinct names
hashing to one `u64` is a 10^-7 event that extracts the wrong archive member,
and no test can reach it by choosing symbol names — that is what a 64-bit hash
is for. So the insert and the lookup both take the hash as a parameter, and the
test supplies a colliding pair directly. It fails when the branch is disabled,
which is the only evidence that it works.

### `dead_strip` 35 -> 31 ms

`Atoms::resolved` was a `Vec<Vec<Option<usize>>>`: the traversal reads it once
per edge — 1.2 million times — and a nested vector makes that two dependent
loads, the inner vector's pointer and then the entry. It also spent sixteen
bytes per entry to carry a value that fits in four, so the array being streamed
was four times larger than the answers in it. One flat `Vec<u32>` with a base
per block, and the probes that fill it run on every core.

```
atoms  6.0 -> 4.1 ms      traverse  21.0 -> 18.6 ms
```

### `emit` did not move

The symbol table computed its string-table size and its highest interning key
in two separate reductions over the same 1.7 million entries — 81 MB read twice
to answer two questions about it. Fusing them is strictly less work and the
change is kept, but the stage did not move:

```
emit   head 61.9 / 65.5 / 68.6 ms      after 61.4 / 67.5 / 68.2 ms
```

That is recorded as *unmeasured*, not as a win. A change that must be faster
and measures flat is a change whose saving was smaller than the thing it was
measured inside, and writing it down as 3 ms because the arithmetic says so is
how a stage table stops describing the linker. `emit` is 82 MB of string table
and 27 MB of entries built into a 194 MB image that is then hashed and written;
one pass over 81 MB is not where it lives.

## 182. Two of the three probes were free, and one of them was the same probe twice

`load_objects` opens with a serial pass asking the session whether it already
holds each input. Serial because the session is not shareable across the reader
threads that follow, which is true — and it is the *map* that is not shareable,
not the probing.

A probe is a `stat` for a content-addressed path and a read plus a BLAKE3 for
one of rustc's. A debug rust-analyzer link has 133 loose objects and 22 MB of
them, hashed on one thread before any reader starts: **7.9 ms**.

Hashing a file touches nothing shared. Probing every input first, on every
core, and handing the answers to the serial pass leaves it with only the map
work.

It also found the second probe. `Session::object` has always carried this
comment:

> `current` has just probed this file, and for a path that is not evidence that
> probe is a hash of its bytes, so asking the content index costs nothing more
> than the lookup.

and then called `InputKey::probe(path)` again, reading and hashing the same
bytes a second time. The comment describes the design; the code did not
implement it. It only fires when the by-path lookup missed — and rustc renames
every object of a recompiled crate, so missing by path is the ordinary case,
not the rare one. That is the second time in this file a doc comment has
described a saving the code below it did not take (177 was the first).

```
read_and_parse   73 ms -> 66 ms
```

## 183. `parse_symbol_map`, and laying the image out to count its sections

Two more of the same species, found by reading the two functions the profile
pointed at rather than by profiling further.

### 18.9 million comparisons to answer 73,095 questions

`parse_symbol_map` resolves each archive symbol to the member defining it. The
archive symbol table addresses members by header offset and the index addresses
them by position, so the two are reconciled through the member's data range:

```rust
if let Some(found) = members.iter().find(|m| m.offset == offset) {
```

Once per symbol. rust-analyzer's largest rlib lists 73,095 symbols across 258
members, which is 18.9 million comparisons — finding 77's shape again, inside a
function that reads like a lookup, and on the single thread that archive is
being indexed on. A sorted index answers it in eight comparisons, and a
one-entry cache answers it in none: the symbol table lists a member's symbols
together, so the previous answer is right 257 times out of 258.

### The layout computed twice to size its own header

`Image::build` laid the whole image out, used the result only to count segments
and sections, chose a header reservation from that count, and laid it out again
for real.

The reservation is a genuine input to the second pass — `__TEXT` starts after
the commands, so every address depends on it. The *section set* is not. It comes
from `output_segment_for` and `output_section_name`, which read an input's own
segment, name and kind and nothing else. Sections cannot appear or disappear
when addresses move.

So the shape is derivable directly from the inputs, and `blinker_layout::output_shape`
does it. Two tests pin the claim rather than restating it: that the predicted
shape equals a real layout's, and that a layout's shape does not move when the
reservation changes by a factor of 64. And the failure mode was already safe —
the emitter compares the commands it actually wrote against the reservation and
returns `CommandsOverflowedReservation` — so a shape that disagreed would fail
the link rather than write load commands over the first section.

It measured 2 ms, not the 15 the removed pass appeared to be worth. Almost all
of what the first pass cost was warming 9,722 contributions into cache for the
second, which then ran that much faster. Removing redundant work is still right;
predicting how much it was worth from how long it took was not.

## 184. The counter that cost more than the stage it was next to

`accounting` had been sitting at 9-13 ms in every profile in this file, and it
is not part of a link. It counts how many contributions kept their address —
a diagnostic, gated behind `count_placement`, which only the relink harness
turns on.

So the 9 ms was never a cost to a user. It was a cost to every *measurement*:
`scripts/relink.py` sets the flag, so every stage table this file quotes was
taken from a link doing 9 ms of work that a real one does not, and the
percentages beside every other stage were computed against that total.

What it was doing is the third occurrence of the same thing in two days:

```rust
.filter(|(path, key)| blinker_cache::InputKey::probe(path).as_ref() == Some(key))
```

341 inputs probed, which for rustc's objects is a read and a BLAKE3 — the same
22 MB hashed at the top of the link (182), hashed again by the reuse plan
before that was fixed, and hashed a third time here. The session proved every
one of them and `key_for` hands the answer back. **9.2 ms -> 1.1 ms.**

The comment directly above it reads:

> a counter that costs a millisecond to compute is a measurement changing the
> thing it measures.

It cost nine.

### Two more of the same kind

**The previous image was copied to be read.** `previous_signature` cloned the
last link's finished binary — 194 MB — out of a cache that is rebuilt from
*this* link's image before being stored again. Nothing reads the old bytes
afterwards, so it can be taken. `layout` 31.0 -> 28.4 ms.

**The FDE map was grown from empty.** Finding where 265,308 FDEs landed runs on
every core and takes 6.2 ms; merging the per-core results into one map took
4.6, because the map started empty and eighteen doublings reinsert everything
already in it at each one. Finding 135, in the serial tail of a parallel pass.
`unwind` 20.0 -> 16.6 ms.

### The shape of the last three days

Eleven changes, and the profiler pointed at the right stage every time and at
the right *line* none of them. Every one was found by reading the function
underneath the number: a sort for a search nobody performs, a scan where a
lookup was meant, a file hashed three times, a map grown from empty, a layout
computed to count its own sections, a probe the comment above it said had
already happened.

None of them is a hard problem. What made them invisible is that each one lives
inside something that had already been optimised — parallelised, memoised,
given a fast hasher — and a stage that has been worked on reads as a stage that
has been dealt with.
