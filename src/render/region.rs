//! Phase 4 scroll copy.
//!
//! A scrolling area becomes a region of full-width tile rows drawn by "pieces":
//! horizontal image strips that tile the region exactly. A shift re-places every piece
//! `dy` pixels further (source crop plus in-cell offset) and transmits only the newly
//! exposed strip. `base` mirrors what the pieces show; tiles whose pixels differ from it
//! (a scrollbar, sticky headers, the pointer) are drawn as ordinary tiles on top (z=1).
//!
//! WezTerm copies the placement-id images of a cell back into it whenever an image is
//! placed over the cell, doubling them each time (the attachments and quads grow until
//! WezTerm hangs and crashes). There, pieces carry no placement id (placing an image
//! without one replaces its only placement) and overlays are lifted before pieces move.

use super::{par, tile_content, Compat, Content, Packer, Renderer, SlotImage, EPHEMERAL_Z};
use crate::frame::FrameBuf;
use crate::geom::{DirtyMap, Rect};
use crate::kitty::{self, Delete, Encoding, Image, Placement};
use crate::render::FrameStats;
use crate::scroll::{self, BANDS};
use crate::vt::Vt;
use std::collections::VecDeque;

/// A scroll must be seen in this many consecutive frames before a region is created,
/// so a single small scroll does not cost a full-resolution upload.
const ENTRY_STREAK: u32 = 2;
const MIN_REGION_TILE_ROWS: u32 = 4;
/// Share of the rows that can match which must match the shift.
const MIN_COVERAGE: f64 = 0.6;
/// A shifted region whose pieces differ from the frame in more than this share of its
/// tiles is rebuilt: static rows inside it (a sticky header, a prompt line) leave stale
/// copies in the pieces that would otherwise pile up.
const MAX_STALE_SHARE: f64 = 0.25;
/// Largest shift, as a share of the scrolled height, still drawn by moving pixels: the
/// exposed strip goes at full resolution, while redrawing the whole area at half
/// resolution costs about a quarter of its pixels.
const MAX_SHIFT_SHARE: f64 = 0.25;
const MIN_MATCHED_ROWS: usize = 16;
/// Height of the pieces a new region is uploaded in (parallel compression, and strips
/// that can be dropped once scrolled out).
const ENTRY_PIECE_PX: u32 = 256;
/// Every shift adds a strip; beyond this many pieces the adjacent pair with the least
/// rows is uploaded again as one, which bounds the placements sent per shift.
const MAX_PIECES: usize = 32;
/// Every piece has one placement, re-placed on each shift (kitty model only).
const PIECE_PLACEMENT: u32 = 1;

#[derive(Clone, Copy, Debug)]
struct Piece {
    id: u32,
    /// First image row still shown.
    img_y: u32,
    /// Region row where `img_y` is shown.
    top: u32,
    /// Rows shown.
    h: u32,
}

pub(super) struct Region {
    /// Tile rows covered.
    pub(super) ty0: u32,
    pub(super) ty1: u32,
    /// Pixel rows `top..top + height` of the screen, `width` pixels wide.
    top: u32,
    height: u32,
    width: u32,
    /// Pixels shown by the pieces; logical row `r` is stored at `(r + off) % height`.
    base: Vec<u8>,
    /// Row hashes of `base` (`BANDS` per row), stored like `base`.
    hashes: Vec<u128>,
    off: u32,
    /// Top to bottom, covering `0..height` without gaps.
    pieces: VecDeque<Piece>,
}

impl Region {
    fn row_len(&self) -> usize {
        self.width as usize * 3
    }

    fn base_row(&self, r: u32) -> &[u8] {
        let i = ((r + self.off) % self.height) as usize * self.row_len();
        &self.base[i..i + self.row_len()]
    }

