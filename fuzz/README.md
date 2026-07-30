# Fuzzing

Coverage-guided fuzzing for blinker's parsers. Requires nightly and
`cargo-fuzz`:

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run parse_macho
```

**Seed the corpus with real object files.** Random bytes are rejected at the
Mach-O magic number and exercise essentially nothing; the interesting states
are reached by mutating structurally valid input:

```bash
mkdir -p fuzz/corpus/parse_macho
find /path/to/some/rust/project/target -name '*.o' \
  | head -200 | xargs -I{} cp {} fuzz/corpus/parse_macho/
```

## Relationship to the test suite

`crates/macho/tests/robustness.rs` runs the *same* entry point on stable, with
deterministic mutations, as part of the normal gate. That catches the shallow
cases on every run without nightly. This directory is for longer sessions that
go deeper.

The split is deliberate: a robustness regression should fail in the gate, not
only in a fuzzing session nobody has run recently.

## Findings

- **Out-of-range section index** — Mach-O numbers sections from 1, and a
  corrupted `n_sect` naming a nonexistent section was accepted and stored as a
  dangling ID. The parse "succeeded" but produced a structure whose IDs could
  not be dereferenced. Fixed by range-checking every index against its table;
  regression pinned in `parse.rs`.
