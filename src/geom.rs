//! Rectangles in pixel / cell / tile units, damage expansion and merging.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn right(&self) -> u64 {
        self.x as u64 + self.w as u64
    }

    pub fn bottom(&self) -> u64 {
        self.y as u64 + self.h as u64
    }

    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        if (x0 as u64) < x1 && (y0 as u64) < y1 {
            Some(Rect::new(x0, y0, (x1 - x0 as u64) as u32, (y1 - y0 as u64) as u32))
        } else {
            None
        }
    }

    /// Bounding box of both rectangles.
    pub fn bbox(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.right().max(o.right());
        let y1 = self.bottom().max(o.bottom());
        Rect::new(x0, y0, (x1 - x0 as u64) as u32, (y1 - y0 as u64) as u32)
    }

    /// True if the rectangles overlap or share an edge (corner contact does not count).
    pub fn touches(&self, o: &Rect) -> bool {
        let h_overlap = (self.x as u64) < o.right() && (o.x as u64) < self.right();
        let v_overlap = (self.y as u64) < o.bottom() && (o.y as u64) < self.bottom();
        let h_touch = (self.x as u64) <= o.right() && (o.x as u64) <= self.right();
        let v_touch = (self.y as u64) <= o.bottom() && (o.y as u64) <= self.bottom();
        (h_overlap && v_touch) || (v_overlap && h_touch)
    }

    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && (x as u64) < self.right() && y >= self.y && (y as u64) < self.bottom()
    }
}

/// Terminal cell geometry of the drawing area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellGeom {
    pub cell_w: u32,
    pub cell_h: u32,
    pub cols: u32,
    pub rows: u32,
}

impl CellGeom {
    pub fn px_width(&self) -> u32 {
        self.cols * self.cell_w
    }

    pub fn px_height(&self) -> u32 {
        self.rows * self.cell_h
    }

    /// Expands a pixel rectangle outward to cell boundaries and clips it to the grid.
    /// The result is in cell units; `None` if nothing of it lies on the grid.
    pub fn px_rect_to_cells(&self, r: Rect) -> Option<Rect> {
        if r.is_empty() || self.cell_w == 0 || self.cell_h == 0 {
            return None;
        }
        let cw = self.cell_w as u64;
        let ch = self.cell_h as u64;
        let x0 = r.x as u64 / cw;
        let y0 = r.y as u64 / ch;
        let x1 = r.right().div_ceil(cw).min(self.cols as u64);
        let y1 = r.bottom().div_ceil(ch).min(self.rows as u64);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some(Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
    }
}

/// Merges overlapping or edge-touching rectangles into their bounding box as long as
/// the box wastes at most `max_waste` (0.0..=1.0) of its area on uncovered space.
pub fn merge_rects(mut rects: Vec<Rect>, max_waste: f64) -> Vec<Rect> {
    rects.retain(|r| !r.is_empty());
    loop {
        let mut merged_any = false;
        let mut i = 0;
        while i < rects.len() {
            let mut j = i + 1;
            while j < rects.len() {
                let (a, b) = (rects[i], rects[j]);
                if a.touches(&b) {
                    let bb = a.bbox(&b);
                    let inter = a.intersect(&b).map_or(0, |r| r.area());
                    let covered = a.area() + b.area() - inter;
                    let waste = (bb.area() - covered) as f64 / bb.area() as f64;
                    if waste <= max_waste {
                        rects[i] = bb;
                        rects.swap_remove(j);
                        merged_any = true;
                        continue;
                    }
                }
                j += 1;
            }
            i += 1;
        }
        if !merged_any {
            return rects;
        }
    }
}

/// The cell grid partitioned into tiles of `tile_cols` x `tile_rows` cells.
/// Edge tiles are smaller when the grid is not divisible by the tile size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileGrid {
    pub geom: CellGeom,
    pub tile_cols: u32,
    pub tile_rows: u32,
}

impl TileGrid {
    pub fn new(geom: CellGeom, tile_cols: u32, tile_rows: u32) -> Self {
        assert!(tile_cols > 0 && tile_rows > 0, "tile size must be positive");
        Self { geom, tile_cols, tile_rows }
    }