    /// Copies region rows `rows` from the frame, with their hashes from `frame_hashes`.
    fn store_rows(&mut self, fb: &FrameBuf, frame_hashes: &[u128], rows: std::ops::Range<u32>) {
        let (len, stride) = (self.row_len(), fb.width() as usize * 3);
        for r in rows {
            let p = ((r + self.off) % self.height) as usize;
            let s = (self.top + r) as usize;
            self.base[p * len..(p + 1) * len].copy_from_slice(&fb.data()[s * stride..s * stride + len]);
            self.hashes[p * BANDS..(p + 1) * BANDS].copy_from_slice(&frame_hashes[s * BANDS..(s + 1) * BANDS]);
        }
    }

    fn row_hashes(&self, r: u32) -> &[u128] {
        let p = ((r + self.off) % self.height) as usize;
        &self.hashes[p * BANDS..(p + 1) * BANDS]
    }

    fn copy_rows(&self, rows: std::ops::Range<u32>, out: &mut Vec<u8>) {
        out.clear();
        for r in rows {
            out.extend_from_slice(self.base_row(r));
        }
    }

    /// Whether the screen pixels `px` of `fb` equal what the pieces show there. Rows
    /// whose band hashes match are taken as equal without comparing pixels.
    fn shows(&self, fb: &FrameBuf, frame_hashes: &[u128], px: Rect) -> bool {
        let bands = (0..BANDS).filter(|&b| {
            let r = scroll::band_range(self.width, b);
            r.start < px.x + px.w && px.x < r.end
        });
        let bands: Vec<usize> = bands.collect();
        let hashed = (px.y..px.y + px.h).all(|y| {
            let base = self.row_hashes(y - self.top);
            bands.iter().all(|&b| frame_hashes[y as usize * BANDS + b] == base[b])
        });
        if hashed {
            return true;
        }
        let stride = fb.width() as usize * 3;
        let (x0, x1) = (px.x as usize * 3, (px.x + px.w) as usize * 3);
        (px.y..px.y + px.h).all(|y| {
            let s = y as usize * stride;
            fb.data()[s + x0..s + x1] == self.base_row(y - self.top)[x0..x1]
        })
    }

    fn check(&self) {
        let mut next = 0;
        for p in &self.pieces {
            assert_eq!(p.top, next, "pieces must tile the region");
            assert!(p.h > 0);
            next += p.h;
        }
        assert_eq!(next, self.height, "pieces must cover the region");
    }
}

impl Renderer {
    pub(super) fn in_region(&self, ty: u32) -> bool {
        self.region.as_ref().is_some_and(|r| (r.ty0..r.ty1).contains(&ty))
    }

    /// Screen pixel rows of tile rows `ty0..ty1` over the full grid width.
    fn rows_px(&self, ty0: u32, ty1: u32) -> Rect {
        self.grid.cells_to_px(self.grid.tiles_to_cells(Rect::new(0, ty0, self.grid.tiles_x(), ty1 - ty0)))
    }

    /// Hashes the rows of dirty tile rows and handles the scroll region: shifts it,
    /// draws what differs from it, creates or dissolves it. Tiles of a region that
    /// dissolves are marked in `dirty` for the normal path.
    pub(super) fn scroll_step(&mut self, fb: &FrameBuf, dirty: &mut DirtyMap, vt: &mut Vt, st: &mut FrameStats) {
        let old = self.rehash(fb, dirty);
        let (w, h) = (self.grid.geom.px_width(), self.grid.geom.px_height());
        if fb.width() < w || fb.height() < h {
            // Scroll copy reads rows straight from the frame, which must cover the grid.
            self.dissolve(Some(dirty), vt);
            return;
        }
        let dirty_rows: Vec<u32> =
            (0..self.grid.tiles_y()).filter(|&ty| (0..self.grid.tiles_x()).any(|tx| dirty.is_marked(tx, ty))).collect();
        let Some((&first, &last)) = dirty_rows.first().zip(dirty_rows.last()) else { return };
        match &self.region {
            Some(r) if dirty_rows.iter().any(|&ty| (r.ty0..r.ty1).contains(&ty)) => {
                let shift = self.detect(&old, r.ty0, r.ty1).map(|s| s.dy);
                self.update_region(fb, shift, dirty, vt, st);
            }
            Some(_) => {}
            None if last + 1 - first >= MIN_REGION_TILE_ROWS => match self.detect(&old, first, last + 1) {
                Some(s) => {
                    self.scroll_streak += 1;
                    let top = self.rows_px(first, last + 1).y as usize;
                    let moving = s.moving();
                    let rows = self.tile_rows_within(top + moving.start, top + moving.end);
                    if self.scroll_streak >= ENTRY_STREAK && rows.len() as u32 >= MIN_REGION_TILE_ROWS {
                        self.enter_region(fb, rows.start, rows.end, vt, st);
                    }
                }
                None => self.scroll_streak = 0,
            },
            None => self.scroll_streak = 0,
        }
    }

