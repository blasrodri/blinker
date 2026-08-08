//! Immutable generations, stable gates, and one atomic swap.
//!
//! The property this exists to hold
//! --------------------------------
//!
//! ```text
//!   request A starts on G1
//!         │
//!         │        publish G2
//!         │           ↓
//!         │      request B starts on G2
//!         │
//!   request A finishes entirely on G1
//! ```
//!
//! One operation must never observe a mixture of old and new code. That is why
//! functions are not published one at a time: a generation is built complete,
//! validated, and then installed with a single store. A scope captures the
//! generation pointer once on entry and every gate it reaches uses *that*
//! table, so a publication landing halfway through a request is invisible to
//! it.
//!
//! Gates
//! -----
//!
//! A gate is a fixed index assigned at base-image build time. Calling through
//! gate 381 means "load slot 381 of the generation this scope captured, and
//! call it". The indirection is deliberate: a function pointer, callback or
//! vtable entry that may outlive a generation must point at something
//! permanent, and the gate is that permanent thing.
//!
//! R1 implements the gate in Rust rather than as the three-instruction
//! `adrp/ldr/br` sequence the final design wants. Measuring the macro overhead
//! comes before optimizing the sequence — an obvious optimization trusted
//! without a control is how this project has generated most of its findings.
//!
//! Reclamation
//! -----------
//!
//! §49. Two classes, with different arguments, and the difference is the point.
//!
//! **A discarded candidate** was never published. `Runtime::current` never
//! pointed at it, so no scope can be holding it and no rollback can reach it.
//! Its slabs go back immediately, and the argument is one sentence.
//!
//! **A retired generation** is harder, and the design pays for it in two ways.
//! A scope that entered while it was current may still be running, so it is
//! reference-counted: a scope registers on the generation it captured and the
//! last one out reclaims. And `rollback_code` can make *any* generation in
//! history current again, so "retired" is not a property that can be inferred —
//! it has to be a stated retention policy. Generations outside the window are
//! retired and reclaimed; a rollback to one of them is **refused**, not
//! attempted.
//!
//! What is never freed is the `Generation` struct itself. It is a slot table
//! and two counters — hundreds of bytes against a slab's kilobytes — and
//! keeping it forever is what makes the reference count safe to touch without
//! a hazard pointer or an epoch: a thread that loads a stale `current` pointer
//! reads a live object with a valid counter, discovers it is stale, and lets
//! go.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::arena::Slab;

/// A published, immutable set of implementations.
///
/// `slots` is indexed by gate. It is boxed and never mutated after
/// publication — a generation that could be edited in place would reintroduce
/// exactly the tearing the design exists to prevent.
pub struct Generation {
    pub id: u64,
    pub parent: u64,
    slots: Box<[*const u8]>,
    /// The arena memory this generation's code lives in.
    ///
    /// Taken — not dropped — when the generation is reclaimed, so that the
    /// `Generation` itself survives as an inert husk that a racing reader can
    /// safely inspect and reject.
    slabs: Mutex<Vec<Slab>>,
    /// Live scopes holding this generation.
    scopes: AtomicUsize,
    /// Outside the retention window: eligible for reclamation once the last
    /// scope leaves, and no longer a legal rollback target.
    retired: AtomicBool,
    /// Its slabs have gone back to the arena. Terminal.
    reclaimed: AtomicBool,
}

// SAFETY: a generation is immutable once published, and the pointers it holds
// address arena slabs that are never freed while it is reachable.
unsafe impl Send for Generation {}
unsafe impl Sync for Generation {}

impl Generation {
    pub fn implementation(&self, gate: usize) -> Option<*const u8> {
        // A reclaimed generation answers nothing. It cannot be reached on the
        // forward path — nothing points at it — so this is the belt to the
        // retention policy's braces, and it fails closed.
        if self.reclaimed.load(Ordering::Acquire) {
            return None;
        }
        self.slots.get(gate).copied().filter(|p| !p.is_null())
    }

    pub fn is_reclaimed(&self) -> bool {
        self.reclaimed.load(Ordering::Acquire)
    }