    pub fn tiles_x(&self) -> u32 {
        self.geom.cols.div_ceil(self.tile_cols)
    }

    pub fn tiles_y(&self) -> u32 {
        self.geom.rows.div_ceil(self.tile_rows)
    }

    pub fn count(&self) -> usize {
        self.tiles_x() as usize * self.tiles_y() as usize
    }

    pub fn index(&self, tx: u32, ty: u32) -> usize {
        ty as usize * self.tiles_x() as usize + tx as usize
    }

    /// Cells covered by a rectangle given in tile units, clipped to the grid.
    pub fn tiles_to_cells(&self, r: Rect) -> Rect {
        let x0 = (r.x * self.tile_cols).min(self.geom.cols);
        let y0 = (r.y * self.tile_rows).min(self.geom.rows);
        let x1 = ((r.x + r.w) * self.tile_cols).min(self.geom.cols);
        let y1 = ((r.y + r.h) * self.tile_rows).min(self.geom.rows);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn tile_cells(&self, tx: u32, ty: u32) -> Rect {
        self.tiles_to_cells(Rect::new(tx, ty, 1, 1))
    }

    pub fn cells_to_px(&self, c: Rect) -> Rect {
        let g = &self.geom;
        Rect::new(c.x * g.cell_w, c.y * g.cell_h, c.w * g.cell_w, c.h * g.cell_h)
    }

    pub fn tile_px(&self, tx: u32, ty: u32) -> Rect {
        self.cells_to_px(self.tile_cells(tx, ty))
    }

    /// Expands a cell rectangle to the tiles that contain it.
    pub fn cells_to_tiles(&self, c: Rect) -> Rect {
        let x0 = c.x / self.tile_cols;
        let y0 = c.y / self.tile_rows;
        let x1 = (c.x + c.w).div_ceil(self.tile_cols).min(self.tiles_x());
        let y1 = (c.y + c.h).div_ceil(self.tile_rows).min(self.tiles_y());
        Rect::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }
}

/// Dirty flags per tile.
#[derive(Clone, Debug)]
pub struct DirtyMap {
    w: u32,
    h: u32,
    bits: Vec<bool>,
    count: usize,
}

impl DirtyMap {
    pub fn new(w: u32, h: u32) -> Self {
        Self { w, h, bits: vec![false; w as usize * h as usize], count: 0 }
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }

    pub fn len(&self) -> usize {
        self.bits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn clear(&mut self) {
        self.bits.fill(false);
        self.count = 0;
    }

    pub fn mark(&mut self, x: u32, y: u32) {
        if x < self.w && y < self.h {
            let i = y as usize * self.w as usize + x as usize;
            if !self.bits[i] {
                self.bits[i] = true;
                self.count += 1;
            }
        }
    }

    pub fn is_marked(&self, x: u32, y: u32) -> bool {
        x < self.w && y < self.h && self.bits[y as usize * self.w as usize + x as usize]
    }

    pub fn mark_rect(&mut self, r: Rect) {
        let x1 = (r.right()).min(self.w as u64) as u32;
        let y1 = (r.bottom()).min(self.h as u64) as u32;
        for y in r.y..y1 {
            for x in r.x..x1 {
                self.mark(x, y);
            }
        }
    }

    pub fn mark_all(&mut self) {
        self.bits.fill(true);
        self.count = self.bits.len();
    }

    pub fn is_dirty(&self, x: u32, y: u32) -> bool {
        x < self.w && y < self.h && self.bits[y as usize * self.w as usize + x as usize]
    }

    /// Dirty positions in row-major order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let w = self.w as usize;
        self.bits.iter().enumerate().filter(|(_, d)| **d).map(move |(i, _)| ((i % w) as u32, (i / w) as u32))
    }