    /// Updates the row hash table for dirty tile rows; returns the previous table.
    fn rehash(&mut self, fb: &FrameBuf, dirty: &DirtyMap) -> Vec<u128> {
        let rows = self.grid.geom.px_height() as usize;
        if self.row_hashes.len() != rows * BANDS {
            self.row_hashes = vec![0; rows * BANDS];
        }
        let old = self.row_hashes.clone();
        let width = self.grid.geom.px_width();
        let dirty_row = |ty: u32| (0..self.grid.tiles_x()).any(|tx| dirty.is_marked(tx, ty));
        // Runs of dirty tile rows, hashed in one parallel pass each.
        let mut ty = 0;
        while ty < self.grid.tiles_y() {
            if !dirty_row(ty) {
                ty += 1;
                continue;
            }
            let start = ty;
            while ty < self.grid.tiles_y() && dirty_row(ty) {
                ty += 1;
            }
            let px = self.rows_px(start, ty);
            let (a, b) = (px.y as usize * BANDS, (px.y + px.h) as usize * BANDS);
            scroll::hash_rows(fb, width, px.y..px.y + px.h, self.cfg.threads, &mut self.row_hashes[a..b]);
        }
        old
    }

    fn detect(&self, old: &[u128], ty0: u32, ty1: u32) -> Option<scroll::Shift> {
        let px = self.rows_px(ty0, ty1);
        let (a, b) = (px.y as usize * BANDS, (px.y + px.h) as usize * BANDS);
        let max = (px.h as f64 * MAX_SHIFT_SHARE) as usize;
        scroll::find_shift(&old[a..b], &self.row_hashes[a..b], max)
            .filter(|s| s.coverage() >= MIN_COVERAGE && s.matched >= MIN_MATCHED_ROWS)
    }

    /// Tile rows lying entirely within pixel rows `y0..y1`.
    fn tile_rows_within(&self, y0: usize, y1: usize) -> std::ops::Range<u32> {
        let inside: Vec<u32> = (0..self.grid.tiles_y())
            .filter(|&ty| {
                let px = self.rows_px(ty, ty + 1);
                px.y as usize >= y0 && (px.y + px.h) as usize <= y1
            })
            .collect();
        match (inside.first(), inside.last()) {
            (Some(&a), Some(&b)) => a..b + 1,
            _ => 0..0,
        }
    }

    fn enter_region(&mut self, fb: &FrameBuf, ty0: u32, ty1: u32, vt: &mut Vt, st: &mut FrameStats) {
        let tiles = Rect::new(0, ty0, self.grid.tiles_x(), ty1 - ty0);
        if self.cfg.compat == Compat::WezTerm {
            // Pieces share z=0 with ephemerals, which WezTerm does not stack by id.
            self.erase_covered(tiles, vt);
        }
        for ty in ty0..ty1 {
            for tx in 0..self.grid.tiles_x() {
                let i = self.grid.index(tx, ty);
                self.retire_image(i, self.grid.tile_cells(tx, ty), vt);
                self.set_lowres(i, None);
                let s = &mut self.slots[i];
                s.cover += 1;
                s.image = SlotImage::Scroll;
                s.shown = Content::Unknown;
            }
        }
        let px = self.rows_px(ty0, ty1);
        let mut base = Vec::new();
        fb.extract(px, &mut base);
        let hashes = self.row_hashes[px.y as usize * BANDS..(px.y + px.h) as usize * BANDS].to_vec();
        let (top, height, width) = (px.y, px.h, px.w);
        self.region = Some(Region { ty0, ty1, top, height, width, base, hashes, off: 0, pieces: VecDeque::new() });
        let spans: Vec<(u32, u32)> =
            (0..px.h).step_by(ENTRY_PIECE_PX as usize).map(|t| (t, ENTRY_PIECE_PX.min(px.h - t))).collect();
        let pieces = self.send_pieces(&spans, vt, st);
        self.region.as_mut().expect("region just created").pieces.extend(pieces);
        self.place_pieces(vt, true);
        self.scroll_streak = 0;
    }

