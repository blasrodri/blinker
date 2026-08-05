//! Running a per-object pass on every core.
//!
//! # Why this exists
//!
//! Reading and parsing the inputs has been parallel for a long time; nothing
//! after it was. On a debug rust-analyzer link that left roughly 300 ms of the
//! 830 ms link — surveying relocations, placing symbols, building the unwind
//! table, applying relocations — running on one core of fifteen, over 5,637
//! objects that do not read each other.
//!
//! # Why it is chunked and not work-stolen
//!
//! `load_objects` hands out inputs through an atomic cursor, which balances
//! itself but gives each worker an arbitrary *set* of inputs. That is fine
//! there because each result is filed by its own index. It is not fine for a
//! pass whose results are concatenated, because the order they are
//! concatenated in reaches the output — which GOT slot a symbol gets, and
//! therefore every address after it.
//!
//! So the work is cut into contiguous chunks, the cursor hands out *chunk*
//! indices, and the results are merged in chunk order. More chunks than
//! threads keeps it balanced; the order is fixed before any thread starts.
//!
//! A link whose output depends on thread scheduling is not a link, and the
//! byte comparison against a cold link is what says it does not.
//!
//! # Why the threads are not created here
//!
//! They were, once per call, and a link makes 29 of these calls: 413 threads
//! created and joined to do work that is often microseconds. Creating and
//! joining fifteen scoped threads on this machine costs 0.11–0.15 ms whatever
//! they do, so the smallest calls were paying more to arrange the parallelism
//! than the parallelism saved — an extraction round of 415 names took 0.17 ms,
//! essentially all of it spawn.
//!
//! So the threads are made once and parked, and a call wakes them. Nothing
//! about the work changes: the same chunk bounds, the same atomic cursor
//! handing out the same chunk indices, the same merge in chunk order.
//!
//! # Why they park rather than spin
//!
//! The obvious next step — have the workers spin on the epoch instead of
//! parking, the way a trading system would — is measurably *worse* here, and
//! it is worth saying why since it is the opposite of what the technique
//! promises. Per call, on a machine with other work on it:
//!
//! ```text
//! spawn per call                   0.111 ms
//! parked pool                      0.042 ms
//! spinning pool                    0.080 ms
//! ```
//!
//! and with the 400 µs of serial work a real link puts between two calls, the
//! spinning pool degrades to 0.18–0.23 ms while the parked one holds at 0.05.
//! A spinning worker is not free to the rest of the machine: fourteen of them
//! compete for cores with the *submitter*, which is running a chunk itself,
//! and with whatever else the developer is doing. Busy-waiting buys latency
//! with a core, and a linker on a shared desktop cannot afford the core.

use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

/// How many chunks per thread. Four is enough to absorb an object that is ten
/// times the size of its neighbours without making the merge itself the cost.
const CHUNKS_PER_THREAD: usize = 4;

/// How many threads a pass may use, asked once rather than per call.
fn cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

thread_local! {
    /// Whether this thread is already inside a pool job.
    ///
    /// Set on every participant, the submitting thread included, because the
    /// submitter runs a chunk itself. A `map_chunks` reached from inside
    /// `work` would otherwise submit to a pool that is already busy and wait
    /// for its own completion; it creates threads instead. Nesting is not
    /// known to happen, which is exactly why it is handled here rather than
    /// asserted against — the cost of being wrong is a hung link.
    static IN_JOB: Cell<bool> = const { Cell::new(false) };
}

/// A job, with its lifetimes erased so it can cross into the parked workers.
///
/// Sound only because of the barrier in [`Pool::run`]: the submitter does not
/// return until every worker has finished with this pointer, so the closure it
/// points at outlives every use of it.
#[derive(Clone, Copy)]
struct Job {
    body: *const (),
    call: unsafe fn(*const ()),
}

// SAFETY: the pointer is used only between the submitter publishing it and the
// submitter observing `running == 0`, and the closure it names is `Sync`.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

struct Dispatch {
    /// Bumped once per submitted job. A worker compares it against the last
    /// one it ran to tell a real wake-up from a spurious one.
    epoch: u64,
    job: Option<Job>,
    /// Workers that have not yet finished the current job.
    running: usize,
}

