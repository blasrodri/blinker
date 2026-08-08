//! A preallocated `MAP_JIT` arena, and the W^X dance macOS requires.
//!
//! Apple Silicon will not map a page both writable and executable. What it
//! offers instead is `MAP_JIT` plus a *per-thread* toggle:
//! `pthread_jit_write_protect_np(0)` makes every `MAP_JIT` page in the process
//! writable **for the calling thread only**, and `(1)` makes them executable
//! again. Two consequences shape this module:
//!
//! - **One publisher thread.** The toggle is per-thread state, so a second
//!   thread writing while the first has flipped to execute gets a fault. Every
//!   write goes through [`Arena::write`], which flips, writes and flips back
//!   with the toggle held across the whole copy.
//! - **The instruction cache is not coherent with the data cache.** Freshly
//!   written bytes must be flushed with `sys_icache_invalidate` before they are
//!   branched to, or the core executes whatever was in the i-cache before —
//!   which on a reused slab is the *previous generation's code*.
//!
//! Measured beforehand rather than assumed: `MAP_JIT` needs no entitlement and
//! no Hardened Runtime opt-in under a plain ad-hoc signature, which is what
//! blinker's linker already emits. There is no signing work between this and a
//! working base image.
//!
//! The arena is reserved once at startup. Reserving per publication would put
//! an `mmap` on the hot path, and the whole claim being tested is that
//! publication is sub-millisecond.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

unsafe extern "C" {
    fn pthread_jit_write_protect_np(enabled: libc::c_int);
    fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
}

#[derive(Debug)]
pub enum ArenaError {
    Reserve(std::io::Error),
    /// The arena is full. Fails closed: a publication that cannot fit does not
    /// get to overwrite a live generation.
    Exhausted { wanted: usize, left: usize },
}

impl std::fmt::Display for ArenaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArenaError::Reserve(e) => write!(f, "cannot reserve a JIT arena: {e}"),
            ArenaError::Exhausted { wanted, left } => {
                write!(f, "arena exhausted: wanted {wanted} bytes, {left} left")
            }
        }
    }
}

/// One contiguous `MAP_JIT` reservation, handed out as slabs.
pub struct Arena {
    base: *mut u8,
    size: usize,
    /// Bump pointer, for slabs that cannot be satisfied from the free list.
    used: AtomicUsize,
    /// Returned slabs, as `(offset, len)`, newest first.
    ///
    /// A free list rather than a compacting allocator, and the reason is the
    /// same one that makes this whole design work: a slab's address is baked
    /// into a published generation's slot table and into every relocation
    /// inside it. Nothing can move once it is written, so the only reclamation
    /// available is reuse in place.
    ///
    /// First fit, and no coalescing. Patch slabs are a few hundred bytes to a
    /// few kilobytes and successive revisions of one program ask for nearly the
    /// same size, so the list stays short and hits almost always. Coalescing is
    /// what to add when a measurement says the list is growing, and §49 reports
    /// that it is not.
    free: Mutex<Vec<(usize, usize)>>,
}

// SAFETY: the arena hands out disjoint slabs and never aliases them. `base`
// is a fixed mapping for the arena's whole lifetime, and the only interior
// mutability is the bump pointer, which is atomic.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

/// A slab of executable memory, still empty.
pub struct Slab {
    pub ptr: *mut u8,
    pub len: usize,
    /// Offset from the arena base, so it can be handed back.
    pub(crate) offset: usize,
}