    /// Removes the region; its slots that showed the pieces need redrawing and are
    /// marked in `dirty` when given.
    pub(super) fn dissolve(&mut self, mut dirty: Option<&mut DirtyMap>, vt: &mut Vt) {
        let Some(r) = self.region.take() else { return };
        for p in &r.pieces {
            kitty::delete(&mut vt.buf, Delete::Image { id: p.id });
        }
        for ty in r.ty0..r.ty1 {
            for tx in 0..self.grid.tiles_x() {
                let i = self.grid.index(tx, ty);
                let s = &mut self.slots[i];
                s.cover -= 1;
                if s.image == SlotImage::Scroll {
                    s.image = SlotImage::None;
                    s.shown = Content::Unknown;
                    if let Some(d) = dirty.as_deref_mut() {
                        d.mark(tx, ty);
                    }
                }
            }
        }
        self.scroll_streak = 0;
    }

    fn update_region(
        &mut self,
        fb: &FrameBuf,
        shift: Option<i32>,
        dirty: &mut DirtyMap,
        vt: &mut Vt,
        st: &mut FrameStats,
    ) {
        let (ty0, ty1) = {
            let r = self.region.as_ref().expect("region exists");
            (r.ty0, r.ty1)
        };
        if let Some(dy) = shift {
            self.shift_region(fb, dy, vt, st);
        }
        let tx = self.grid.tiles_x();
        let tiles: Vec<(u32, u32)> = (ty0..ty1)
            .flat_map(|y| (0..tx).map(move |x| (x, y)))
            .filter(|&(x, y)| shift.is_some() || dirty.is_marked(x, y))
            .collect();
        let (grid, region, hashes) = (self.grid, self.region.as_ref().expect("region exists"), &self.row_hashes);
        let results = par::map_ranges(tiles.len(), self.cfg.threads, 256, |r| {
            let mut buf = Vec::new();
            tiles[r]
                .iter()
                .map(|&(x, y)| {
                    let px = grid.tile_px(x, y);
                    if region.shows(fb, hashes, px) {
                        None
                    } else {
                        Some(tile_content(fb, px, &mut buf))
                    }
                })
                .collect::<Vec<_>>()
        });
        let results: Vec<Option<Content>> = results.into_iter().flatten().collect();
        // Only changes count: overlays that stay as they are cost nothing.
        let changes = tiles
            .iter()
            .zip(&results)
            .filter(|(&(x, y), c)| {
                let s = &self.slots[self.grid.index(x, y)];
                c.is_some_and(|c| !(matches!(s.image, SlotImage::Tile { .. }) && s.shown == c))
            })
            .count();
        let differing = results.iter().filter(|c| c.is_some()).count();
        crate::log_debug!(
            "scroll region {ty0}..{ty1}: shift {shift:?}, {differing} tiles differ from the pieces, {changes} changed"
        );
        let region_tiles = ((ty1 - ty0) * tx) as usize;
        let stale = shift.is_some() && differing as f64 > MAX_STALE_SHARE * region_tiles as f64;
        if changes * 2 > region_tiles || stale {
            // Mostly new content: the pieces no longer help.
            self.dissolve(Some(dirty), vt);
            return;
        }
        if let Some(dy) = shift {
            if self.cfg.compat == Compat::WezTerm {
                self.lift_overlays(vt);
            }
            self.place_pieces(vt, dy > 0);
            st.scroll_shifts += 1;
        }
        for ((x, y), c) in tiles.into_iter().zip(results) {
            let i = self.grid.index(x, y);
            match c {
                None => {
                    if matches!(self.slots[i].image, SlotImage::Tile { .. }) {
                        self.retire_image(i, self.grid.tile_cells(x, y), vt);
                        st.changed_tiles += 1;
                    }
                    self.slots[i].image = SlotImage::Scroll;
                    self.slots[i].shown = Content::Unknown;
                }
                Some(c) => self.update_tile(fb, x, y, c, vt, st),
            }
        }
    }

