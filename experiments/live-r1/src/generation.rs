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
//! There is none. Every generation is retained until the process exits. That
//! is not an oversight: epoch reclamation is engineering that follows the
//! go/no-go, and a scheme built before anything was measured would be a scheme
//! nobody could justify.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
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
    /// Kept so the code outlives every scope that can reach it. R1 never
    /// drops a generation, so this is retention rather than lifetime
    /// management.
    #[allow(dead_code)]
    slabs: Vec<Slab>,
}

// SAFETY: a generation is immutable once published, and the pointers it holds
// address arena slabs that are never freed while it is reachable.
unsafe impl Send for Generation {}
unsafe impl Sync for Generation {}

impl Generation {
    pub fn implementation(&self, gate: usize) -> Option<*const u8> {
        self.slots.get(gate).copied().filter(|p| !p.is_null())
    }
}

/// The runtime: one current generation, swapped atomically.
pub struct Runtime {
    current: AtomicPtr<Generation>,
    next_id: AtomicU64,
    /// Everything ever published. R1 retains all of it; see the module docs.
    history: Mutex<Vec<Box<Generation>>>,
}

impl Runtime {
    /// Generation 0: the base image's own implementations.
    pub fn new(gates: usize) -> Runtime {
        let zero = Box::new(Generation {
            id: 0,
            parent: 0,
            slots: vec![std::ptr::null(); gates].into_boxed_slice(),
            slabs: Vec::new(),
        });
        let pointer = Box::into_raw(zero);
        // SAFETY: freshly leaked, and re-boxed into `history` so it is owned
        // exactly once.
        let owned = unsafe { Box::from_raw(pointer) };
        Runtime {
            current: AtomicPtr::new(pointer),
            next_id: AtomicU64::new(1),
            history: Mutex::new(vec![owned]),
        }
    }

    /// Build a candidate. Nothing is visible until [`Runtime::publish`].
    pub fn candidate(&self, slots: Vec<*const u8>, slabs: Vec<Slab>) -> Box<Generation> {
        let parent = self.enter().id;
        Box::new(Generation {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            parent,
            slots: slots.into_boxed_slice(),
            slabs,
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
    body(runtime.enter())
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
