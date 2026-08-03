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

/// How many chunks per thread. Four is enough to absorb an object that is ten
/// times the size of its neighbours without making the merge itself the cost.
const CHUNKS_PER_THREAD: usize = 4;

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
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
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

    let cursor = std::sync::atomic::AtomicUsize::new(0);
    let (bounds, work) = (&bounds, &work);
    let claimed: Vec<Vec<(usize, R)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(bounds.len()))
            .map(|_| {
                let cursor = &cursor;
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        let next = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(start, end)) = bounds.get(next) else {
                            return mine;
                        };
                        mine.push((next, work(start, &items[start..end])));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a worker panicked"))
            .collect()
    });

    // Back into chunk order. `Option` because the results arrive scattered and
    // there is no meaningful default to fill with.
    let mut ordered: Vec<Option<R>> = (0..bounds.len()).map(|_| None).collect();
    for result in claimed.into_iter().flatten() {
        ordered[result.0] = Some(result.1);
    }
    ordered
        .into_iter()
        .map(|result| result.expect("every chunk was claimed exactly once"))
        .collect()
}