impl Arena {
    pub fn reserve(size: usize) -> Result<Arena, ArenaError> {
        let size = size.next_multiple_of(page_size());
        // SAFETY: a fresh anonymous mapping of `size` bytes. `MAP_JIT` is what
        // makes RWX legal on Apple Silicon; without it this returns EINVAL.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(ArenaError::Reserve(std::io::Error::last_os_error()));
        }
        Ok(Arena {
            base: base.cast(),
            size,
            used: AtomicUsize::new(0),
            free: Mutex::new(Vec::new()),
        })
    }

    /// Carve a slab. Alignment is 16, which every AArch64 instruction stream
    /// and constant pool this emits is happy with.
    pub fn slab(&self, len: usize) -> Result<Slab, ArenaError> {
        let aligned = len.next_multiple_of(16);
        // A returned slab first. Whole blocks only: splitting one would need
        // the remainder tracked and eventually coalesced, and a patch closure's
        // size barely moves between revisions, so an exact-or-larger block is
        // almost always waiting.
        {
            let mut free = self.free.lock().expect("not poisoned");
            if let Some(index) = free.iter().position(|(_, len)| *len >= aligned) {
                let (offset, len) = free.swap_remove(index);
                // SAFETY: an offset and length this arena handed out before.
                return Ok(Slab { ptr: unsafe { self.base.add(offset) }, len, offset });
            }
        }
        let start = self.used.fetch_add(aligned, Ordering::Relaxed);
        if start + aligned > self.size {
            // Give the bump pointer back so a failed publication does not
            // permanently consume the arena it could not use.
            self.used.fetch_sub(aligned, Ordering::Relaxed);
            return Err(ArenaError::Exhausted {
                wanted: aligned,
                left: self.size.saturating_sub(start),
            });
        }
        // SAFETY: `start + aligned <= size`, so the slab lies inside the
        // mapping, and `fetch_add` guarantees no other caller got these bytes.
        Ok(Slab {
            ptr: unsafe { self.base.add(start) },
            len: aligned,
            offset: start,
        })
    }

    /// Hand a slab back.
    ///
    /// # Safety
    /// Nothing may execute or hold a pointer into `slab` after this returns.
    /// That is the whole safety obligation of reclamation and it is not one the
    /// arena can check: it does not know which generation owns a slab, which
    /// scopes are live, or whether a rollback might want it again. The caller
    /// that does know is [`crate::generation::Runtime`], and §49 is the
    /// argument it makes.
    pub unsafe fn release(&self, slab: Slab) {
        self.free.lock().expect("not poisoned").push((slab.offset, slab.len));
    }

    /// Bytes sitting on the free list.
    pub fn reclaimable(&self) -> usize {
        self.free.lock().expect("not poisoned").iter().map(|(_, len)| len).sum()
    }

    /// How many blocks the free list holds. A number that keeps climbing is
    /// fragmentation, and is the measurement that would justify coalescing.
    pub fn free_blocks(&self) -> usize {
        self.free.lock().expect("not poisoned").len()
    }

    /// Fill a slab, with the write protection flipped for exactly as long as
    /// the copy takes.
    ///
    /// The i-cache flush is *not* done here: a caller writing code then
    /// applying relocations to it would flush twice and branch to bytes it
    /// then modified. [`Arena::publish`] is the flush, and it is the last
    /// thing to touch a slab before anything can execute it.
    ///
    /// # Safety
    /// `slab` must have come from this arena and must not be executing.
    pub unsafe fn write(&self, slab: &Slab, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= slab.len, "write past the slab");
        unsafe {
            pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), slab.ptr.add(offset), bytes.len());
            pthread_jit_write_protect_np(1);
        }
    }

    /// Make a slab executable: flush the instruction cache over it.
    ///
    /// Separate from `write` because relocation happens in between. Skipping
    /// this does not fail loudly — the core runs whatever the i-cache already
    /// held for those addresses, which on a fresh slab is usually zeroes and
    /// on a reused one is the previous generation.
    ///
    /// # Safety
    /// `slab` must have come from this arena and hold complete, relocated code.
    pub unsafe fn publish(&self, slab: &Slab) {
        unsafe { sys_icache_invalidate(slab.ptr.cast(), slab.len) }
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub fn capacity(&self) -> usize {
        self.size
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: `base`/`size` are exactly what `mmap` returned.
        unsafe { libc::munmap(self.base.cast(), self.size) };
    }
}

fn page_size() -> usize {
    // SAFETY: a plain sysconf query with no arguments to get wrong.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mov x0, #<value>; ret`, so a test can tell two generations apart by
    /// what the code returns rather than by inspecting bytes.
    pub fn returns(value: u16) -> [u8; 8] {
        let mov = 0xd280_0000u32 | ((value as u32) << 5);
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&mov.to_le_bytes());
        out[4..].copy_from_slice(&0xd65f_03c0u32.to_le_bytes()); // ret
        out
    }

    #[test]
    fn code_written_into_the_arena_runs() {
        let arena = Arena::reserve(64 * 1024).expect("reserve");
        let slab = arena.slab(8).expect("slab");
        unsafe {
            arena.write(&slab, 0, &returns(42));
            arena.publish(&slab);
        }
        // SAFETY: the slab holds a complete `mov`/`ret` for the C ABI, and
        // has been i-cache flushed.
        let f: extern "C" fn() -> u64 = unsafe { std::mem::transmute(slab.ptr) };
        assert_eq!(f(), 42);
    }

    /// Two slabs must not overlap, or one generation's code silently becomes
    /// another's.
    #[test]
    fn slabs_are_disjoint() {
        let arena = Arena::reserve(64 * 1024).expect("reserve");
        let first = arena.slab(8).expect("slab");
        let second = arena.slab(8).expect("slab");
        unsafe {
            arena.write(&first, 0, &returns(1));
            arena.write(&second, 0, &returns(2));
            arena.publish(&first);
            arena.publish(&second);
        }
        let a: extern "C" fn() -> u64 = unsafe { std::mem::transmute(first.ptr) };
        let b: extern "C" fn() -> u64 = unsafe { std::mem::transmute(second.ptr) };
        assert_eq!((a(), b()), (1, 2));
    }

    /// Exhaustion fails closed rather than handing back a slab that runs off
    /// the end of the mapping.
    #[test]
    fn an_exhausted_arena_refuses_rather_than_overruns() {
        let arena = Arena::reserve(1).expect("reserve"); // rounds to one page
        let capacity = arena.capacity();
        assert!(arena.slab(capacity).is_ok());
        match arena.slab(16) {
            Err(ArenaError::Exhausted { .. }) => {}
            other => panic!("expected exhaustion, got {other:?}", other = other.is_ok()),
        }
        // And the failed request did not consume the arena.
        assert_eq!(arena.used(), capacity);
    }
}