    /// Give the slabs back, if nothing can be holding them.
    ///
    /// Returns the bytes released. Idempotent, and returns 0 for a generation
    /// that is still current, still held, or already reclaimed.
    fn reclaim(&self, arena: &crate::arena::Arena) -> usize {
        if !self.retired.load(Ordering::Acquire) {
            return 0;
        }
        if self.scopes.load(Ordering::Acquire) != 0 {
            return 0;
        }
        // Claim the right to reclaim exactly once. Two threads leaving the last
        // two scopes at the same moment would otherwise both try.
        if self.reclaimed.swap(true, Ordering::AcqRel) {
            return 0;
        }
        let taken = std::mem::take(&mut *self.slabs.lock().expect("slabs"));
        let mut released = 0;
        for slab in taken {
            released += slab.len;
            // SAFETY: `retired` says the retention policy has ruled out any
            // future rollback to this generation, and `scopes == 0` says no
            // scope captured it and is still running. Together those are every
            // way a pointer into these slabs could still be reachable.
            unsafe { arena.release(slab) };
        }
        released
    }
}

/// The runtime: one current generation, swapped atomically.
pub struct Runtime {
    current: AtomicPtr<Generation>,
    next_id: AtomicU64,
    /// Every `Generation` ever published, retained forever — see the module
    /// docs. Their *slabs* are not: those are reclaimed under the policy below.
    history: Mutex<Vec<Box<Generation>>>,
    /// How many published generations stay rollback-able, counting the current
    /// one. `usize::MAX` retains everything, which is what R1 did.
    ///
    /// A number rather than a heuristic, because it is the whole safety
    /// argument for retiring a *published* generation: nothing can infer
    /// whether a rollback is coming, so the depth available has to be declared
    /// and a rollback past it has to be refused.
    retention: usize,
    /// Bytes handed back to the arena, cumulative.
    reclaimed: AtomicUsize,
}

impl Runtime {
    /// Generation 0: the base image's own implementations.
    /// Retain every generation, as R1 did.
    pub fn new(gates: usize) -> Runtime {
        Runtime::with_retention(gates, usize::MAX)
    }

    /// Keep `retention` generations rollback-able and reclaim the rest.
    pub fn with_retention(gates: usize, retention: usize) -> Runtime {
        let zero = Box::new(Generation {
            id: 0,
            parent: 0,
            slots: vec![std::ptr::null(); gates].into_boxed_slice(),
            slabs: Mutex::new(Vec::new()),
            scopes: AtomicUsize::new(0),
            retired: AtomicBool::new(false),
            reclaimed: AtomicBool::new(false),
        });
        let pointer = Box::into_raw(zero);
        // SAFETY: freshly leaked, and re-boxed into `history` so it is owned
        // exactly once.
        let owned = unsafe { Box::from_raw(pointer) };
        Runtime {
            current: AtomicPtr::new(pointer),
            next_id: AtomicU64::new(1),
            history: Mutex::new(vec![owned]),
            retention,
            reclaimed: AtomicUsize::new(0),
        }
    }

    /// Bytes returned to the arena so far.
    pub fn reclaimed_bytes(&self) -> usize {
        self.reclaimed.load(Ordering::Relaxed)
    }

    /// Capture the current generation and register as a live reader.
    ///
    /// The retry is what makes reference counting safe here. Between loading
    /// the pointer and incrementing its counter, a publication can land — so
    /// the pointer is re-read afterwards, and a mismatch means this thread
    /// incremented a generation that is no longer current and has to let go and
    /// try again. It never touches freed memory doing so, because the
    /// `Generation` struct outlives the process.
    fn acquire(&self) -> &Generation {
        loop {
            let pointer = self.current.load(Ordering::Acquire);
            // SAFETY: every generation ever published is retained in `history`
            // for the runtime's lifetime, so a loaded pointer is always live.
            let generation: &Generation = unsafe { &*pointer };
            generation.scopes.fetch_add(1, Ordering::Acquire);
            if self.current.load(Ordering::Acquire) == pointer {
                return generation;
            }
            generation.scopes.fetch_sub(1, Ordering::Release);
        }
    }

    /// The last scope out of a retired generation gives its slabs back.
    fn leave(&self, generation: &Generation, arena: Option<&crate::arena::Arena>) {
        if generation.scopes.fetch_sub(1, Ordering::AcqRel) == 1 {
            if let Some(arena) = arena {
                let released = generation.reclaim(arena);
                self.reclaimed.fetch_add(released, Ordering::Relaxed);
            }
        }
    }