struct Pool {
    /// Held for the length of one job.
    ///
    /// The workers share a single job slot and a single outstanding count, so
    /// two submitters at once corrupt each other: the second overwrites
    /// `running` with a fresh count while the first's workers are still
    /// decrementing it, and a worker that never woke for the first epoch —
    /// because the second had already replaced it — decrements once where two
    /// jobs' worth was charged. `running` then never reaches zero and both
    /// submitters wait forever.
    ///
    /// A linker process links one program at a time, so this is uncontended in
    /// the product; it is `cargo test`, which runs test functions on concurrent
    /// threads, that submits two at once. Taken with `try_lock` rather than
    /// waited on, because making the second link *queue* behind the first would
    /// serialise two links that could have run side by side — it spawns its own
    /// threads instead, which is what every call did before this pool existed.
    submit: Mutex<()>,
    state: Mutex<Dispatch>,
    /// Raised to start a job.
    wake: Condvar,
    /// Raised by the last worker out.
    idle: Condvar,
    /// Parked threads, which is one fewer than the cores used: the submitter
    /// runs a chunk too rather than waiting on work it could be doing.
    workers: usize,
}

impl Pool {
    fn get() -> Option<&'static Pool> {
        static POOL: OnceLock<Option<&'static Pool>> = OnceLock::new();
        *POOL.get_or_init(|| {
            let workers = cores().checked_sub(1).filter(|n| *n > 0)?;
            // Leaked deliberately: the pool lives as long as the process, and
            // a resident worker links thousands of times through it.
            let pool: &'static Pool = Box::leak(Box::new(Pool {
                submit: Mutex::new(()),
                state: Mutex::new(Dispatch {
                    epoch: 0,
                    job: None,
                    running: 0,
                }),
                wake: Condvar::new(),
                idle: Condvar::new(),
                workers,
            }));
            let mut started = 0;
            for _ in 0..workers {
                if std::thread::Builder::new()
                    .name("blinker-chunk".into())
                    .spawn(|| pool.serve())
                    .is_ok()
                {
                    started += 1;
                }
            }
            // A machine that will not give us threads is not one to wait on.
            match started == workers {
                true => Some(pool),
                false => None,
            }
        })
    }

    /// A parked worker's whole life.
    fn serve(&'static self) {
        let mut ran = 0u64;
        loop {
            let job = {
                let mut state = self.state.lock().unwrap_or_else(|held| held.into_inner());
                while state.epoch == ran {
                    state = self
                        .wake
                        .wait(state)
                        .unwrap_or_else(|held| held.into_inner());
                }
                ran = state.epoch;
                state.job
            };
            if let Some(job) = job {
                // SAFETY: `running` was set before the epoch was published and
                // this worker is counted in it, so the submitter is still
                // inside `run` and the closure is still alive. Panics are
                // caught by the body itself, so no unwind crosses this call
                // and the decrement below always happens.
                unsafe { (job.call)(job.body) };
            }
            let mut state = self.state.lock().unwrap_or_else(|held| held.into_inner());
            state.running -= 1;
            if state.running == 0 {
                self.idle.notify_all();
            }
        }
    }

    /// Run `body` on every worker and on this thread, and return when they are
    /// all done with it.
    fn run<F: Fn() + Sync>(&self, body: F) {
        unsafe fn call<F: Fn()>(body: *const ()) {
            unsafe { (*(body as *const F))() }
        }
        let job = Job {
            body: std::ptr::from_ref(&body).cast::<()>(),
            call: call::<F>,
        };
        {
            let mut state = self.state.lock().unwrap_or_else(|held| held.into_inner());
            state.job = Some(job);
            state.running = self.workers;
            state.epoch += 1;
        }
        self.wake.notify_all();

        // This thread runs a chunk too, rather than waiting on work it could
        // be doing.
        body();

        let mut state = self.state.lock().unwrap_or_else(|held| held.into_inner());
        while state.running > 0 {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(|held| held.into_inner());
        }
    }
}