    /// WezTerm: deletes the overlay tiles of the region so moving the pieces cannot
    /// duplicate them; the caller places the ones still needed again.
    fn lift_overlays(&mut self, vt: &mut Vt) {
        let (ty0, ty1) = {
            let r = self.region.as_ref().expect("region exists");
            (r.ty0, r.ty1)
        };
        for ty in ty0..ty1 {
            for tx in 0..self.grid.tiles_x() {
                let i = self.grid.index(tx, ty);
                if matches!(self.slots[i].image, SlotImage::Tile { .. }) {
                    self.retire_image(i, self.grid.tile_cells(tx, ty), vt);
                    self.slots[i].image = SlotImage::Scroll;
                    self.slots[i].shown = Content::Unknown;
                }
            }
        }
    }

    /// Moves the region's content by `dy` (up when positive) and uploads what appears;
    /// the pieces are placed by the caller.
    fn shift_region(&mut self, fb: &FrameBuf, dy: i32, vt: &mut Vt, st: &mut FrameStats) {
        let r = self.region.as_mut().expect("region exists");
        let (h, n) = (r.height, dy.unsigned_abs());
        r.off = if dy > 0 { (r.off + n) % h } else { (r.off + h - n) % h };
        let exposed = if dy > 0 { h - n..h } else { 0..n };
        r.store_rows(fb, &self.row_hashes, exposed.clone());
        let mut gone = Vec::new();
        for p in r.pieces.iter_mut() {
            let top = p.top as i64 - dy as i64;
            let (mut img_y, mut t, mut ph) = (p.img_y as i64, top, p.h as i64);
            if t < 0 {
                img_y -= t;
                ph += t;
                t = 0;
            }
            ph = ph.min(h as i64 - t);
            if ph <= 0 {
                gone.push(p.id);
                p.h = 0;
            } else {
                (p.img_y, p.top, p.h) = (img_y as u32, t as u32, ph as u32);
            }
        }
        r.pieces.retain(|p| p.h > 0);
        for id in gone {
            kitty::delete(&mut vt.buf, Delete::Image { id });
        }
        let new = self.send_pieces(&[(exposed.start, n)], vt, st);
        let r = self.region.as_mut().expect("region exists");
        if dy > 0 {
            r.pieces.extend(new);
        } else {
            for p in new.into_iter().rev() {
                r.pieces.push_front(p);
            }
        }
        while self.region.as_ref().is_some_and(|r| r.pieces.len() > MAX_PIECES) {
            self.merge_smallest_pair(vt, st);
        }
    }

    /// Uploads the adjacent pair of pieces with the fewest rows again as one piece.
    fn merge_smallest_pair(&mut self, vt: &mut Vt, st: &mut FrameStats) {
        let r = self.region.as_ref().expect("region exists");
        let i = (0..r.pieces.len() - 1).min_by_key(|&i| r.pieces[i].h + r.pieces[i + 1].h).expect("two pieces");
        let (a, b) = (r.pieces[i], r.pieces[i + 1]);
        kitty::delete(&mut vt.buf, Delete::Image { id: a.id });
        kitty::delete(&mut vt.buf, Delete::Image { id: b.id });
        let merged = self.send_pieces(&[(a.top, a.h + b.h)], vt, st);
        let r = self.region.as_mut().expect("region exists");
        r.pieces.remove(i + 1);
        r.pieces[i] = merged[0];
    }

