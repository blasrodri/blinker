# Decisions

Choices that shaped the implementation, with the evidence behind them. A
decision recorded here should explain *why* well enough that a later reader can
tell whether the reasoning still holds.

---

## D1: Wrap the `object` crate rather than hand-roll Mach-O parsing

**Date:** M1 kickoff. **Status:** adopted.

### The question

The implementation plan flagged this as M1's biggest scope lever. Spec §14 asks
for a parser where "every offset, count, multiplication, and range must be
checked", with unsafe minimised and fuzzed — which reads like an argument for
writing it ourselves, since we control every access. Against that: a
general-purpose parser is a large amount of well-tested code we would be
reimplementing.

### What the spike found

Parsing real Rust objects with `object` 0.36:

1. **Bounds checking and performance are not the deciding factors.** It parsed
   2,276 real objects (230 MB) without incident at 130–420 MB/s. It is fuzzed
   upstream. Hand-rolling would have to re-earn that.

2. **The apparent blocker dissolved on inspection.** `object`'s *portable*
   `RelocationKind` enum returns `Unknown` for most ARM64 Mach-O relocations —
   23 of the first 63 examined. For a linker that would be fatal: spec §20 is
   explicit that unknown relocations must not be guessed. But the portable enum
   is a convenience layer. `Relocation::flags()` returns
   `RelocationFlags::MachO { r_type, r_pcrel, r_length }` — the raw fields
   straight from the file. No fidelity is lost; we simply must not use the
   portable enum.

3. **Ownership is a conversion problem either way.** `MachOFile64<'data>`
   borrows the input buffer, while M4's cache needs an owned, serialisable
   representation with stable IDs. That conversion step is required no matter
   who does the parsing, so it is not a point of difference.

### Decision

Use `object` for structural parsing, and convert immediately into blinker's own
owned, stable-ID, serialisable representation. Never consume `object`'s portable
abstractions — read raw Mach-O fields and map them ourselves, so an
unrecognised relocation type is an explicit error rather than an `Unknown` we
might quietly tolerate.

### Consequences

- Bounds checking is inherited, not reimplemented. Spec §14's requirement is met
  by a fuzzed upstream parser rather than by our own audit.
- Our unsafe surface for object parsing is zero.
- The representation, stable IDs, and serialisation remain ours — M4's cache is
  unaffected by the choice.
- **The risk we accept:** `object` may return a benign-looking default where we
  need an explicit refusal. Mitigated by reading raw fields everywhere it
  matters, and by treating any unmapped `r_type` as an error.
- If `object` ever becomes an obstacle, the conversion boundary means it can be
  replaced without touching anything downstream of `ParsedObject`.

---

## D2: The ARM64 relocation set to implement is 10 kinds, not 12

**Date:** M1 kickoff. **Status:** adopted.

Spec §20 says to implement only the relocation forms observed in the supported
workload. Census across four real third-party projects — 2,276 objects, ~2.4
million relocations:

| Relocation | ripgrep | fd | hyperfine | tokei |
|---|---:|---:|---:|---:|
| `ARM64_RELOC_UNSIGNED` | 47.4% | 42.3% | 50.1% | 45.9% |
| `ARM64_RELOC_BRANCH26` | 27.4% | 33.7% | 25.6% | 27.6% |
| `ARM64_RELOC_PAGEOFF12` | 8.6% | 6.8% | 7.1% | 8.9% |
| `ARM64_RELOC_PAGE21` | 8.6% | 6.7% | 7.1% | 8.9% |
| `ARM64_RELOC_SUBTRACTOR` | 7.6% | 10.0% | 9.8% | 8.5% |
| `ARM64_RELOC_POINTER_TO_GOT` | 0.1% | 0.3% | 0.1% | 0.1% |
| `ARM64_RELOC_GOT_LOAD_PAGE21` | 0.1% | 0.1% | 0.1% | 0.1% |
| `ARM64_RELOC_GOT_LOAD_PAGEOFF12` | 0.1% | 0.1% | 0.1% | 0.1% |
| `ARM64_RELOC_TLVP_LOAD_PAGE21` | <0.01% | <0.01% | <0.01% | <0.01% |
| `ARM64_RELOC_TLVP_LOAD_PAGEOFF12` | <0.01% | <0.01% | <0.01% | <0.01% |

**The set is identical in all four projects** — only the proportions move. Two
defined kinds never appear: `ARM64_RELOC_ADDEND` and
`ARM64_RELOC_AUTHENTICATED_POINTER` (the latter is arm64e pointer
authentication, which `aarch64-apple-darwin` does not use).

**Decision.** M2 implements these ten. The two unobserved kinds are represented
explicitly as unsupported so they trigger a structured fallback rather than a
wrong answer.

**Implementation note.** Five kinds cover 99.7% of all relocations, but the
long tail is not optional: `TLVP_LOAD_*` appears only a handful of times per
project and is how thread-local storage is addressed. Getting it wrong breaks
TLS in a way no aggregate count would reveal.

---

## D3: Parsing throughput sets the M4 cache target

**Date:** M1 kickoff. **Status:** informational.

The spike measured object parsing at **130–420 MB/s**, and a real link reads
65–200 MB of inputs. Parsing alone is therefore on the order of **0.5–1.5
seconds** per link — against a total delegated link time of 80–140 ms measured
in M0.

That gap is the whole argument for the plan's ordering. Spec §35 asks for ≥80%
of unchanged object parsing to be avoided; these numbers say what that is worth
in absolute terms, and confirm that input parsing — not output generation — is
where a repeated-link win comes from.