    /// Retire everything outside the retention window and reclaim what it can.
    ///
    /// Called after a publication. A generation still held by a live scope is
    /// marked retired and left alone; the scope that eventually leaves it does
    /// the reclaiming.
    fn retire(&self, arena: &crate::arena::Arena) {
        let history = self.history.lock().expect("history");
        if self.retention == usize::MAX || history.len() <= self.retention {
            return;
        }
        let current = self.current.load(Ordering::Acquire);
        let cutoff = history.len() - self.retention;
        for generation in history.iter().take(cutoff) {
            if std::ptr::eq(&**generation, current) {
                continue;
            }
            generation.retired.store(true, Ordering::Release);
            let released = generation.reclaim(arena);
            self.reclaimed.fetch_add(released, Ordering::Relaxed);
        }
    }

    /// Publish, then retire what the policy no longer protects.
    pub fn publish_and_retire(&self, candidate: Box<Generation>, arena: &crate::arena::Arena) -> u64 {
        let id = self.publish(candidate);
        self.retire(arena);
        id
    }

    /// Give a never-published candidate's memory straight back.
    ///
    /// The easy half of §49, and the one that matters most in practice: an
    /// agent rejects far more candidates than it commits. `Runtime::current`
    /// never pointed at this, so no scope holds it and no rollback can reach
    /// it — there is nothing to reason about.
    pub fn discard(&self, candidate: Box<Generation>, arena: &crate::arena::Arena) -> usize {
        candidate.retired.store(true, Ordering::Release);
        let released = candidate.reclaim(arena);
        self.reclaimed.fetch_add(released, Ordering::Relaxed);
        released
    }

    /// Build a candidate. Nothing is visible until [`Runtime::publish`].
    pub fn candidate(&self, slots: Vec<*const u8>, slabs: Vec<Slab>) -> Box<Generation> {
        let parent = self.enter().id;
        Box::new(Generation {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            parent,
            slots: slots.into_boxed_slice(),
            slabs: Mutex::new(slabs),
            scopes: AtomicUsize::new(0),
            retired: AtomicBool::new(false),
            reclaimed: AtomicBool::new(false),
        })
    }

    /// Install a candidate. One store, and it is the entire publication.
    ///
    /// `Release` pairs with the `Acquire` in [`Runtime::enter`]: every write
    /// that built the generation — the code bytes, the relocations, the slot
    /// table — happens-before any scope that observes the new pointer. Without
    /// the ordering a scope could see the pointer and not the code.
    pub fn publish(&self, candidate: Box<Generation>) -> u64 {
        let id = candidate.id;
        let pointer: *mut Generation = &*candidate as *const Generation as *mut Generation;
        self.history.lock().expect("history").push(candidate);
        self.current.store(pointer, Ordering::Release);
        id
    }

    /// What a scope captures on entry, and keeps for its whole life.
    pub fn enter(&self) -> &Generation {
        let pointer = self.current.load(Ordering::Acquire);
        // SAFETY: every generation ever published is retained in `history`
        // for the runtime's lifetime, so a loaded pointer is always live.
        unsafe { &*pointer }
    }

    /// Roll back to a previously published generation.
    ///
    /// Code rollback, and nothing more: whatever the retired generation did to
    /// globals, files or sockets is still done. Named `rollback_code` so the
    /// call site cannot read as a promise about state.
    pub fn rollback_code(&self, id: u64) -> bool {
        let history = self.history.lock().expect("history");
        let Some(target) = history.iter().find(|g| g.id == id) else {
            return false;
        };
        // Refused, not attempted. A reclaimed generation's slabs have been
        // handed back and may already hold a later revision's code; installing
        // its slot table would branch into whatever is there now. This is the
        // one place the retention window has teeth, and it fails closed.
        if target.is_reclaimed() || target.retired.load(Ordering::Acquire) {
            return false;
        }
        let pointer: *mut Generation = &**target as *const Generation as *mut Generation;
        self.current.store(pointer, Ordering::Release);
        true
    }

    pub fn generations(&self) -> usize {
        self.history.lock().expect("history").len()
    }
}

/// Run `body` against one generation, captured once.
///
/// This is the unit of consistency. Reloading the current generation inside
/// the body — for each gate, say — is what would let one request observe half
/// of a publication.
pub fn scope<T>(runtime: &Runtime, body: impl FnOnce(&Generation) -> T) -> T {
    scope_in(runtime, None, body)
}