    /// Disjoint rectangles that exactly cover the dirty positions: horizontal runs,
    /// stacked vertically while consecutive rows have an identical span.
    pub fn to_rects(&self) -> Vec<Rect> {
        let mut done = Vec::new();
        let mut open: Vec<Rect> = Vec::new();
        for y in 0..self.h {
            let mut runs = Vec::new();
            let mut x = 0;
            while x < self.w {
                if self.is_dirty(x, y) {
                    let start = x;
                    while x < self.w && self.is_dirty(x, y) {
                        x += 1;
                    }
                    runs.push((start, x - start));
                } else {
                    x += 1;
                }
            }
            let mut next_open = Vec::with_capacity(runs.len());
            for (rx, rw) in runs {
                if let Some(pos) = open.iter().position(|o| o.x == rx && o.w == rw) {
                    let mut o = open.swap_remove(pos);
                    o.h += 1;
                    next_open.push(o);
                } else {
                    next_open.push(Rect::new(rx, y, rw, 1));
                }
            }
            done.append(&mut open);
            open = next_open;
        }
        done.append(&mut open);
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: CellGeom = CellGeom { cell_w: 9, cell_h: 21, cols: 282, rows: 59 };

    #[test]
    fn px_to_cells_exact_boundaries() {
        assert_eq!(G.px_rect_to_cells(Rect::new(9, 21, 9, 21)), Some(Rect::new(1, 1, 1, 1)));
        assert_eq!(G.px_rect_to_cells(Rect::new(0, 0, 18, 42)), Some(Rect::new(0, 0, 2, 2)));
    }

    #[test]
    fn px_to_cells_expands_partial_cells() {
        // x 5..14 touches cols 0 and 1, y 20..23 touches rows 0 and 1.
        assert_eq!(G.px_rect_to_cells(Rect::new(5, 20, 9, 3)), Some(Rect::new(0, 0, 2, 2)));
        // A single pixel maps to its cell.
        assert_eq!(G.px_rect_to_cells(Rect::new(17, 41, 1, 1)), Some(Rect::new(1, 1, 1, 1)));
    }

    #[test]
    fn px_to_cells_clips_to_grid() {
        let r = G.px_rect_to_cells(Rect::new(2530, 1230, 100, 100)).unwrap();
        assert_eq!(r, Rect::new(281, 58, 1, 1));
        assert_eq!(G.px_rect_to_cells(Rect::new(2538, 0, 10, 10)), None);
        assert_eq!(G.px_rect_to_cells(Rect::new(0, 1239, 10, 10)), None);
    }

    #[test]
    fn px_to_cells_rejects_empty_and_huge() {
        assert_eq!(G.px_rect_to_cells(Rect::new(10, 10, 0, 5)), None);
        assert_eq!(G.px_rect_to_cells(Rect::new(10, 10, 5, 0)), None);
        // Values near u32::MAX must not overflow.
        let r = G.px_rect_to_cells(Rect::new(0, 0, u32::MAX, u32::MAX)).unwrap();
        assert_eq!(r, Rect::new(0, 0, 282, 59));
        assert_eq!(G.px_rect_to_cells(Rect::new(u32::MAX, u32::MAX, u32::MAX, u32::MAX)), None);
        let zero = CellGeom { cell_w: 0, cell_h: 0, cols: 1, rows: 1 };
        assert_eq!(zero.px_rect_to_cells(Rect::new(0, 0, 1, 1)), None);
    }

    #[test]
    fn touches_edges_but_not_corners() {
        let a = Rect::new(0, 0, 2, 2);
        assert!(a.touches(&Rect::new(2, 0, 2, 2)));
        assert!(a.touches(&Rect::new(0, 2, 2, 2)));
        assert!(a.touches(&Rect::new(1, 1, 2, 2)));
        assert!(!a.touches(&Rect::new(2, 2, 2, 2)));
        assert!(!a.touches(&Rect::new(3, 0, 1, 1)));
    }

    #[test]
    fn merge_overlapping_and_adjacent() {
        let m = merge_rects(vec![Rect::new(0, 0, 2, 2), Rect::new(1, 0, 2, 2)], 0.0);
        assert_eq!(m, vec![Rect::new(0, 0, 3, 2)]);
        let m = merge_rects(vec![Rect::new(0, 0, 2, 1), Rect::new(0, 1, 2, 1)], 0.0);
        assert_eq!(m, vec![Rect::new(0, 0, 2, 2)]);
        let m = merge_rects(vec![Rect::new(0, 0, 1, 1), Rect::new(5, 5, 1, 1)], 1.0);
        assert_eq!(m.len(), 2, "disjoint rects are never merged");
    }

    #[test]
    fn merge_respects_waste_limit() {
        // L-shape: bbox 2x2 covers 3 cells -> 25% waste.
        let l = vec![Rect::new(0, 0, 2, 1), Rect::new(0, 1, 1, 1)];
        assert_eq!(merge_rects(l.clone(), 0.2).len(), 2);
        assert_eq!(merge_rects(l, 0.25), vec![Rect::new(0, 0, 2, 2)]);
    }

    #[test]
    fn merge_chains_and_drops_empty() {
        let m = merge_rects(
            vec![Rect::new(0, 0, 1, 1), Rect::new(0, 0, 0, 5), Rect::new(1, 0, 1, 1), Rect::new(2, 0, 1, 1)],
            0.0,
        );
        assert_eq!(m, vec![Rect::new(0, 0, 3, 1)]);
    }

    #[test]
    fn tile_grid_with_remainder() {
        let g = CellGeom { cell_w: 9, cell_h: 21, cols: 10, rows: 5 };
        let t = TileGrid::new(g, 4, 2);
        assert_eq!((t.tiles_x(), t.tiles_y(), t.count()), (3, 3, 9));
        assert_eq!(t.tile_cells(0, 0), Rect::new(0, 0, 4, 2));
        assert_eq!(t.tile_cells(2, 2), Rect::new(8, 4, 2, 1));
        assert_eq!(t.tile_px(2, 2), Rect::new(72, 84, 18, 21));
        assert_eq!(t.cells_to_tiles(Rect::new(3, 1, 2, 1)), Rect::new(0, 0, 2, 1));
        assert_eq!(t.cells_to_tiles(Rect::new(9, 4, 1, 1)), Rect::new(2, 2, 1, 1));
        assert_eq!(t.tiles_to_cells(Rect::new(1, 1, 2, 2)), Rect::new(4, 2, 6, 3));
    }

    #[test]
    fn tiles_partition_the_grid_exactly() {
        let g = CellGeom { cell_w: 9, cell_h: 21, cols: 13, rows: 7 };
        for (tw, th) in [(1, 1), (2, 3), (5, 2), (13, 7), (20, 20)] {
            let t = TileGrid::new(g, tw, th);
            let mut hits = vec![0u32; (g.cols * g.rows) as usize];
            for ty in 0..t.tiles_y() {
                for tx in 0..t.tiles_x() {
                    let c = t.tile_cells(tx, ty);
                    assert!(!c.is_empty());
                    for y in c.y..c.y + c.h {
                        for x in c.x..c.x + c.w {
                            hits[(y * g.cols + x) as usize] += 1;
                        }
                    }
                }
            }
            assert!(hits.iter().all(|&h| h == 1), "tile {tw}x{th} must cover each cell once");
        }
    }

    #[test]
    fn dirty_map_rects_cover_exactly() {
        let mut d = DirtyMap::new(8, 6);
        d.mark_rect(Rect::new(1, 1, 3, 2));
        d.mark_rect(Rect::new(2, 2, 4, 3));
        d.mark(7, 0);
        d.mark(7, 0);
        assert_eq!(d.count(), 6 + 12 - 2 + 1);
        let rects = d.to_rects();
        let mut cover = DirtyMap::new(8, 6);
        let mut total = 0;
        for r in &rects {
            for y in r.y..r.y + r.h {
                for x in r.x..r.x + r.w {
                    assert!(!cover.is_dirty(x, y), "rects must be disjoint");
                    cover.mark(x, y);
                    total += 1;
                }
            }
        }
        assert_eq!(total, d.count());
        assert!(d.iter().all(|(x, y)| cover.is_dirty(x, y)));
    }

    #[test]
    fn dirty_map_stacks_identical_spans() {
        let mut d = DirtyMap::new(4, 4);
        d.mark_all();
        assert_eq!(d.to_rects(), vec![Rect::new(0, 0, 4, 4)]);
        d.clear();
        assert_eq!(d.count(), 0);
        assert!(d.to_rects().is_empty());
        d.mark_rect(Rect::new(3, 3, 10, 10));
        assert_eq!(d.to_rects(), vec![Rect::new(3, 3, 1, 1)]);
    }
}
