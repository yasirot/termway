//! Data parallelism on scoped threads. There is no pool: a frame takes tens of
//! milliseconds while spawning a thread costs tens of microseconds.

use std::ops::Range;

/// Default worker count: the machine's parallelism, capped so the compositor and the
/// apps keep some cores.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8)
}

/// Splits `0..n` into at most `threads` contiguous ranges of at least `min_len` items
/// (except when `n` is smaller) and returns `f` of each range, in order. The first
/// range runs on the calling thread.
pub fn map_ranges<R: Send>(n: usize, threads: usize, min_len: usize, f: impl Fn(Range<usize>) -> R + Sync) -> Vec<R> {
    let parts = (n / min_len.max(1)).clamp(1, threads.max(1));
    if parts == 1 {
        return vec![f(0..n)];
    }
    let mut ranges = (0..parts).map(|i| i * n / parts..(i + 1) * n / parts);
    let first = ranges.next().expect("parts >= 1");
    std::thread::scope(|s| {
        let f = &f;
        let handles: Vec<_> = ranges.map(|r| s.spawn(move || f(r))).collect();
        let mut out = vec![f(first)];
        out.extend(handles.into_iter().map(|h| h.join().expect("worker panicked")));
        out
    })
}

/// Runs `f(first_index, chunk)` on contiguous chunks of `data` whose lengths are
/// multiples of `unit`, in parallel.
pub fn for_chunks_mut<T: Send>(
    data: &mut [T],
    unit: usize,
    threads: usize,
    min_units: usize,
    f: impl Fn(usize, &mut [T]) + Sync,
) {
    let units = data.len() / unit.max(1);
    let parts = (units / min_units.max(1)).clamp(1, threads.max(1));
    if parts == 1 {
        f(0, data);
        return;
    }
    let per = units.div_ceil(parts) * unit;
    std::thread::scope(|s| {
        let f = &f;
        let mut chunks = data.chunks_mut(per).enumerate();
        let (_, first) = chunks.next().expect("at least one chunk");
        for (i, chunk) in chunks {
            s.spawn(move || f(i * per, chunk));
        }
        f(0, first);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_everything_in_order() {
        for (n, threads, min) in [(0, 4, 1), (1, 4, 1), (10, 3, 1), (10, 8, 4), (1000, 8, 16)] {
            let parts = map_ranges(n, threads, min, |r| r);
            assert!(parts.len() <= threads.max(1));
            let flat: Vec<usize> = parts.into_iter().flatten().collect();
            assert_eq!(flat, (0..n).collect::<Vec<_>>(), "n={n} threads={threads} min={min}");
        }
    }

    #[test]
    fn chunks_respect_units() {
        let mut v = vec![0usize; 3 * 101];
        for_chunks_mut(&mut v, 3, 4, 10, |start, chunk| {
            assert_eq!(start % 3, 0);
            assert_eq!(chunk.len() % 3, 0);
            for (i, x) in chunk.iter_mut().enumerate() {
                *x = start + i;
            }
        });
        assert_eq!(v, (0..3 * 101).collect::<Vec<_>>());
    }
}