    /// Transmits region rows `(top, h)` from `base` as new piece images (not placed).
    fn send_pieces(&mut self, spans: &[(u32, u32)], vt: &mut Vt, st: &mut FrameStats) -> Vec<Piece> {
        let r = self.region.as_ref().expect("region exists");
        let (width, level, threads, png) = (r.width, self.cfg.zlib_level, self.cfg.threads, self.cfg.large_png);
        let raws: Vec<Vec<u8>> = spans
            .iter()
            .map(|&(t, h)| {
                let mut v = Vec::new();
                r.copy_rows(t..t + h, &mut v);
                v
            })
            .collect();
        let packed: Vec<(Vec<u8>, Encoding)> = if raws.len() == 1 {
            let mut z = Vec::new();
            let e = self.compressor.pack(&raws[0], width, spans[0].1, threads, png, &mut z);
            vec![(if e == Encoding::Raw { raws.into_iter().next().expect("one span") } else { z }, e)]
        } else {
            par::map_ranges(raws.len(), threads, 1, |range| {
                let mut p = Packer::new(level);
                raws[range.clone()]
                    .iter()
                    .zip(&spans[range])
                    .map(|(raw, &(_, h))| {
                        let mut z = Vec::new();
                        let e = p.pack(raw, width, h, 1, png, &mut z);
                        (if e == Encoding::Raw { raw.clone() } else { z }, e)
                    })
                    .collect::<Vec<_>>()
            })
            .into_iter()
            .flatten()
            .collect()
        };
        let mut out = Vec::with_capacity(spans.len());
        for (&(top, h), (data, encoding)) in spans.iter().zip(packed) {
            let id = self.alloc_id();
            kitty::transmit(&mut vt.buf, &Image { id, width, height: h, data: &data, encoding }, None);
            st.images_sent += 1;
            st.payload_bytes += data.len() as u64;
            out.push(Piece { id, img_y: 0, top, h });
        }
        out
    }

    /// Re-places every piece. Pieces must be moved in the scroll direction's order
    /// (top first when content moves up) so a moved piece never lands on one that has
    /// not moved yet, which WezTerm would stack in no defined order.
    fn place_pieces(&mut self, vt: &mut Vt, top_first: bool) {
        let r = self.region.as_ref().expect("region exists");
        let cell_h = self.grid.geom.cell_h;
        let placement_id = (self.cfg.compat == Compat::Kitty).then_some(PIECE_PLACEMENT);
        let order: Box<dyn Iterator<Item = &Piece>> =
            if top_first { Box::new(r.pieces.iter()) } else { Box::new(r.pieces.iter().rev()) };
        for p in order {
            let y = r.top + p.top;
            vt.move_to(y / cell_h, 0);
            let place = Placement {
                source: Some((0, p.img_y, r.width, p.h)),
                offset: (0, y % cell_h),
                placement_id,
                z: EPHEMERAL_Z,
                ..Default::default()
            };
            kitty::place(&mut vt.buf, p.id, &place);
        }
    }

    pub(super) fn check_region(&self) {
        if let Some(r) = &self.region {
            r.check();
            for ty in 0..self.grid.tiles_y() {
                for tx in 0..self.grid.tiles_x() {
                    let image = self.slots[self.grid.index(tx, ty)].image;
                    if (r.ty0..r.ty1).contains(&ty) {
                        let ok = matches!(image, SlotImage::Scroll | SlotImage::Tile { .. });
                        assert!(ok, "region slot {tx},{ty} shows {image:?}");
                    } else {
                        assert_ne!(image, SlotImage::Scroll, "scroll slot {tx},{ty} outside the region");
                    }
                }
            }
        } else {
            assert!(self.slots.iter().all(|s| s.image != SlotImage::Scroll));
        }
    }
}
