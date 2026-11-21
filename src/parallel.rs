//! A small worker pool for per-image work (std threads only).

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use lopdf::{Document, Object, ObjectId};

use crate::decode::MAX_PIXELS;

/// Most objects followed when listing what an image may read.
pub(crate) const REACH_LIMIT: usize = 4096;

/// Most worker threads used, whatever the machine has.
const MAX_WORKERS: usize = 8;

/// How many workers to use: the available cores, at most MAX_WORKERS, and
/// one (the calling thread) where threads cannot be spawned.
fn workers() -> usize {
    if cfg!(target_family = "wasm") {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_WORKERS)
}

/// `f` over `items`, results in the same order. Items run concurrently
/// only while their combined `weight` (decoded pixels) stays within
/// MAX_PIXELS, so peak memory stays near what one maximal image already
/// needs; an item heavier than that runs alone. With one worker, or one
/// item, everything runs on the calling thread.
pub(crate) fn map_bounded<T: Send>(
    items: &[usize],
    weight: impl Fn(usize) -> u64 + Sync,
    f: impl Fn(usize) -> T + Sync,
) -> Vec<T> {
    let workers = workers().min(items.len());
    if workers <= 1 {
        return items.iter().map(|&i| f(i)).collect();
    }
    let next = AtomicUsize::new(0);
    let in_flight = Mutex::new(0u64);
    let freed = Condvar::new();
    let mut results: Vec<Option<T>> = (0..items.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&item) = items.get(k) else {
                            break;
                        };
                        let w = weight(item).clamp(1, MAX_PIXELS);
                        {
                            let mut used = in_flight.lock().unwrap();
                            while *used > 0 && *used + w > MAX_PIXELS {
                                used = freed.wait(used).unwrap();
                            }
                            *used += w;
                        }
                        // Released even if `f` panics, so that no worker
                        // waits forever for room that never frees up.
                        let _release = Release {
                            in_flight: &in_flight,
                            freed: &freed,
                            weight: w,
                        };
                        let result = f(item);
                        mine.push((k, result));
                    }
                    mine
                })
            })
            .collect();
        for handle in handles {
            let mine = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            for (k, result) in mine {
                results[k] = Some(result);
            }
        }
    });
    results.into_iter().map(|r| r.unwrap()).collect()
}

/// Returns an item's weight to the budget when dropped (also on unwind).
struct Release<'a> {
    in_flight: &'a Mutex<u64>,
    freed: &'a Condvar,
    weight: u64,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let mut used = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *used -= self.weight;
        self.freed.notify_all();
    }
}

/// The objects a unit of work may read.
pub(crate) enum Reads {
    Objects(HashSet<ObjectId>),
    /// Too many to list: treat as reading everything.
    Everything,
}

/// Split `units` (in order) into consecutive batches that can each run
/// against one snapshot of the document with the same result as running
/// one unit after another: a batch ends before the first unit that reads
/// an object written by an earlier unit of the same batch. (A unit reading
/// a later unit's object sees it unwritten either way.)
pub(crate) fn batches(
    units: &[usize],
    reads: impl Fn(usize) -> Reads,
    writes: impl Fn(usize) -> Vec<ObjectId>,
) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut batch: Vec<usize> = Vec::new();
    let mut written: HashSet<ObjectId> = HashSet::new();
    for &unit in units {
        let conflict = match reads(unit) {
            Reads::Everything => !batch.is_empty(),
            Reads::Objects(ids) => ids.iter().any(|id| written.contains(id)),
        };
        if conflict {
            out.push(std::mem::take(&mut batch));
            written.clear();
        }
        batch.push(unit);
        written.extend(writes(unit));
    }
    if !batch.is_empty() {
        out.push(batch);
    }
    out
}

/// Every object reachable through references from object `id` (itself
/// included), not looking inside stream contents; Everything past `limit`.
pub(crate) fn reachable(doc: &Document, id: ObjectId, limit: usize) -> Reads {
    let mut seen: HashSet<ObjectId> = HashSet::from([id]);
    let mut stack = vec![id];
    while let Some(next) = stack.pop() {
        let mut refs = Vec::new();
        if let Some(o) = doc.objects.get(&next) {
            collect_refs(o, &mut refs);
        }
        for r in refs {
            if seen.insert(r) {
                if seen.len() > limit {
                    return Reads::Everything;
                }
                stack.push(r);
            }
        }
    }
    Reads::Objects(seen)
}

fn collect_refs(o: &Object, out: &mut Vec<ObjectId>) {
    match o {
        Object::Reference(r) => out.push(*r),
        Object::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        Object::Dictionary(d) => d.iter().for_each(|(_, x)| collect_refs(x, out)),
        Object::Stream(s) => s.dict.iter().for_each(|(_, x)| collect_refs(x, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_their_order() {
        let items: Vec<usize> = (0..100).collect();
        assert_eq!(
            map_bounded(&items, |_| 1, |i| i * 2),
            (0..100).map(|i| i * 2).collect::<Vec<_>>()
        );
    }

    #[test]
    fn heavy_items_never_overlap() {
        let items: Vec<usize> = (0..16).collect();
        let running = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        map_bounded(
            &items,
            |_| MAX_PIXELS / 2 + 1,
            |_| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
                running.fetch_sub(1, Ordering::SeqCst);
            },
        );
        assert_eq!(most.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panicking_item_does_not_hang_the_others() {
        let items: Vec<usize> = (0..4).collect();
        let result = std::panic::catch_unwind(|| {
            map_bounded(
                &items,
                |_| MAX_PIXELS,
                |i| {
                    if i == 0 {
                        panic!("boom");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                },
            )
        });
        assert!(result.is_err());
    }

    #[test]
    fn batches_split_before_reading_an_earlier_write() {
        // Unit 2 reads what unit 0 writes; unit 1 reads what unit 3 writes
        // (later, so no split); unit 4 reads everything.
        let reads = |u: usize| match u {
            1 => Reads::Objects(HashSet::from([(13, 0)])),
            2 => Reads::Objects(HashSet::from([(10, 0)])),
            4 => Reads::Everything,
            _ => Reads::Objects(HashSet::new()),
        };
        let writes = |u: usize| vec![(10 + u as u32, 0)];
        assert_eq!(
            batches(&[0, 1, 2, 3, 4, 5], reads, writes),
            vec![vec![0, 1], vec![2, 3], vec![4, 5]]
        );
    }
}
