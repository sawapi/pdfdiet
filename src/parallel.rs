//! A small worker pool for per-image work (std threads only).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use crate::decode::MAX_PIXELS;

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
                        let result = f(item);
                        *in_flight.lock().unwrap() -= w;
                        freed.notify_all();
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
}