/// The same, registering as a live reader so reclamation can wait for it.
///
/// `arena` is what makes the last scope out able to give the memory back. It is
/// optional because a caller with no reclamation to do should not have to
/// invent an arena to say so.
pub fn scope_in<T>(
    runtime: &Runtime,
    arena: Option<&crate::arena::Arena>,
    body: impl FnOnce(&Generation) -> T,
) -> T {
    let generation = runtime.acquire();
    let out = body(generation);
    runtime.leave(generation, arena);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::Arena;
    use std::sync::mpsc;
    use std::sync::Arc;

    fn returns(value: u16) -> [u8; 8] {
        let mov = 0xd280_0000u32 | ((value as u32) << 5);
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&mov.to_le_bytes());
        out[4..].copy_from_slice(&0xd65f_03c0u32.to_le_bytes());
        out
    }

    fn generation_returning(arena: &Arena, runtime: &Runtime, value: u16) -> Box<Generation> {
        let slab = arena.slab(8).expect("slab");
        unsafe {
            arena.write(&slab, 0, &returns(value));
            arena.publish(&slab);
        }
        let pointer = slab.ptr as *const u8;
        runtime.candidate(vec![pointer], vec![slab])
    }

    fn call(generation: &Generation, gate: usize) -> u64 {
        let pointer = generation.implementation(gate).expect("a slot");
        // SAFETY: the slot holds a complete `mov`/`ret` for the C ABI.
        let f: extern "C" fn() -> u64 = unsafe { std::mem::transmute(pointer) };
        f()
    }

    #[test]
    fn a_published_generation_is_what_new_scopes_see() {
        let arena = Arena::reserve(64 * 1024).expect("arena");
        let runtime = Runtime::new(1);
        let first = generation_returning(&arena, &runtime, 1);
        runtime.publish(first);
        assert_eq!(scope(&runtime, |g| call(g, 0)), 1);

        let second = generation_returning(&arena, &runtime, 2);
        runtime.publish(second);
        assert_eq!(scope(&runtime, |g| call(g, 0)), 2);
    }

    /// The architectural property, as a test rather than a claim.
    ///
    /// A scope that entered before a publication must finish on the generation
    /// it entered with — not observe the new one partway through.
    #[test]
    fn a_scope_finishes_on_the_generation_it_entered() {
        let arena = Arc::new(Arena::reserve(64 * 1024).expect("arena"));
        let runtime = Arc::new(Runtime::new(1));
        runtime.publish(generation_returning(&arena, &runtime, 1));

        let (entered, has_entered) = mpsc::channel();
        let (published, was_published) = mpsc::channel();

        let worker = {
            let runtime = Arc::clone(&runtime);
            std::thread::spawn(move || {
                scope(&runtime, |generation| {
                    let before = call(generation, 0);
                    entered.send(()).expect("signal");
                    // Hold the scope open across the publication.
                    was_published.recv().expect("wait");
                    let after = call(generation, 0);
                    (before, after)
                })
            })
        };

        has_entered.recv().expect("the worker entered");
        runtime.publish(generation_returning(&arena, &runtime, 2));
        published.send(()).expect("signal");

        let (before, after) = worker.join().expect("worker");
        assert_eq!(
            (before, after),
            (1, 1),
            "a scope observed a publication that landed after it entered"
        );
        // And a scope entered afterwards sees the new one.
        assert_eq!(scope(&runtime, |g| call(g, 0)), 2);
    }

    #[test]
    fn rollback_restores_a_previous_generation() {
        let arena = Arena::reserve(64 * 1024).expect("arena");
        let runtime = Runtime::new(1);
        let first = runtime.publish(generation_returning(&arena, &runtime, 7));
        runtime.publish(generation_returning(&arena, &runtime, 9));
        assert_eq!(scope(&runtime, |g| call(g, 0)), 9);
        assert!(runtime.rollback_code(first));
        assert_eq!(scope(&runtime, |g| call(g, 0)), 7);
        assert!(!runtime.rollback_code(4242));
    }

    /// Many readers across a publication, so the store/load ordering is
    /// exercised rather than merely written down. Every reader must see one
    /// generation's answer for its whole scope, never a torn mixture.
    #[test]
    fn concurrent_scopes_never_observe_a_mixture() {
        let arena = Arc::new(Arena::reserve(1024 * 1024).expect("arena"));
        let runtime = Arc::new(Runtime::new(1));
        runtime.publish(generation_returning(&arena, &runtime, 1));

        let readers: Vec<_> = (0..8)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                std::thread::spawn(move || {
                    for _ in 0..2_000 {
                        let observed = scope(&runtime, |generation| {
                            let first = call(generation, 0);
                            std::hint::spin_loop();
                            (first, call(generation, 0))
                        });
                        assert_eq!(
                            observed.0, observed.1,
                            "one scope saw two different implementations"
                        );
                    }
                })
            })
            .collect();

        for value in 2..40u16 {
            runtime.publish(generation_returning(&arena, &runtime, value));
        }
        for reader in readers {
            reader.join().expect("reader");
        }
    }
}