/// One chunk's result, written by the single worker that claimed its index.
struct Slots<R>(Vec<UnsafeCell<Option<R>>>);

// SAFETY: the atomic cursor hands out each index exactly once, so no two
// threads ever hold the same cell, and the submitter reads them only after the
// barrier in `Pool::run`.
unsafe impl<R: Send> Sync for Slots<R> {}

/// Cut `items` into contiguous chunks and run `work` on each, on every core.
///
/// `work` is given a chunk's starting index and its slice, and its results come
/// back in chunk order regardless of which thread finished first.
pub(crate) fn map_chunks<'a, T, R>(
    items: &'a [T],
    work: impl Fn(usize, &'a [T]) -> R + Sync,
) -> Vec<R>
where
    T: Sync + 'a,
    R: Send,
{
    let threads = cores();
    if threads <= 1 || items.len() <= 1 {
        return vec![work(0, items)];
    }

    // At least one item per chunk, and never more chunks than items.
    let wanted = (threads * CHUNKS_PER_THREAD).min(items.len());
    let size = items.len().div_ceil(wanted);
    let bounds: Vec<(usize, usize)> = (0..items.len())
        .step_by(size)
        .map(|start| (start, (start + size).min(items.len())))
        .collect();

    let cursor = AtomicUsize::new(0);
    let slots: Slots<R> = Slots((0..bounds.len()).map(|_| UnsafeCell::new(None)).collect());
    // A panic in `work` has to come back out on this thread rather than take
    // down a pool worker that the next link still needs.
    let escaped: Mutex<Option<Box<dyn std::any::Any + Send>>> = Mutex::new(None);

    let (bounds, work, slots, escaped) = (&bounds, &work, &slots, &escaped);
    let chunks = || loop {
        let next = cursor.fetch_add(1, Ordering::Relaxed);
        let Some(&(start, end)) = bounds.get(next) else {
            return;
        };
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            work(start, &items[start..end])
        }));
        match done {
            // SAFETY: `next` came from the cursor, so this cell is this
            // thread's alone, and nothing reads it until every worker is done.
            Ok(value) => unsafe { *slots.0[next].get() = Some(value) },
            Err(panic) => {
                let mut held = escaped.lock().unwrap_or_else(|held| held.into_inner());
                if held.is_none() {
                    *held = Some(panic);
                }
                // Draining the rest of the cursor would run `work` again on a
                // link that is already failing; leaving the remaining chunks
                // unclaimed lets every worker fall out of the loop instead.
                cursor.fetch_add(bounds.len(), Ordering::Relaxed);
            }
        }
    };

    // Marked on every participant — the parked workers, this thread, and any
    // thread the fallback below creates — so that a `map_chunks` reached from
    // inside `work` sees it whatever path got it here. `chunks` cannot unwind,
    // so the flag is always put back.
    let body = || {
        let outer = IN_JOB.with(|inside| inside.replace(true));
        chunks();
        IN_JOB.with(|inside| inside.set(outer));
    };

    // A pool, unless this thread is already inside a job or another thread is
    // using it. The guard is what makes the second condition safe, so it is
    // held until the job is done.
    let booked = Pool::get()
        .filter(|_| !IN_JOB.with(Cell::get))
        .and_then(|pool| Some((pool, pool.submit.try_lock().ok()?)));
    match booked {
        Some((pool, _holding)) => pool.run(body),
        // No pool, already inside one, or one is busy: threads for this call.
        None => {
            std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads.min(bounds.len()) - 1)
                    .map(|_| scope.spawn(body))
                    .collect();
                body();
                for worker in workers {
                    let _ = worker.join();
                }
            });
        }
    }

    if let Some(panic) = escaped
        .lock()
        .unwrap_or_else(|held| held.into_inner())
        .take()
    {
        std::panic::resume_unwind(panic);
    }
    slots
        .0
        .iter()
        // SAFETY: every worker is done, and this thread now holds the cells
        // alone. `iter` rather than `into_iter` because `slots` is borrowed by
        // `body` above, which outlives it lexically.
        .map(|slot| unsafe { (*slot.get()).take() })
        .map(|slot| slot.expect("every chunk was claimed exactly once"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole module exists for. If chunk results came back in
    /// completion order rather than chunk order, this is what would say so.
    #[test]
    fn results_come_back_in_chunk_order() {
        let items: Vec<usize> = (0..10_000).collect();
        for _ in 0..50 {
            let seen: Vec<usize> = map_chunks(&items, |start, chunk| {
                // Uneven on purpose: without it every chunk takes the same time
                // and finishing order matches chunk order by luck.
                if start % 3 == 0 {
                    std::thread::yield_now();
                }
                chunk.iter().sum::<usize>()
            });
            assert_eq!(seen.iter().sum::<usize>(), items.iter().sum::<usize>());
            let mut at = 0;
            for total in seen {
                let mut expected = 0;
                while expected < total && at < items.len() {
                    expected += items[at];
                    at += 1;
                }
                assert_eq!(expected, total, "a chunk's result landed out of order");
            }
        }
    }

    /// A panic in `work` has to reach the caller, not strand a pooled worker
    /// that the next link in this process still needs. The second call is the
    /// point: it runs on the same pool the first one panicked on.
    #[test]
    fn a_panic_comes_out_on_the_calling_thread_and_the_pool_survives() {
        let items: Vec<usize> = (0..1_000).collect();
        let held = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let escaped = std::panic::catch_unwind(|| {
            map_chunks(&items, |start, _| {
                assert!(start != 0, "the chunk that was asked to fail");
                0usize
            })
        });
        std::panic::set_hook(held);
        let message = escaped.expect_err("the panic should have reached here");
        // Both spellings: a panic with a literal message carries a `&str`, one
        // that formats carries a `String`.
        let text = message
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| message.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            text.contains("the chunk that was asked to fail"),
            "the original panic should survive, got {text:?}"
        );

        let after: Vec<usize> = map_chunks(&items, |_, chunk| chunk.len());
        assert_eq!(after.iter().sum::<usize>(), items.len());
    }

    /// `map_chunks` inside `map_chunks` must not wait on a pool that is already
    /// busy running the outer call — which is this thread.
    #[test]
    fn a_nested_call_does_not_wait_on_itself() {
        let outer: Vec<usize> = (0..64).collect();
        let inner: Vec<usize> = (0..256).collect();
        let seen: Vec<usize> = map_chunks(&outer, |_, chunk| {
            let counted: Vec<usize> = map_chunks(&inner, |_, deep| deep.len());
            assert_eq!(counted.iter().sum::<usize>(), inner.len());
            chunk.len()
        });
        assert_eq!(seen.iter().sum::<usize>(), outer.len());
    }

    /// Several threads calling at once must not wait on each other. This is the
    /// test that was missing: the pool shares one job slot, so an early version
    /// deadlocked the moment `cargo test` ran two of these tests side by side,
    /// and every single-threaded test passed while it did.
    #[test]
    fn concurrent_callers_do_not_wait_on_each_other() {
        let items: Vec<usize> = (0..20_000).collect();
        let items = &items;
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(move || {
                    for _ in 0..20 {
                        let seen: Vec<usize> = map_chunks(items, |_, chunk| chunk.len());
                        assert_eq!(seen.iter().sum::<usize>(), items.len());
                    }
                });
            }
        });
    }

    /// Every chunk is claimed exactly once, so every slot is filled exactly
    /// once — the assumption the `UnsafeCell` writes rest on.
    #[test]
    fn every_chunk_runs_exactly_once() {
        let items: Vec<usize> = (0..5_000).collect();
        let runs: Vec<AtomicUsize> = (0..items.len()).map(|_| AtomicUsize::new(0)).collect();
        let seen: Vec<usize> = map_chunks(&items, |start, chunk| {
            for ran in runs.iter().skip(start).take(chunk.len()) {
                ran.fetch_add(1, Ordering::Relaxed);
            }
            chunk.len()
        });
        assert_eq!(seen.iter().sum::<usize>(), items.len());
        assert!(runs.iter().all(|at| at.load(Ordering::Relaxed) == 1));
    }
}
