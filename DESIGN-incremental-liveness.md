# Incremental liveness

The largest single item in a debug edit relink: `dead_strip` is 15.0 ms of 65.5,
and `liveness` is 10.8 of that. It recomputes global reachability over 3,565
contributions to discover that **3 of 27,803 addresses moved**.

This is the design. It is written down because the failure mode is silent —
stripping an atom that is still reachable produces a binary that links, runs,
and crashes somewhere else later — so it should be implemented deliberately
rather than incrementally guessed at.

## What exists today

`reachability::plan` runs three phases (`StripTimings`):

- `Atoms::build` — splits each section into atoms; boundaries are memoised per
  parse in the session already (finding 127). 4.7 ms at debug scale.
- `liveness` — grouping (1.3 ms) then traversal (9.0 ms) from roots.
- `Strip::build` — turns the live set into per-section piece lists. 0.2 ms.

The traversal marks atoms in a `LiveSet` bitset (finding 117), following
`target_atom` for each relocation inside each live atom.

## The invariant to preserve

An atom is live iff it is a root, or some live atom refers to it. Recomputing
from scratch gives the least fixed point. Any incremental scheme must produce
*the same set*, not a superset — a superset is not "safe", it silently disables
dead-stripping and grows the binary, and finding 98 showed that unretained
growth moves contributions and breaks the placement invariant.

## The shape

Hold, in the session, keyed by nothing (it is whole-link state):

    live: LiveSet                      the previous answer
    incoming: Vec<u32>                 for each atom, how many *live* atoms refer to it
    edges_of: FastMap<u32, Vec<Edge>>  outgoing edges, per object

`edges_of` is per object and a pure function of that object plus the atom
numbering. The numbering changes whenever any object's atom count changes, which
an edit routinely does — so either the numbering must be made stable per object
(atom ids as `(object, index-within-object)` rather than a global index), or the
edge lists must be rebuilt. **Making atom identity per-object is the enabling
change** and should come first; it is also what lets `edges_of` be memoised
beside the boundaries in finding 127's memo.

## The update

Given the set of changed objects C (2 of 80 in an ordinary edit):

1. **Remove.** For every atom `a` of an object in C, for every outgoing edge
   `a -> b`: if `a` was live, decrement `incoming[b]`.
2. **Rebuild** the atoms and edges of the objects in C.
3. **Add.** For every atom `a` of an object in C that is a root, or whose
   `incoming` is non-zero, mark live and push to the worklist. Propagate as
   today, incrementing `incoming` as edges are followed.
4. **Collect.** Atoms whose `incoming` dropped to zero and are not roots are
   *candidates* for death. They are not necessarily dead: a cycle can keep its
   own refcount up with nothing outside referring to it.

Step 4 is where a naive refcount is wrong, and it is the whole difficulty.

## Handling cycles

Refcounting cannot free cycles. Two options, in order of preference:

**(a) Bounded re-derivation.** Collect the candidate set S (atoms whose incoming
hit zero). Clear liveness for S and everything reachable *only* from S, then
re-propagate from the roots restricted to that region. Correct, and proportional
to the affected region rather than the program — which for a body edit is
approximately nothing.

**(b) Verify by full recomputation, off the critical path.** Do (a), and on a
debug/`--blinker-verify` build also run the full traversal and assert set
equality. This is how the invariant stays honest: the counters in finding 105's
spirit, but for correctness rather than performance.

Ship (a) with (b) available, and run (b) in CI on every fixture.

## Test obligations

The existing suite has `dead_strips.rs` and `reachability.rs`. Add, at minimum:

- an edit that makes a previously-live function unreachable — it must be
  stripped, and the incremental result must equal the cold result byte for byte
- an edit that makes a previously-dead function reachable
- an edit inside a cycle of mutually-recursive functions that nothing else
  reaches — the whole cycle must die
- a weak symbol whose definition moves between objects
- the equality property: for every fixture, incremental liveness == cold
  liveness, as sets, not as sizes

The last one is the only test that would have caught the `__eh_frame`
`SUBTRACTOR` bug in finding 121 by construction rather than by luck.

## Expected value

At debug scale, `dead_strip` 15.0 ms -> ~1 ms. That is the largest single item
in the relink, and it is worth roughly six times what the same change is worth
on the release workload (finding 130) — which is why it moved from last on the
list to first.
