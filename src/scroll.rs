//! Scroll detection on per-row hashes.
//!
//! Each pixel row is hashed in [`BANDS`] vertical bands, so a scrollbar, a sidebar or
//! the pointer spoils only its own band. Hashes are 128-bit because equal hashes are
//! also taken as equal pixels (like tile keys). A shift `dy` means `new[y] == old[y + dy]`
//! (content moved up by `dy` pixels when positive).

use crate::frame::FrameBuf;
use crate::par;
use std::collections::HashMap;
use std::ops::Range;

pub const BANDS: usize = 8;

/// Pixel columns of band `b` for rows `width` pixels wide.
pub fn band_range(width: u32, b: usize) -> Range<u32> {
    (b * width as usize / BANDS) as u32..((b + 1) * width as usize / BANDS) as u32
}
/// Rows per worker below which hashing stays on one thread.
const PAR_MIN_ROWS: usize = 128;

/// Hashes rows `rows` of `fb` over its first `width` pixels, `BANDS` values per row.
/// Rows or bands outside the framebuffer hash as empty.
pub fn hash_rows(fb: &FrameBuf, width: u32, rows: Range<u32>, threads: usize, out: &mut [u128]) {
    debug_assert_eq!(out.len(), rows.len() * BANDS);
    let width = width.min(fb.width()) as usize;
    let stride = fb.width() as usize * 3;
    let data = fb.data();
    let (first, height) = (rows.start, fb.height());
    par::for_chunks_mut(out, BANDS, threads, PAR_MIN_ROWS, |start, chunk| {
        for (i, row) in chunk.chunks_exact_mut(BANDS).enumerate() {
            let y = first + (start / BANDS + i) as u32;
            for (b, h) in row.iter_mut().enumerate() {
                let (x0, x1) = (b * width / BANDS, (b + 1) * width / BANDS);
                debug_assert_eq!(band_range(width as u32, b), x0 as u32..x1 as u32);
                *h = if y < height {
                    let base = y as usize * stride;
                    xxhash_rust::xxh3::xxh3_128(&data[base + x0 * 3..base + x1 * 3])
                } else {
                    0
                };
            }
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shift {
    pub dy: i32,
    /// Candidate rows where at least half of the informative bands match.
    pub matched: usize,
    /// Rows that can match: those of the range, minus the `|dy|` newly exposed ones,
    /// whose old content has an informative (unique) band.
    pub candidates: usize,
    /// First and last matching rows (new-frame rows, relative to the range).
    pub first: usize,
    pub last: usize,
}

impl Shift {
    pub fn coverage(&self) -> f64 {
        self.matched as f64 / self.candidates.max(1) as f64
    }

    /// Rows `start..end` that move, newly exposed ones included.
    pub fn moving(&self) -> std::ops::Range<usize> {
        let n = self.dy.unsigned_abs() as usize;
        if self.dy > 0 {
            self.first..self.last + 1 + n
        } else {
            self.first.saturating_sub(n)..self.last + 1
        }
    }
}

/// Finds the dominant non-zero shift between `old` and `new` (hash tables of the same
/// `rows` rows, `BANDS` per row). Only rows whose band hash is unique in `old` vote,
/// as repeated rows (blank lines) fit any shift.
pub fn find_shift(old: &[u128], new: &[u128], max_shift: usize) -> Option<Shift> {
    let rows = old.len() / BANDS;
    debug_assert_eq!(new.len(), old.len());
    let mut seen: HashMap<(usize, u128), Option<usize>> = HashMap::with_capacity(rows * BANDS);
    for y in 0..rows {
        for b in 0..BANDS {
            seen.entry((b, old[y * BANDS + b])).and_modify(|e| *e = None).or_insert(Some(y));
        }
    }
    let mut votes: HashMap<i32, usize> = HashMap::new();
    for y in 0..rows {
        for b in 0..BANDS {
            if let Some(Some(oy)) = seen.get(&(b, new[y * BANDS + b])) {
                let dy = *oy as i32 - y as i32;
                if dy != 0 && dy.unsigned_abs() as usize <= max_shift {
                    *votes.entry(dy).or_default() += 1;
                }
            }
        }
    }
    // The final key breaks a tie between `d` and `-d`, which HashMap order would decide at random.
    let (&dy, _) = votes.iter().max_by_key(|(&dy, &n)| (n, std::cmp::Reverse(dy.unsigned_abs()), dy))?;
    let span = dy.unsigned_abs() as usize;
    let range = if dy > 0 { 0..rows - span } else { span..rows };
    // Judge each row by its informative bands only: blank bands are equal everywhere and
    // would let a new line "match" the static prompt line under it.
    let mut candidates = 0;
    let rows_matching: Vec<usize> = range
        .filter(|&y| {
            let oy = (y as i32 + dy) as usize;
            let informative: Vec<usize> =
                (0..BANDS).filter(|&b| matches!(seen.get(&(b, old[oy * BANDS + b])), Some(Some(_)))).collect();
            if informative.is_empty() {
                return false;
            }
            candidates += 1;
            let equal = informative.iter().filter(|&&b| new[y * BANDS + b] == old[oy * BANDS + b]).count();
            equal * 2 >= informative.len() && equal > 0
        })
        .collect();
    let (&first, &last) = rows_matching.first().zip(rows_matching.last())?;
    Some(Shift { dy, matched: rows_matching.len(), candidates, first, last })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hash table of a synthetic page: row y of the page has content `page[y]`, band b
    /// shows `page[y] * 31 + b`; `static_band` keeps a fixed value (a scrollbar).
    fn view(page: &[u128], top: usize, rows: usize, static_band: Option<usize>) -> Vec<u128> {
        let mut t = Vec::new();
        for y in 0..rows {
            for b in 0..BANDS {
                t.push(if Some(b) == static_band { 7 } else { page[top + y] * 31 + b as u128 });
            }
        }
        t
    }

    fn page(n: usize) -> Vec<u128> {
        // Every third row is blank, like the gaps between text lines.
        (0..n as u128).map(|y| if y % 3 == 0 { 1 } else { y * 2_654_435_761 % 1_000_003 }).collect()
    }

    #[test]
    fn finds_scroll_in_both_directions_despite_a_static_band() {
        let p = page(400);
        for (from, to) in [(100, 137), (137, 100), (0, 1), (0, 119)] {
            let old = view(&p, from, 120, Some(BANDS - 1));
            let new = view(&p, to, 120, Some(BANDS - 1));
            let s = find_shift(&old, &new, 119).expect("a shift");
            assert_eq!(s.dy, to as i32 - from as i32, "{from}->{to}");
            if (to as i32 - from as i32).unsigned_abs() < 120 {
                assert_eq!(s.matched, s.candidates, "{from}->{to}: every overlapping row matches");
            }
        }
    }

    #[test]
    fn moving_rows_exclude_a_static_footer() {
        let p = page(400);
        // 100 moving rows over a 20-row static footer (like less's prompt line).
        let with_footer = |top: usize| {
            let mut t = view(&p, top, 100, None);
            t.extend(
                view(&[5; 20].map(|v: u128| v + 1_000), 0, 20, None).iter().enumerate().map(|(i, v)| v + i as u128),
            );
            t
        };
        let s = find_shift(&with_footer(10), &with_footer(13), 90).expect("a shift");
        assert_eq!((s.dy, s.moving()), (3, 0..100));
        let s = find_shift(&with_footer(13), &with_footer(10), 90).expect("a shift");
        assert_eq!((s.dy, s.moving()), (-3, 0..100));
    }

    #[test]
    fn no_shift_for_unrelated_or_identical_content() {
        let p = page(400);
        let q: Vec<u128> = page(400).iter().map(|v| v + 99_999_999).collect();
        assert!(find_shift(&view(&p, 10, 100, None), &view(&q, 10, 100, None), 99).is_none());
        let same = view(&p, 10, 100, None);
        assert!(find_shift(&same, &same, 99).is_none(), "identical frames vote only for dy=0");
    }

    #[test]
    fn hashes_rows_per_band() {
        let mut fb = FrameBuf::new(16, 4);
        fb.set_pixel(15, 2, [9, 9, 9]);
        let mut t = vec![0; 5 * BANDS];
        hash_rows(&fb, 16, 0..5, 2, &mut t);
        assert_eq!(t[0..BANDS], t[BANDS..2 * BANDS], "identical rows hash alike");
        assert_ne!(t[2 * BANDS + BANDS - 1], t[BANDS - 1], "the changed pixel is in the last band");
        assert_eq!(t[2 * BANDS..3 * BANDS - 1], t[0..BANDS - 1], "other bands are unaffected");
        assert!(t[4 * BANDS..].iter().all(|&h| h == 0), "rows below the frame are empty");
    }
}