#[cfg(test)]
mod reclamation {
    use super::*;
    use crate::arena::Arena;

    fn returns(value: u16) -> [u8; 8] {
        let mov = 0xd280_0000u32 | ((value as u32) << 5);
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&mov.to_le_bytes());
        out[4..].copy_from_slice(&0xd65f_03c0u32.to_le_bytes());
        out
    }

    fn generation(arena: &Arena, runtime: &Runtime, value: u16) -> Box<Generation> {
        let slab = arena.slab(64).expect("slab");
        // SAFETY: a fresh slab, nothing executing it.
        unsafe {
            arena.write(&slab, 0, &returns(value));
            arena.publish(&slab);
        }
        let entry = slab.ptr as *const u8;
        runtime.candidate(vec![entry], vec![slab])
    }

    #[test]
    fn a_discarded_candidate_gives_its_memory_straight_back() {
        let arena = Arena::reserve(64 * 1024).expect("arena");
        let runtime = Runtime::new(1);
        let before = arena.used();
        for value in 0..32 {
            let candidate = generation(&arena, &runtime, value);
            runtime.discard(candidate, &arena);
        }
        // Thirty-two candidates, one slab's worth of arena. Without a free list
        // this is where the leak lived: an agent rejects far more than it
        // commits, and every rejection used to cost a slab forever.
        assert_eq!(arena.used() - before, 64);
        assert_eq!(arena.reclaimable(), 64);
    }

    #[test]
    fn a_retired_generation_is_reclaimed_and_cannot_be_rolled_back_to() {
        let arena = Arena::reserve(64 * 1024).expect("arena");
        // Two rollback-able generations: the current one and its predecessor.
        let runtime = Runtime::with_retention(1, 2);
        let mut ids = Vec::new();
        for value in 1..=4u16 {
            let candidate = generation(&arena, &runtime, value);
            ids.push(runtime.publish_and_retire(candidate, &arena));
        }
        // The newest still answers.
        assert!(runtime.rollback_code(ids[3]));
        assert_eq!(scope(&runtime, |g| call(g)), 4);
        // Its predecessor is inside the window.
        assert!(runtime.rollback_code(ids[2]));
        assert_eq!(scope(&runtime, |g| call(g)), 3);
        // Anything older is refused rather than attempted, which is the whole
        // teeth of the retention policy: those slabs may already hold a later
        // revision's code.
        assert!(!runtime.rollback_code(ids[0]));
        assert!(runtime.reclaimed_bytes() > 0);
    }

    fn call(generation: &Generation) -> u64 {
        let pointer = generation.implementation(0).expect("an implementation");
        // SAFETY: a `mov`/`ret` this test wrote and i-cache flushed.
        let f: extern "C" fn() -> u64 = unsafe { std::mem::transmute(pointer) };
        f()
    }

    #[test]
    fn a_live_scope_keeps_its_generation_from_being_reclaimed() {
        let arena = Arena::reserve(64 * 1024).expect("arena");
        let runtime = Runtime::with_retention(1, 1);
        let first = generation(&arena, &runtime, 7);
        runtime.publish_and_retire(first, &arena);

        // A scope entered on generation 1, still running, while three more are
        // published on top of it. Retirement must not take the memory out from
        // under it — this is the property the whole reference count exists for.
        let observed = scope_in(&runtime, Some(&arena), |held| {
            for value in 10..13u16 {
                let candidate = generation(&arena, &runtime, value);
                runtime.publish_and_retire(candidate, &arena);
            }
            call(held)
        });
        assert_eq!(observed, 7);
        // And once it leaves, the memory does come back.
        assert!(runtime.reclaimed_bytes() > 0);
    }

    #[test]
    fn released_slabs_are_handed_out_again() {
        let arena = Arena::reserve(64 * 1024).expect("arena");
        let runtime = Runtime::new(1);
        let first = generation(&arena, &runtime, 1);
        let address = first.implementation(0).expect("slot");
        runtime.discard(first, &arena);
        let second = generation(&arena, &runtime, 2);
        // The same bytes, reused — and answering with the *new* code, which is
        // what the i-cache flush in `Arena::publish` is for. Reuse without it
        // would run the previous generation from the instruction cache.
        assert_eq!(second.implementation(0), Some(address));
        assert_eq!(call(&second), 2);
    }
}
