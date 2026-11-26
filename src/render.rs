//! Turns framebuffer changes into Kitty graphics / VT output (Phase 1-4).
//!
//! The terminal is modelled per tile slot. Two kinds of images exist:
//! - cached tiles: one tile-sized image per distinct content, placed at any number of
//!   slots and kept in an LRU (Phase 3/4);
//! - ephemeral images: arbitrary tile-aligned rectangles that are deleted once no slot
//!   shows them any more (Phase 1/2 frames and damage rects, Phase 4 half-res images).
//!
//! Kitty stacks same-z images by image id, not by placement order. Placements are
//! therefore never allowed to overlap except "newer ephemeral over older ephemeral"
//! (ids are monotonic) and, in Phase 4, "tile (z=1) over ephemeral (z=0)".
//! [`Compat::WezTerm`] cannot rely on that order and erases the cells instead.

use crate::frame::{self, FrameBuf};
use crate::geom::{merge_rects, DirtyMap, Rect, TileGrid};
use crate::kitty::{self, Delete, Encoding, Image, Placement};
use crate::lru::Lru;
use crate::par;
use crate::png;
use crate::vt::Vt;
use crate::zlib::Deflater;

mod region;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Whole frame every time.
    Full = 1,
    /// Only damaged rectangles.
    Damage = 2,
    /// Tile cache with hashing and LRU.
    Tiles = 3,
    /// Tile cache plus half resolution while in motion.
    Progressive = 4,
}

impl Phase {
    pub fn from_number(n: u32) -> Option<Self> {
        Some(match n {
            1 => Self::Full,
            2 => Self::Damage,
            3 => Self::Tiles,
            4 => Self::Progressive,
            _ => return None,
        })
    }
}

/// How the terminal implements image placements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compat {
    /// kitty and Ghostty: placements float above the grid, positional deletes work and
    /// same-z images stack by image id.
    Kitty,
    /// WezTerm attaches images to cells: only `d=a` and `d=i` deletes are implemented,
    /// placing an image without a placement id drops its other placements, same-z
    /// images stack in no defined order, erasing a cell (ECH) drops its images that
    /// have no placement id, and placing an image over a cell duplicates the cell's
    /// placement-id images (so that must never happen).
    WezTerm,
}

impl Compat {
    /// Picks the model from the XTVERSION answer; unknown terminals get the spec's.
    pub fn detect(terminal_name: Option<&str>) -> Self {
        match terminal_name {
            Some(n) if n.starts_with("WezTerm") => Self::WezTerm,
            _ => Self::Kitty,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::WezTerm => "wezterm",
        }
    }
}

/// Whether large images should go as PNG, from the XTVERSION answer: Ghostty 1.3's zlib
/// inflater can crash on them (see [`crate::png`]). PNG costs nothing measurable, so
/// every Ghostty version gets it.
pub fn needs_png(terminal_name: Option<&str>) -> bool {
    terminal_name.is_some_and(|n| n.to_ascii_lowercase().starts_with("ghostty"))
}

#[derive(Clone, Debug)]
pub struct RenderConfig {
    pub phase: Phase,
    pub compat: Compat,
    /// Budget for cached tile images, estimated at 4 bytes per pixel as terminals store RGBA.
    pub cache_bytes: u64,
    /// zlib level for frames, damage rectangles and half-res images.
    pub zlib_level: u32,
    /// zlib level for cached tiles; tiny inputs gain little from higher levels.
    pub tile_zlib_level: u32,
    /// Paint uniform tiles as terminal cell backgrounds instead of images.
    pub solid_fill: bool,
    /// Phase 4: fraction of dirty tiles in a frame that counts as motion.
    pub lowres_threshold: f64,
    /// Phase 4: how long a half-res tile must stay unchanged before refinement.
    pub refine_delay: Duration,
    pub first_image_id: u32,
    /// Worker threads for hashing and compressing large images.
    pub threads: usize,
    /// Phase 4: draw scrolling by moving already sent pixels.
    pub scroll_copy: bool,
    /// Send images over [`PNG_MIN_BYTES`] as PNG rather than zlib-compressed RGB, for
    /// Ghostty 1.3 whose zlib inflater can crash on them (see [`crate::png`]).
    pub large_png: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            phase: Phase::Progressive,
            compat: Compat::Kitty,
            cache_bytes: 128 << 20,
            zlib_level: 6,
            tile_zlib_level: 1,
            solid_fill: true,
            lowres_threshold: 0.05,
            refine_delay: Duration::from_millis(200),
            first_image_id: 1,
            threads: 1,
            scroll_copy: true,
            large_png: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Tiles marked dirty by compositor damage.
    pub dirty_tiles: u64,
    /// Tiles whose displayed content actually changed.
    pub changed_tiles: u64,
    /// Image transmissions of any kind.
    pub images_sent: u64,
    /// Tile cache misses, i.e. tiles transmitted.
    pub tiles_sent: u64,
    /// Tiles displayed from the cache without retransmission.
    pub cache_hits: u64,
    pub solid_fills: u64,
    pub lowres_images: u64,
    pub refined_tiles: u64,
    pub evictions: u64,
    /// Image payload after compression, before base64.
    pub payload_bytes: u64,
    /// Scroll-copy shifts, i.e. frames drawn by moving already sent pixels.
    pub scroll_shifts: u64,
}

impl FrameStats {
    pub fn add(&mut self, o: &FrameStats) {
        self.dirty_tiles += o.dirty_tiles;
        self.changed_tiles += o.changed_tiles;
        self.images_sent += o.images_sent;
        self.tiles_sent += o.tiles_sent;
        self.cache_hits += o.cache_hits;
        self.solid_fills += o.solid_fills;
        self.lowres_images += o.lowres_images;
        self.refined_tiles += o.refined_tiles;
        self.evictions += o.evictions;
        self.payload_bytes += o.payload_bytes;
        self.scroll_shifts += o.scroll_shifts;
    }
}

const PHASE4_TILE_Z: i32 = 1;
const EPHEMERAL_Z: i32 = 0;
/// Phase 4: captured frames a half-res tile must survive unchanged before it is
/// refined while other parts of the screen keep moving.
const QUIET_FRAMES: u8 = 2;
/// Approximate bytes of a cursor move plus a per-tile delete / a placement / a row delete,
/// used to choose between per-tile and per-row deletion.
const TILE_DELETE_COST: usize = 19;
const TILE_PLACE_COST: usize = 25;
const ROW_DELETE_COST: usize = 22;
/// Live ephemeral area (in screens) that triggers a compacting full redraw.
const MAX_EPHEMERAL_SCREENS: u64 = 4;
const MERGE_WASTE: f64 = 0.3;
/// Tiles per worker below which hashing stays on one thread.
const PAR_MIN_TILES: usize = 256;
/// Raw size above which [`RenderConfig::large_png`] sends PNG. Ghostty 1.3's inflater can
/// only fail once its 64 KiB window wraps; half of that leaves a margin.
pub const PNG_MIN_BYTES: usize = 32 << 10;

/// Encodes RGB pixels for transmission.
struct Packer {
    deflater: Deflater,
    rows: Vec<u8>,
    z: Vec<u8>,
}

impl Packer {
    fn new(level: u32) -> Self {
        Self { deflater: Deflater::new(level), rows: Vec::new(), z: Vec::new() }
    }

    /// Writes the payload for `raw` (`w`x`h` RGB) into `out`. [`Encoding::Raw`] leaves `out`
    /// alone: send `raw` itself (compression is off or would not shrink it).
    fn pack(&mut self, raw: &[u8], w: u32, h: u32, threads: usize, large_png: bool, out: &mut Vec<u8>) -> Encoding {
        if large_png && raw.len() > PNG_MIN_BYTES {
            png::scanlines(raw, w, &mut self.rows);
            self.deflater.compress(&self.rows, &mut self.z, threads);
            png::encode(w, h, &self.z, out);
            return Encoding::Png;
        }
        if self.deflater.level() == 0 || raw.is_empty() {
            return Encoding::Raw;
        }
        self.deflater.compress(raw, out, threads);
        if out.len() < raw.len() {
            Encoding::Zlib
        } else {
            Encoding::Raw
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotImage {
    None,
    Tile {
        key: u128,
        id: u32,
    },
    /// The topmost ephemeral image covering the slot.
    Ephemeral(u32),
    /// Drawn by the scroll region's pieces.
    Scroll,
}

/// Identity of a tile's full-resolution pixels, independent of how it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    Unknown,
    Solid([u8; 3]),
    Pixels(u128),
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    /// What the slot currently represents (a half-res image represents its source).
    shown: Content,
    /// Background painted in the text layer; `None` is the terminal default.
    bg: Option<[u8; 3]>,
    image: SlotImage,
    /// Live ephemeral images physically covering the slot (visible or not).
    cover: u32,
    /// Set while the slot shows half-resolution content awaiting refinement.
    lowres_since: Option<Instant>,
    /// Captured frames since `lowres_since` in which the slot did not change.
    quiet_frames: u8,
}

const BLANK: Slot =
    Slot { shown: Content::Unknown, bg: None, image: SlotImage::None, cover: 0, lowres_since: None, quiet_frames: 0 };

/// An image about to be transmitted from the renderer's pixel buffers.
struct Outgoing {
    id: u32,
    w: u32,
    h: u32,
    /// Pixels are in the half-resolution buffer.
    half: bool,
    /// Compress with the tile level.
    tile: bool,
}

#[derive(Debug)]
struct Ephemeral {
    rect: Rect,
    /// Slots where this image is the topmost content.
    visible: u64,
}

pub struct Renderer {
    cfg: RenderConfig,
    grid: TileGrid,
    slots: Vec<Slot>,
    ephemerals: HashMap<u32, Ephemeral>,
    ephemeral_area: u64,
    maybe_dead: Vec<u32>,
    cache: Lru<u128>,
    next_id: u32,
    tile_z: i32,
    lowres_pending: usize,
    compressor: Packer,
    tile_compressor: Packer,
    px: Vec<u8>,
    half: Vec<u8>,
    z: Vec<u8>,
    note: Option<String>,
    region: Option<region::Region>,
    /// Phase 4 scroll detection: `scroll::BANDS` hashes per pixel row of the last frame.
    row_hashes: Vec<u128>,
    /// Consecutive frames with a detected scroll while no region exists.
    scroll_streak: u32,
}

impl Renderer {
    pub fn new(mut cfg: RenderConfig, grid: TileGrid) -> Self {
        // Every on-screen tile is pinned, so the budget must at least hold one screen
        // (edge tiles are clipped, so this is the screen, not count x nominal tile size).
        let min_budget = grid.geom.px_width() as u64 * grid.geom.px_height() as u64 * 4;
        let mut note = None;
        if cfg.phase >= Phase::Tiles && cfg.cache_bytes < min_budget {
            note = Some(format!(
                "cache budget {} MiB is below one screen of tiles; raised to {} MiB",
                cfg.cache_bytes >> 20,
                min_budget.div_ceil(1 << 20)
            ));
            cfg.cache_bytes = min_budget;
        }
        let level = cfg.zlib_level.min(9);
        let tile_level = cfg.tile_zlib_level.min(9);
        Self {
            tile_z: if cfg.phase == Phase::Progressive { PHASE4_TILE_Z } else { 0 },
            next_id: cfg.first_image_id.max(1),
            compressor: Packer::new(level),
            tile_compressor: Packer::new(tile_level),
            cfg,
            grid,
            slots: vec![BLANK; grid.count()],
            ephemerals: HashMap::new(),
            ephemeral_area: 0,
            maybe_dead: Vec::new(),
            cache: Lru::new(),
            lowres_pending: 0,
            px: Vec::new(),
            half: Vec::new(),
            z: Vec::new(),
            note,
            region: None,
            row_hashes: Vec::new(),
            scroll_streak: 0,
        }
    }

    pub fn grid(&self) -> &TileGrid {
        &self.grid
    }

    pub fn config(&self) -> &RenderConfig {
        &self.cfg
    }

    /// A one-off configuration adjustment worth logging.
    pub fn take_note(&mut self) -> Option<String> {
        self.note.take()
    }

    pub fn cached_tiles(&self) -> usize {
        self.cache.len()
    }

    pub fn cache_cost(&self) -> u64 {
        self.cache.total_cost()
    }

    pub fn live_ephemerals(&self) -> usize {
        self.ephemerals.len()
    }

    /// Deletes every image this renderer may have created (on reset and exit).
    pub fn delete_all_images(&self, vt: &mut Vt) {
        kitty::delete(&mut vt.buf, Delete::AllVisible { free: true });
        let last = self.next_id.saturating_sub(1);
        // WezTerm's d=A already frees every image and it ignores range deletes.
        if last >= self.cfg.first_image_id && self.cfg.compat == Compat::Kitty {
            kitty::delete(&mut vt.buf, Delete::Range { from: self.cfg.first_image_id, to: last, free: true });
        }
    }

    /// Clears the screen and all terminal-side state, e.g. after a resize.
    pub fn reset(&mut self, vt: &mut Vt) {
        self.delete_all_images(vt);
        vt.clear_screen();
        vt.forget_state();
        self.slots.fill(BLANK);
        self.ephemerals.clear();
        self.ephemeral_area = 0;
        self.maybe_dead.clear();
        self.cache.drain();
        self.lowres_pending = 0;
        self.next_id = self.cfg.first_image_id.max(1);
        self.region = None;
        self.row_hashes.clear();
        self.scroll_streak = 0;
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn ids_exhausted(&self) -> bool {
        // Leave room for one frame worth of allocations.
        self.next_id as u64 + 2 * self.grid.count() as u64 + 16 > u32::MAX as u64
    }

    #[doc(hidden)]
    pub fn set_next_id_for_test(&mut self, id: u32) {
        self.next_id = id;
    }

    /// Renders the tiles marked in `dirty` (tile units) from `fb`.
    pub fn render(&mut self, fb: &FrameBuf, dirty: &DirtyMap, now: Instant, vt: &mut Vt) -> FrameStats {
        let mut st = FrameStats { dirty_tiles: dirty.count() as u64, ..Default::default() };
        if self.ids_exhausted() {
            self.reset(vt);
            let mut all = DirtyMap::new(self.grid.tiles_x(), self.grid.tiles_y());
            all.mark_all();
            return self.render(fb, &all, now, vt);
        }
        let all = Rect::new(0, 0, self.grid.tiles_x(), self.grid.tiles_y());
        match self.cfg.phase {
            Phase::Full => self.place_ephemeral(fb, all, false, now, vt, &mut st),
            Phase::Damage => {
                for r in merge_rects(dirty.to_rects(), MERGE_WASTE) {
                    self.place_ephemeral(fb, r, false, now, vt, &mut st);
                }
            }
            Phase::Tiles => {
                let changed = self.changed_tiles(fb, dirty);
                self.update_tiles(fb, &changed, vt, &mut st);
            }
            Phase::Progressive => {
                let mut dirty = dirty.clone();
                if self.cfg.scroll_copy {
                    self.scroll_step(fb, &mut dirty, vt, &mut st);
                }
                let changed = self.changed_tiles(fb, &dirty);
                let motion =
                    !changed.is_empty() && changed.len() as f64 >= self.cfg.lowres_threshold * self.grid.count() as f64;
                if motion {
                    let mut map = DirtyMap::new(self.grid.tiles_x(), self.grid.tiles_y());
                    for &(x, y, _) in &changed {
                        map.mark(x, y);
                    }
                    for r in merge_rects(map.to_rects(), MERGE_WASTE) {
                        for part in self.outside_region(r) {
                            self.place_ephemeral(fb, part, true, now, vt, &mut st);
                        }
                    }
                    for (x, y, c) in changed {
                        let i = self.grid.index(x, y);
                        self.slots[i].shown = c;
                    }
                } else {
                    self.update_tiles(fb, &changed, vt, &mut st);
                }
                for s in &mut self.slots {
                    if s.lowres_since.is_some_and(|t| t != now) {
                        s.quiet_frames = s.quiet_frames.saturating_add(1);
                    }
                }
                self.refine_due(fb, now, false, vt, &mut st);
            }
        }
        self.finish(fb, now, vt, &mut st);
        st
    }

    fn content_of(&mut self, fb: &FrameBuf, tx: u32, ty: u32) -> Content {
        tile_content(fb, self.grid.tile_px(tx, ty), &mut self.px)
    }

    /// Dirty tiles whose pixels differ from what the slot represents; the scroll region
    /// is handled separately.
    fn changed_tiles(&mut self, fb: &FrameBuf, dirty: &DirtyMap) -> Vec<(u32, u32, Content)> {
        let tiles: Vec<(u32, u32)> = dirty.iter().filter(|&(_, y)| !self.in_region(y)).collect();
        let grid = self.grid;
        let contents = par::map_ranges(tiles.len(), self.cfg.threads, PAR_MIN_TILES, |r| {
            let mut buf = Vec::new();
            tiles[r].iter().map(|&(x, y)| tile_content(fb, grid.tile_px(x, y), &mut buf)).collect::<Vec<_>>()
        });
        tiles
            .into_iter()
            .zip(contents.into_iter().flatten())
            .filter(|&((x, y), c)| c != self.slots[self.grid.index(x, y)].shown)
            .map(|((x, y), c)| (x, y, c))
            .collect()
    }

    /// Phase 4: when half-res tiles should next be refined.
    pub fn next_refine_at(&self) -> Option<Instant> {
        if self.lowres_pending == 0 {
            return None;
        }
        self.slots.iter().filter_map(|s| s.lowres_since).min().map(|t| t + self.cfg.refine_delay)
    }

    /// Phase 4: refines every half-res tile older than the refine delay. Call it only
    /// when the compositor has reported no damage for that long; while frames keep
    /// arriving, [`Renderer::render`] refines tiles that stayed unchanged instead.
    /// (Time alone is not enough: on a slow link frames are far apart even in motion.)
    pub fn refine(&mut self, fb: &FrameBuf, now: Instant, vt: &mut Vt) -> FrameStats {
        let mut st = FrameStats::default();
        self.refine_due(fb, now, true, vt, &mut st);
        self.finish(fb, now, vt, &mut st);
        st
    }

    fn finish(&mut self, fb: &FrameBuf, now: Instant, vt: &mut Vt, st: &mut FrameStats) {
        self.collect_ephemerals(vt);
        if self.ephemeral_area > MAX_EPHEMERAL_SCREENS * self.grid.count() as u64 {
            // Many mostly hidden images: replace everything with one frame. Phase 4 sends
            // it at half resolution, so it costs a quarter and refinement retires it
            // (a full-resolution one would linger under every slot not redrawn since).
            crate::log_debug!("compacting {} live ephemeral images", self.ephemerals.len());
            self.dissolve(None, vt);
            let all = Rect::new(0, 0, self.grid.tiles_x(), self.grid.tiles_y());
            self.place_ephemeral(fb, all, self.cfg.phase == Phase::Progressive, now, vt, st);
            self.collect_ephemerals(vt);
        }
        self.evict(vt, st);
    }

    fn refine_due(&mut self, fb: &FrameBuf, now: Instant, screen_static: bool, vt: &mut Vt, st: &mut FrameStats) {
        if self.lowres_pending == 0 {
            return;
        }
        let tx = self.grid.tiles_x() as usize;
        let mut by_image: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (i, s) in self.slots.iter().enumerate() {
            if let (Some(t), SlotImage::Ephemeral(e)) = (s.lowres_since, s.image) {
                let quiet = screen_static || s.quiet_frames >= QUIET_FRAMES;
                if quiet && now.saturating_duration_since(t) >= self.cfg.refine_delay {
                    by_image.entry(e).or_default().push(i);
                }
            }
        }
        for (e, idxs) in by_image {
            if self.ephemerals.get(&e).is_some_and(|eph| eph.visible == idxs.len() as u64) {
                // The whole image goes: delete it first so uniform tiles can become cell fills.
                self.remove_ephemeral(e, vt);
                for &i in &idxs {
                    self.slots[i].image = SlotImage::None;
                }
            }
            for i in idxs {
                let (x, y) = ((i % tx) as u32, (i / tx) as u32);
                let c = self.content_of(fb, x, y);
                self.update_tile(fb, x, y, c, vt, st);
                st.refined_tiles += 1;
            }
        }
    }

    fn set_lowres(&mut self, i: usize, since: Option<Instant>) {
        match (self.slots[i].lowres_since.is_some(), since.is_some()) {
            (false, true) => self.lowres_pending += 1,
            (true, false) => self.lowres_pending -= 1,
            _ => {}
        }
        self.slots[i].lowres_since = since;
        self.slots[i].quiet_frames = 0;
    }

    /// Removes the slot's image from the terminal model, emitting deletes as needed.
    fn retire_image(&mut self, i: usize, cells: Rect, vt: &mut Vt) {
        match self.slots[i].image {
            SlotImage::None | SlotImage::Scroll => {}
            SlotImage::Tile { key, id } => {
                if self.cfg.compat == Compat::WezTerm {
                    kitty::delete(&mut vt.buf, Delete::Placement { id, placement_id: slot_placement_id(i) });
                } else if self.slots[i].cover == 0 {
                    vt.move_to(cells.y, cells.x);
                    kitty::delete(&mut vt.buf, Delete::AtCursor);
                } else {
                    kitty::delete(&mut vt.buf, Delete::AtCellZ { col: cells.x, row: cells.y, z: self.tile_z });
                }
                self.cache.unpin(&key);
            }
            SlotImage::Ephemeral(e) => {
                let eph = self.ephemerals.get_mut(&e).expect("slot references unknown ephemeral");
                eph.visible -= 1;
                if eph.visible == 0 {
                    self.maybe_dead.push(e);
                }
            }
        }
        self.slots[i].image = SlotImage::None;
    }

    fn remove_ephemeral(&mut self, e: u32, vt: &mut Vt) {
        let Some(eph) = self.ephemerals.remove(&e) else { return };
        kitty::delete(&mut vt.buf, Delete::Image { id: e });
        for y in eph.rect.y..eph.rect.y + eph.rect.h {
            for x in eph.rect.x..eph.rect.x + eph.rect.w {
                let i = self.grid.index(x, y);
                self.slots[i].cover -= 1;
            }
        }
        self.ephemeral_area -= eph.rect.area();
    }

    fn collect_ephemerals(&mut self, vt: &mut Vt) {
        let mut dead = std::mem::take(&mut self.maybe_dead);
        dead.sort_unstable();
        dead.dedup();
        for e in dead {
            if self.ephemerals.get(&e).is_some_and(|eph| eph.visible == 0) {
                self.remove_ephemeral(e, vt);
            }
        }
    }

    fn evict(&mut self, vt: &mut Vt, st: &mut FrameStats) {
        for (_, id) in self.cache.evict_to(self.cfg.cache_bytes) {
            kitty::delete(&mut vt.buf, Delete::Image { id });
            st.evictions += 1;
        }
    }

    /// Transmits the pixels in `self.half` (half-res) or `self.px` as a new image at the cursor.
    fn send_image(&mut self, img: Outgoing, place: &Placement, vt: &mut Vt, st: &mut FrameStats) {
        let raw = std::mem::take(if img.half { &mut self.half } else { &mut self.px });
        let packer = if img.tile { &mut self.tile_compressor } else { &mut self.compressor };
        let encoding = packer.pack(&raw, img.w, img.h, self.cfg.threads, self.cfg.large_png, &mut self.z);
        let data: &[u8] = if encoding == Encoding::Raw { &raw } else { &self.z };
        kitty::transmit(&mut vt.buf, &Image { id: img.id, width: img.w, height: img.h, data, encoding }, Some(place));
        st.images_sent += 1;
        st.payload_bytes += data.len() as u64;
        if img.half {
            self.half = raw;
        } else {
            self.px = raw;
        }
    }

    fn place_ephemeral(
        &mut self,
        fb: &FrameBuf,
        r: Rect,
        lowres: bool,
        now: Instant,
        vt: &mut Vt,
        st: &mut FrameStats,
    ) {
        if r.is_empty() {
            return;
        }
        debug_assert!((r.y..r.y + r.h).all(|y| !self.in_region(y)), "ephemeral over the scroll region");
        let cells = self.grid.tiles_to_cells(r);
        let px = self.grid.cells_to_px(cells);
        let whole_screen = r == Rect::new(0, 0, self.grid.tiles_x(), self.grid.tiles_y());
        let wezterm = self.cfg.compat == Compat::WezTerm;
        let bulk = whole_screen
            .then(|| {
                let has_tiles = self.slots.iter().any(|s| matches!(s.image, SlotImage::Tile { .. }));
                match self.cfg.compat {
                    Compat::Kitty => {
                        (has_tiles && self.tile_z != EPHEMERAL_Z).then_some(Delete::ZIndex { z: self.tile_z })
                    }
                    // Everything is replaced, older ephemerals included.
                    Compat::WezTerm => {
                        (has_tiles || !self.ephemerals.is_empty()).then_some(Delete::AllVisible { free: false })
                    }
                }
            })
            .flatten();
        if let Some(d) = bulk {
            // One bulk delete instead of one per tile.
            kitty::delete(&mut vt.buf, d);
            for s in &mut self.slots {
                if let SlotImage::Tile { key, .. } = s.image {
                    self.cache.unpin(&key);
                    s.image = SlotImage::None;
                }
            }
        } else if wezterm {
            self.erase_covered(r, vt);
        }
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let i = self.grid.index(x, y);
                self.retire_image(i, self.grid.tile_cells(x, y), vt);
            }
        }
        let (w, h) = if lowres {
            let mut full = std::mem::take(&mut self.px);
            let size = fb.extract_half(px, &mut full, &mut self.half);
            self.px = full;
            size
        } else {
            fb.extract(px, &mut self.px);
            (px.w, px.h)
        };
        let id = self.alloc_id();
        let place = if lowres {
            Placement { cols: Some(cells.w), rows: Some(cells.h), ..Default::default() }
        } else {
            Placement::default()
        };
        vt.move_to(cells.y, cells.x);
        self.send_image(Outgoing { id, w, h, half: lowres, tile: false }, &place, vt, st);
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let i = self.grid.index(x, y);
                self.slots[i].image = SlotImage::Ephemeral(id);
                self.slots[i].cover += 1;
                self.set_lowres(i, lowres.then_some(now));
                if !lowres {
                    // Phase 1/2 do not track content; half-res keeps representing it.
                    self.slots[i].shown = Content::Unknown;
                }
            }
        }
        self.ephemerals.insert(id, Ephemeral { rect: r, visible: r.area() });
        self.ephemeral_area += r.area();
        st.changed_tiles += r.area();
        if lowres {
            st.lowres_images += 1;
        }
    }

    /// Parts of `r` (tile units) above and below the scroll region.
    fn outside_region(&self, r: Rect) -> Vec<Rect> {
        let Some(reg) = &self.region else { return vec![r] };
        let (y0, y1) = (r.y, r.y + r.h);
        [(y0, y1.min(reg.ty0)), (y0.max(reg.ty1), y1)]
            .into_iter()
            .filter(|(a, b)| a < b)
            .map(|(a, b)| Rect::new(r.x, a, r.w, b - a))
            .collect()
    }

    /// WezTerm: erases the cells of tiles in `r` (tile units) that ephemerals cover, so
    /// the image placed next never shares a cell with a same-z image. Ephemerals carry
    /// no placement id, which is what lets ECH drop them.
    fn erase_covered(&mut self, r: Rect, vt: &mut Vt) {
        for y in r.y..r.y + r.h {
            let mut x = r.x;
            while x < r.x + r.w {
                if self.slots[self.grid.index(x, y)].cover == 0 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < r.x + r.w && self.slots[self.grid.index(x, y)].cover > 0 {
                    self.slots[self.grid.index(x, y)].bg = None;
                    x += 1;
                }
                let cells = self.grid.tiles_to_cells(Rect::new(start, y, x - start, 1));
                for row in cells.y..cells.y + cells.h {
                    vt.erase(row, cells.x, cells.w);
                }
            }
        }
    }

    /// Updates changed tiles (row-major), deleting whole rows at once where cheaper.
    fn update_tiles(&mut self, fb: &FrameBuf, changed: &[(u32, u32, Content)], vt: &mut Vt, st: &mut FrameStats) {
        let mut i = 0;
        while i < changed.len() {
            let y = changed[i].1;
            let end = changed[i..].iter().position(|c| c.1 != y).map_or(changed.len(), |k| i + k);
            self.bulk_delete_row(y, &changed[i..end], vt);
            for &(x, y, c) in &changed[i..end] {
                self.update_tile(fb, x, y, c, vt, st);
            }
            i = end;
        }
    }

    /// Replaces per-tile deletes in tile row `y` by one row delete plus re-placing the
    /// unchanged tiles, when that is shorter and no ephemeral image shares the row.
    fn bulk_delete_row(&mut self, y: u32, row_changes: &[(u32, u32, Content)], vt: &mut Vt) {
        if self.cfg.compat == Compat::WezTerm {
            return;
        }
        let tx = self.grid.tiles_x();
        let row = self.grid.index(0, y)..self.grid.index(0, y) + tx as usize;
        if self.slots[row.clone()].iter().any(|s| s.cover > 0) {
            return;
        }
        let is_tile = |s: &Slot| matches!(s.image, SlotImage::Tile { .. });
        let deletes = row_changes.iter().filter(|c| is_tile(&self.slots[row.start + c.0 as usize])).count();
        let tiles = self.slots[row.clone()].iter().filter(|s| is_tile(s)).count();
        let keeps = tiles - deletes;
        if ROW_DELETE_COST + keeps * TILE_PLACE_COST >= deletes * TILE_DELETE_COST {
            return;
        }
        let cell_row = self.grid.tile_cells(0, y).y;
        kitty::delete(&mut vt.buf, Delete::Row { row: cell_row });
        let mut changed = vec![false; tx as usize];
        row_changes.iter().for_each(|c| changed[c.0 as usize] = true);
        let place = Placement { z: self.tile_z, ..Default::default() };
        for x in 0..tx {
            let i = row.start + x as usize;
            let SlotImage::Tile { key, id } = self.slots[i].image else { continue };
            if changed[x as usize] {
                self.cache.unpin(&key);
                self.slots[i].image = SlotImage::None;
            } else {
                let cells = self.grid.tile_cells(x, y);
                vt.move_to(cells.y, cells.x);
                kitty::place(&mut vt.buf, id, &place);
            }
        }
    }

    /// Makes slot (tx, ty) show `content`, which must be the current pixels of `fb`.
    fn update_tile(&mut self, fb: &FrameBuf, tx: u32, ty: u32, content: Content, vt: &mut Vt, st: &mut FrameStats) {
        let i = self.grid.index(tx, ty);
        let cells = self.grid.tile_cells(tx, ty);
        let px = self.grid.cells_to_px(cells);
        self.set_lowres(i, None);
        self.slots[i].shown = content;
        let slot = self.slots[i];
        if let (true, 0, Content::Solid(c)) = (self.cfg.solid_fill, slot.cover, content) {
            if slot.image == SlotImage::None && slot.bg == Some(c) {
                return;
            }
            self.retire_image(i, cells, vt);
            if slot.bg != Some(c) {
                for row in cells.y..cells.y + cells.h {
                    vt.fill(row, cells.x, cells.w, c);
                }
                self.slots[i].bg = Some(c);
            }
            st.solid_fills += 1;
            st.changed_tiles += 1;
            return;
        }
        fb.extract(px, &mut self.px);
        let key = match content {
            Content::Pixels(k) => k,
            _ => tile_key(px.w, px.h, &self.px),
        };
        if matches!(slot.image, SlotImage::Tile { key: k, .. } if k == key) {
            return;
        }
        self.retire_image(i, cells, vt);
        let place = match self.cfg.compat {
            Compat::Kitty => Placement { z: self.tile_z, ..Default::default() },
            Compat::WezTerm => {
                if self.tile_z == EPHEMERAL_Z {
                    self.erase_covered(Rect::new(tx, ty, 1, 1), vt);
                }
                Placement { z: self.tile_z, placement_id: Some(slot_placement_id(i)), ..Default::default() }
            }
        };
        vt.move_to(cells.y, cells.x);
        let id = match self.cache.get(&key) {
            Some(id) => {
                kitty::place(&mut vt.buf, id, &place);
                st.cache_hits += 1;
                id
            }
            None => {
                let id = self.alloc_id();
                self.send_image(Outgoing { id, w: px.w, h: px.h, half: false, tile: true }, &place, vt, st);
                self.cache.insert(key, id, px.area() * 4);
                st.tiles_sent += 1;
                id
            }
        };
        self.cache.pin(&key);
        self.slots[i].image = SlotImage::Tile { key, id };
        st.changed_tiles += 1;
    }

    /// Consistency check of the internal model (used by tests).
    #[doc(hidden)]
    pub fn check_invariants(&self) {
        let mut visible: HashMap<u32, u64> = HashMap::new();
        let mut cover = vec![0u32; self.slots.len()];
        for (e, eph) in &self.ephemerals {
            assert!(eph.visible > 0, "dead ephemeral {e} not collected");
            for y in eph.rect.y..eph.rect.y + eph.rect.h {
                for x in eph.rect.x..eph.rect.x + eph.rect.w {
                    cover[self.grid.index(x, y)] += 1;
                }
            }
        }
        for y in 0..self.grid.tiles_y() {
            if self.in_region(y) {
                (0..self.grid.tiles_x()).for_each(|x| cover[self.grid.index(x, y)] += 1);
            }
        }
        self.check_region();
        let mut pins: HashMap<u128, u32> = HashMap::new();
        let mut pending = 0;
        for (i, s) in self.slots.iter().enumerate() {
            assert_eq!(s.cover, cover[i], "cover count of slot {i}");
            match s.image {
                SlotImage::Ephemeral(e) => *visible.entry(e).or_default() += 1,
                SlotImage::Tile { key, .. } => *pins.entry(key).or_default() += 1,
                SlotImage::None | SlotImage::Scroll => {}
            }
            pending += s.lowres_since.is_some() as usize;
        }
        assert_eq!(pending, self.lowres_pending);
        for (e, eph) in &self.ephemerals {
            assert_eq!(visible.get(e).copied().unwrap_or(0), eph.visible, "visible count of {e}");
        }
        for (key, n) in pins {
            assert_eq!(self.cache.pins(&key), n, "pin count");
        }
    }
}

/// Placement id of the tile in slot `i`: unique per slot, so placing a cached image in
/// one slot never replaces its placement in another.
fn slot_placement_id(i: usize) -> u32 {
    i as u32 + 1
}

fn tile_content(fb: &FrameBuf, px: Rect, buf: &mut Vec<u8>) -> Content {
    fb.extract(px, buf);
    match frame::solid_color(buf) {
        Some(c) => Content::Solid(c),
        None => Content::Pixels(tile_key(px.w, px.h, buf)),
    }
}

pub fn tile_key(w: u32, h: u32, rgb: &[u8]) -> u128 {
    xxhash_rust::xxh3::xxh3_128_with_seed(rgb, (w as u64) << 32 | h as u64)
}

/// Marks the tiles touched by pixel damage rectangles.
pub fn damage_to_dirty(grid: &TileGrid, damage: &[Rect], dirty: &mut DirtyMap) {
    for &r in damage {
        if let Some(cells) = grid.geom.px_rect_to_cells(r) {
            dirty.mark_rect(grid.cells_to_tiles(cells));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::CellGeom;
    use crate::sim::{Flavor, SimTerm};

    const GEOM: CellGeom = CellGeom { cell_w: 4, cell_h: 6, cols: 16, rows: 8 };

    /// Deterministic xorshift generator (no external crates).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u32 {
            (self.next() % n) as u32
        }
    }

    /// A text-like screen: rows of "glyphs" (4 patterns) on a dark background.
    struct TextScreen {
        lines: Vec<Vec<u8>>,
    }

    impl TextScreen {
        fn new(rng: &mut Rng) -> Self {
            Self { lines: (0..GEOM.rows).map(|_| Self::line(rng)).collect() }
        }
        fn line(rng: &mut Rng) -> Vec<u8> {
            let len = rng.below(GEOM.cols as u64 + 1) as usize;
            (0..GEOM.cols as usize).map(|i| if i < len { 1 + rng.below(4) as u8 } else { 0 }).collect()
        }
        fn scroll(&mut self, rng: &mut Rng) {
            self.lines.remove(0);
            self.lines.push(Self::line(rng));
        }
        fn draw(&self) -> FrameBuf {
            let mut fb = FrameBuf::new(GEOM.px_width(), GEOM.px_height());
            for (r, line) in self.lines.iter().enumerate() {
                for (c, &g) in line.iter().enumerate() {
                    for y in 0..GEOM.cell_h {
                        for x in 0..GEOM.cell_w {
                            let on = g != 0 && (x * 7 + y * 3 + g as u32) % (g as u32 + 1) == 0;
                            let px = if on { [220, 220, 200] } else { [30, 30, 40] };
                            fb.set_pixel(c as u32 * GEOM.cell_w + x, r as u32 * GEOM.cell_h + y, px);
                        }
                    }
                }
            }
            fb
        }
    }

    fn noise_rect(fb: &mut FrameBuf, rng: &mut Rng, r: Rect) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let v = rng.next();
                fb.set_pixel(x, y, [v as u8, (v >> 8) as u8, (v >> 16) as u8]);
            }
        }
    }

    struct Harness {
        renderer: Renderer,
        sim: SimTerm,
        vt: Vt,
        now: Instant,
        total: FrameStats,
    }

    impl Harness {
        fn new(cfg: RenderConfig, tile: (u32, u32)) -> Self {
            Self::with_geom(cfg, tile, GEOM)
        }

        fn with_geom(cfg: RenderConfig, tile: (u32, u32), geom: CellGeom) -> Self {
            let grid = TileGrid::new(geom, tile.0, tile.1);
            let mut sim = SimTerm::new(geom.cols, geom.rows, geom.cell_w, geom.cell_h);
            sim.flavor = match cfg.compat {
                Compat::Kitty => Flavor::Kitty,
                Compat::WezTerm => Flavor::WezTerm,
            };
            let mut h = Self {
                renderer: Renderer::new(cfg, grid),
                sim,
                vt: Vt::new(),
                now: Instant::now(),
                total: FrameStats::default(),
            };
            h.sim.feed(b"\x1b[?1049h");
            h.renderer.reset(&mut h.vt);
            h.flush();
            h
        }

        fn flush(&mut self) -> usize {
            let bytes = self.vt.take();
            self.sim.feed(&bytes);
            assert!(self.sim.errors.is_empty(), "protocol errors: {:?}", self.sim.errors);
            assert!(!self.sim.upload_in_progress());
            self.renderer.check_invariants();
            bytes.len()
        }

        fn step(&mut self, fb: &FrameBuf, damage: &[Rect]) -> (FrameStats, usize) {
            self.now += Duration::from_millis(33);
            let g = *self.renderer.grid();
            let mut dirty = DirtyMap::new(g.tiles_x(), g.tiles_y());
            damage_to_dirty(&g, damage, &mut dirty);
            let st = self.renderer.render(fb, &dirty, self.now, &mut self.vt);
            self.total.add(&st);
            let n = self.flush();
            (st, n)
        }

        fn settle(&mut self, fb: &FrameBuf) {
            self.now += self.renderer.config().refine_delay + Duration::from_millis(1);
            let st = self.renderer.refine(fb, self.now, &mut self.vt);
            self.total.add(&st);
            self.flush();
        }

        fn assert_shows(&self, fb: &FrameBuf, what: &str) {
            let shown = self.sim.render();
            if shown != fb.data() {
                let i = shown.iter().zip(fb.data()).position(|(a, b)| a != b).unwrap() / 3;
                let (x, y) = (i as u32 % fb.width(), i as u32 / fb.width());
                let g = self.renderer.grid().geom;
                panic!("{what}: first mismatch at pixel ({x},{y}) cell ({},{})", x / g.cell_w, y / g.cell_h);
            }
            let bound = self.renderer.grid().count() + self.renderer.live_ephemerals();
            assert!(self.sim.placement_count() <= bound, "{what}: placements must stay bounded");
        }
    }

    fn full() -> Rect {
        Rect::new(0, 0, GEOM.px_width(), GEOM.px_height())
    }

    fn cfg(phase: Phase) -> RenderConfig {
        RenderConfig { phase, cache_bytes: 0, ..RenderConfig::default() }
    }

    fn run_workload(phase: Phase, tile: (u32, u32), solid_fill: bool) -> Harness {
        run_workload_as(Compat::Kitty, phase, tile, solid_fill)
    }

    fn run_workload_as(compat: Compat, phase: Phase, tile: (u32, u32), solid_fill: bool) -> Harness {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut h = Harness::new(RenderConfig { solid_fill, compat, ..cfg(phase) }, tile);
        let mut text = TextScreen::new(&mut rng);
        let mut fb = text.draw();
        h.step(&fb, &[full()]);
        h.settle(&fb);
        h.assert_shows(&fb, "initial frame");

        // Typing: one cell at a time, damage exactly the cell.
        for i in 0..10u32 {
            let (c, r) = (i % GEOM.cols, 3);
            text.lines[r as usize][c as usize] = 1 + (i % 4) as u8;
            fb = text.draw();
            h.step(&fb, &[Rect::new(c * GEOM.cell_w + 1, r * GEOM.cell_h + 2, 1, 1)]);
            h.assert_shows(&fb, "typing");
        }
        h.settle(&fb);
        h.assert_shows(&fb, "after typing");

        // Scrolling with whole-screen damage.
        for _ in 0..12 {
            text.scroll(&mut rng);
            fb = text.draw();
            h.step(&fb, &[full()]);
            if phase != Phase::Progressive {
                h.assert_shows(&fb, "scrolling");
            }
        }
        h.settle(&fb);
        h.assert_shows(&fb, "after scrolling");

        // GUI-like updates: random unaligned rectangles of noise.
        for _ in 0..15 {
            let x = rng.below(GEOM.px_width() as u64 - 8);
            let y = rng.below(GEOM.px_height() as u64 - 8);
            let r = Rect::new(x, y, 1 + rng.below(20), 1 + rng.below(20)).intersect(&full()).unwrap();
            noise_rect(&mut fb, &mut rng, r);
            h.step(&fb, &[r]);
        }
        h.settle(&fb);
        h.assert_shows(&fb, "after noise");

        // Damage without change must not produce any image traffic (except Phase 1).
        let (st, _) = h.step(&fb, &[full()]);
        if phase >= Phase::Tiles {
            assert_eq!(st.changed_tiles, 0);
            assert_eq!(st.images_sent, 0);
        }
        h.assert_shows(&fb, "unchanged damage");
        h
    }

    #[test]
    fn every_phase_reproduces_frames_exactly() {
        for compat in [Compat::Kitty, Compat::WezTerm] {
            for phase in [Phase::Full, Phase::Damage, Phase::Tiles, Phase::Progressive] {
                for tile in [(1, 1), (3, 2)] {
                    for solid in [true, false] {
                        run_workload_as(compat, phase, tile, solid);
                    }
                }
            }
        }
    }

    /// Output of the kitty model breaks on WezTerm: the terminal model must notice.
    #[test]
    fn kitty_output_is_rejected_by_the_wezterm_model() {
        for phase in [Phase::Damage, Phase::Tiles, Phase::Progressive] {
            let r = std::panic::catch_unwind(|| run_workload_as_mismatched(phase));
            let msg = r.expect_err("the WezTerm model must reject kitty-style output");
            let msg = msg.downcast_ref::<String>().map_or("", String::as_str);
            assert!(msg.contains("WezTerm"), "{phase:?}: {msg}");
        }
    }

    fn run_workload_as_mismatched(phase: Phase) {
        let mut h = Harness::new(RenderConfig { compat: Compat::WezTerm, ..cfg(phase) }, (1, 1));
        h.renderer = Renderer::new(RenderConfig { compat: Compat::Kitty, ..cfg(phase) }, *h.renderer.grid());
        let mut rng = Rng(5);
        let mut text = TextScreen::new(&mut rng);
        for _ in 0..6 {
            let fb = text.draw();
            h.step(&fb, &[full()]);
            h.settle(&fb);
            text.scroll(&mut rng);
        }
    }

    /// A browser-like window: static header and footer, a viewport showing `page` from
    /// row `scroll`, and a scrollbar thumb at the right edge.
    fn browser_frame(g: CellGeom, page: &FrameBuf, scroll: u32) -> FrameBuf {
        let (w, h) = (g.px_width(), g.px_height());
        let mut fb = FrameBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let c = if !(12..h - 12).contains(&y) {
                    [40, 40, 90]
                } else if x >= w - 6 {
                    let thumb = 12 + scroll * (h - 40) / page.height();
                    if (thumb..thumb + 16).contains(&y) {
                        [200, 200, 200]
                    } else {
                        [60, 60, 60]
                    }
                } else {
                    page.pixel(x, scroll + y - 12)
                };
                fb.set_pixel(x, y, c);
            }
        }
        fb
    }

    /// Text-like page: 3x5 glyphs from a small font, lines 8 px apart (not a multiple of
    /// the cell height, like a browser).
    fn text_page(rng: &mut Rng, w: u32, h: u32) -> FrameBuf {
        let font: Vec<u16> = (0..16).map(|_| rng.next() as u16 & 0x7fff).collect();
        let mut fb = FrameBuf::new(w, h);
        noise_rect(&mut fb, rng, Rect::new(0, 0, 0, 0));
        for y in 0..h {
            for x in 0..w {
                fb.set_pixel(x, y, [235, 235, 230]);
            }
        }
        for line in 0..h / 8 {
            let len = rng.below(w as u64 / 4);
            for c in 0..len {
                let bits = font[rng.below(16) as usize];
                for (gy, gx) in (0..5).flat_map(|gy| (0..3).map(move |gx| (gy, gx))) {
                    if bits >> (gy * 3 + gx) & 1 == 1 {
                        fb.set_pixel(c * 4 + gx, line * 8 + 1 + gy, [20, 20, 30]);
                    }
                }
            }
        }
        fb
    }

    #[test]
    fn scroll_copy_moves_pixels_instead_of_resending() {
        let g = CellGeom { cell_w: 4, cell_h: 6, cols: 40, rows: 24 };
        let mut rng = Rng(77);
        let page = text_page(&mut rng, g.px_width(), 600);
        let viewport = Rect::new(0, 12, g.px_width(), g.px_height() - 24);
        // Pixel offsets that are not multiples of the cell height, both directions, a pause.
        // Shifts up to a quarter of the 120 px viewport are drawn by moving pixels.
        let offsets = [0, 5, 11, 20, 20, 33, 47, 29, 10, 16, 40, 39, 60, 61, 50];
        let one_px = offsets.windows(2).position(|w| w[1] == w[0] + 1).expect("a 1 px step");
        let mut step_bytes: [Vec<usize>; 2] = Default::default();
        for compat in [Compat::Kitty, Compat::WezTerm] {
            for (k, scroll_copy) in [true, false].into_iter().enumerate() {
                let cfg = RenderConfig { compat, scroll_copy, ..cfg(Phase::Progressive) };
                let mut h = Harness::with_geom(cfg, (1, 1), g);
                let full = Rect::new(0, 0, g.px_width(), g.px_height());
                let first = browser_frame(g, &page, 0);
                h.step(&first, &[full]);
                h.settle(&first);
                let mut exact = 0;
                for &o in &offsets[1..] {
                    let fb = browser_frame(g, &page, o);
                    let (_, n) = h.step(&fb, &[viewport]);
                    step_bytes[k].push(n);
                    // Half-resolution frames are approximate by design.
                    if h.renderer.lowres_pending == 0 {
                        h.assert_shows(&fb, &format!("{compat:?} scroll_copy={scroll_copy} offset {o}"));
                        exact += 1;
                    }
                }
                if scroll_copy {
                    assert!(exact >= offsets.len() - 3, "{compat:?}: scroll copy keeps full resolution ({exact})");
                }
                let last = browser_frame(g, &page, offsets[offsets.len() - 1]);
                h.settle(&last);
                h.assert_shows(&last, &format!("{compat:?} scroll_copy={scroll_copy} settled"));
                if scroll_copy {
                    assert!(h.total.scroll_shifts >= 8, "{compat:?}: {:?}", h.total);
                    assert!(h.renderer.region.is_some());
                } else {
                    assert_eq!(h.total.scroll_shifts, 0);
                }
                // A jump of most of the viewport is cheaper to redraw at half resolution.
                let shifts = h.total.scroll_shifts;
                let jump = browser_frame(g, &page, 150);
                h.step(&jump, &[viewport]);
                h.settle(&jump);
                h.assert_shows(&jump, &format!("{compat:?} scroll_copy={scroll_copy} after a jump"));
                assert_eq!(h.total.scroll_shifts, shifts, "{compat:?}: no shift for a large jump");
                assert!(h.renderer.region.is_none(), "{compat:?}: the jump dissolved the region");
                // Scrolling again builds a new region, which navigation dissolves.
                for o in [152, 157, 163] {
                    h.step(&browser_frame(g, &page, o), &[viewport]);
                }
                if scroll_copy {
                    assert!(h.renderer.region.is_some(), "{compat:?}: region rebuilt");
                }
                let other = text_page(&mut Rng(99), g.px_width(), 600);
                let fb = browser_frame(g, &other, 0);
                h.step(&fb, &[viewport]);
                h.settle(&fb);
                h.assert_shows(&fb, &format!("{compat:?} after navigation"));
                assert!(h.renderer.region.is_none(), "{compat:?}: region dissolved");
            }
            // Moving pixels costs a strip of the scrolled height plus a placement per piece,
            // redrawing costs the whole area; at this tiny size placements dominate, and
            // WezTerm also lifts and re-places the overlays (the scrollbar thumb) each shift.
            let (copy, redraw) = (step_bytes[0][one_px], step_bytes[1][one_px]);
            let factor = if compat == Compat::WezTerm { 1 } else { 2 };
            assert!(copy * factor < redraw, "{compat:?}: 1 px scroll: {copy} vs {redraw} bytes");
            step_bytes = Default::default();
        }
    }

    /// WezTerm duplicates the placement-id images of a cell whenever an image is placed
    /// over it; the model rejects that. A static overlay inside a scrolling area (the
    /// pointer over a blank margin, a fixed button) used to double on every shift until
    /// WezTerm hung and crashed.
    #[test]
    fn wezterm_scroll_copy_never_places_over_overlays() {
        let g = CellGeom { cell_w: 4, cell_h: 6, cols: 40, rows: 24 };
        let mut rng = Rng(31);
        let page = text_page(&mut rng, g.px_width(), 900);
        let viewport = Rect::new(0, 12, g.px_width(), g.px_height() - 24);
        let with_pointer = |fb: &mut FrameBuf| {
            for y in 60..70 {
                for x in 70..76 {
                    fb.set_pixel(x, y, [255, 0, 255]);
                }
            }
        };
        let cfg = RenderConfig { compat: Compat::WezTerm, ..cfg(Phase::Progressive) };
        let mut h = Harness::with_geom(cfg, (1, 1), g);
        let mut fb = browser_frame(g, &page, 0);
        with_pointer(&mut fb);
        h.step(&fb, &[Rect::new(0, 0, g.px_width(), g.px_height())]);
        h.settle(&fb);
        for o in (3..240).step_by(3) {
            let mut fb = browser_frame(g, &page, o);
            with_pointer(&mut fb);
            h.step(&fb, &[viewport]);
            if h.renderer.lowres_pending == 0 {
                h.assert_shows(&fb, &format!("offset {o}"));
            }
        }
        assert!(h.total.scroll_shifts >= 60, "{:?}", h.total);
    }

    #[test]
    fn compat_is_detected_from_xtversion() {
        assert_eq!(Compat::detect(Some("WezTerm 20240203-110809-5046fc22")), Compat::WezTerm);
        assert_eq!(Compat::detect(Some("ghostty 1.1.3")), Compat::Kitty);
        assert_eq!(Compat::detect(Some("kitty(0.39.1)")), Compat::Kitty);
        assert_eq!(Compat::detect(None), Compat::Kitty);
    }

    #[test]
    fn wezterm_tiles_use_placement_ids_and_id_deletes() {
        let mut h = Harness::new(RenderConfig { compat: Compat::WezTerm, ..cfg(Phase::Tiles) }, (1, 1));
        let mut rng = Rng(11);
        let mut text = TextScreen::new(&mut rng);
        let mut vt_bytes = Vec::new();
        for _ in 0..4 {
            let fb = text.draw();
            let g = *h.renderer.grid();
            let mut dirty = DirtyMap::new(g.tiles_x(), g.tiles_y());
            dirty.mark_all();
            h.renderer.render(&fb, &dirty, h.now, &mut h.vt);
            vt_bytes.extend_from_slice(&h.vt.buf);
            h.flush();
            h.assert_shows(&fb, "wezterm scrolling");
            text.scroll(&mut rng);
        }
        let out = String::from_utf8_lossy(&vt_bytes);
        for d in ["d=c", "d=q", "d=y", "d=z", "d=r", "d=R"] {
            assert!(!out.contains(d), "{d} is not implemented by WezTerm");
        }
        assert!(out.contains("a=d,d=i,i="), "tiles are deleted by image and placement id");
        let placements = out.split("\x1b_G").filter(|c| c.starts_with("a=p") || c.starts_with("a=T"));
        assert!(placements.clone().count() > 0);
        assert!(placements.filter(|c| !c.contains(",p=")).count() == 0, "every tile has a placement id");
    }

    #[test]
    fn tile_cache_hits_on_repeated_content() {
        let h = run_workload(Phase::Tiles, (1, 1), true);
        assert!(h.total.cache_hits > h.total.tiles_sent, "{:?}", h.total);
        assert!(h.total.solid_fills > 0);
    }

    #[test]
    fn fully_changed_rows_use_one_row_delete() {
        let mut h = Harness::new(cfg(Phase::Tiles), (1, 1));
        let mut rng = Rng(23);
        let mut fb = FrameBuf::new(GEOM.px_width(), GEOM.px_height());
        noise_rect(&mut fb, &mut rng, full());
        h.step(&fb, &[full()]);
        // Replace every tile of row 2 and a single tile of row 5.
        let row2 = Rect::new(0, 2 * GEOM.cell_h, GEOM.px_width(), GEOM.cell_h);
        let cell = Rect::new(3 * GEOM.cell_w, 5 * GEOM.cell_h, GEOM.cell_w, GEOM.cell_h);
        noise_rect(&mut fb, &mut rng, row2);
        noise_rect(&mut fb, &mut rng, cell);
        h.step(&fb, &[row2, cell]);
        h.assert_shows(&fb, "after row replacement");
        let mut vt = Vt::new();
        let mut hb = Harness::new(cfg(Phase::Tiles), (1, 1));
        let mut fb2 = FrameBuf::new(GEOM.px_width(), GEOM.px_height());
        noise_rect(&mut fb2, &mut Rng(23), full());
        hb.step(&fb2, &[full()]);
        let mut dirty = DirtyMap::new(GEOM.cols, GEOM.rows);
        damage_to_dirty(hb.renderer.grid(), &[row2, cell], &mut dirty);
        hb.renderer.render(&fb, &dirty, hb.now, &mut vt);
        let out = String::from_utf8_lossy(&vt.buf);
        assert_eq!(out.matches("d=y,y=3,").count(), 1, "row 2 is deleted in one command");
        assert_eq!(out.matches("d=c").count(), 1, "the lone tile in row 5 uses a per-tile delete");
    }

    #[test]
    fn phase1_sends_whole_frame_even_without_damage() {
        let mut rng = Rng(7);
        let fb = TextScreen::new(&mut rng).draw();
        let mut h = Harness::new(cfg(Phase::Full), (1, 1));
        h.step(&fb, &[full()]);
        let (st, bytes) = h.step(&fb, &[]);
        assert_eq!(st.images_sent, 1);
        assert!(bytes > 0);
        assert_eq!(h.sim.image_count(), 1, "previous frame must be deleted");
        h.assert_shows(&fb, "phase 1");
    }

    #[test]
    fn phase2_sends_only_damaged_cells() {
        let mut rng = Rng(11);
        let mut fb = TextScreen::new(&mut rng).draw();
        let mut h = Harness::new(cfg(Phase::Damage), (1, 1));
        h.step(&fb, &[full()]);
        let r = Rect::new(3, 5, 2, 2);
        noise_rect(&mut fb, &mut rng, r);
        let (st, _) = h.step(&fb, &[r]);
        assert_eq!(st.images_sent, 1);
        assert_eq!(st.changed_tiles, 2 * 2, "2x2 px at (3,5) spans 2x2 cells of 4x6 px");
        let (st, bytes) = h.step(&fb, &[]);
        assert_eq!((st.images_sent, bytes), (0, 0), "no damage, no output");
        h.assert_shows(&fb, "phase 2");
    }

    #[test]
    fn phase4_uses_half_resolution_then_refines() {
        let mut rng = Rng(3);
        let mut text = TextScreen::new(&mut rng);
        let mut h = Harness::new(cfg(Phase::Progressive), (1, 1));
        let mut fb = text.draw();
        h.step(&fb, &[full()]);
        h.settle(&fb);
        text.scroll(&mut rng);
        fb = text.draw();
        let (st, _) = h.step(&fb, &[full()]);
        assert_eq!(st.lowres_images, 1);
        assert!(h.renderer.next_refine_at().is_some());
        assert_ne!(h.sim.render(), fb.data(), "motion frame is half resolution");
        h.settle(&fb);
        assert!(h.renderer.next_refine_at().is_none());
        assert!(h.total.refined_tiles > 0);
        h.assert_shows(&fb, "refined");
        assert_eq!(h.renderer.live_ephemerals(), 0, "half-res image deleted after refinement");
    }

    #[test]
    fn phase4_slow_frames_do_not_trigger_refinement_by_time_alone() {
        let mut rng = Rng(29);
        let mut text = TextScreen::new(&mut rng);
        let mut h = Harness::new(cfg(Phase::Progressive), (1, 1));
        let mut fb = text.draw();
        h.step(&fb, &[full()]);
        h.settle(&fb);
        text.scroll(&mut rng);
        fb = text.draw();
        let (st, _) = h.step(&fb, &[full()]);
        assert_eq!(st.lowres_images, 1);
        let pending = h.renderer.lowres_pending;
        // A slow link: the next frame arrives a second later, still in motion elsewhere.
        h.now += Duration::from_secs(1);
        let mut next = fb.clone();
        noise_rect(&mut next, &mut rng, Rect::new(0, 0, 4, 6));
        let (st, _) = h.step(&next, &[Rect::new(0, 0, 4, 6)]);
        assert_eq!(st.refined_tiles, 0, "one quiet frame is not enough, however long it took");
        assert!(h.renderer.lowres_pending >= pending - 1);
        // A second unchanged frame refines everything that stayed put.
        noise_rect(&mut next, &mut rng, Rect::new(0, 0, 4, 6));
        let (st, _) = h.step(&next, &[Rect::new(0, 0, 4, 6)]);
        assert!(st.refined_tiles > 0);
        assert_eq!(h.renderer.lowres_pending, 0);
        h.assert_shows(&next, "refined after quiet frames");
    }

    #[test]
    fn small_cache_evicts_but_stays_correct() {
        let mut rng = Rng(5);
        let mut h = Harness::new(cfg(Phase::Tiles), (1, 1));
        let mut fb = FrameBuf::new(GEOM.px_width(), GEOM.px_height());
        for _ in 0..6 {
            noise_rect(&mut fb, &mut rng, full());
            h.step(&fb, &[full()]);
            h.assert_shows(&fb, "noise screens");
        }
        assert!(h.total.evictions > 0);
        let budget = h.renderer.config().cache_bytes;
        assert!(h.renderer.cache_cost() <= budget);
        assert!(h.sim.stats.max_image_bytes <= budget * 2 + 1, "terminal-side memory stays near the budget");
    }

    #[test]
    fn cache_budget_is_raised_to_one_screen() {
        let mut r = Renderer::new(cfg(Phase::Tiles), TileGrid::new(GEOM, 1, 1));
        let min = (GEOM.cols * GEOM.rows * GEOM.cell_w * GEOM.cell_h * 4) as u64;
        assert_eq!(r.config().cache_bytes, min);
        assert!(r.take_note().is_some());
        assert!(r.take_note().is_none());
    }

    #[test]
    fn reset_and_exit_cleanup_leave_no_images() {
        let h = run_workload(Phase::Progressive, (1, 1), true);
        let mut h = h;
        h.renderer.reset(&mut h.vt);
        h.flush();
        assert_eq!((h.sim.image_count(), h.sim.placement_count()), (0, 0));
        let mut rng = Rng(9);
        let fb = TextScreen::new(&mut rng).draw();
        h.step(&fb, &[full()]);
        h.settle(&fb);
        h.assert_shows(&fb, "after reset");
        h.renderer.delete_all_images(&mut h.vt);
        h.vt.raw(b"\x1b[?1049l");
        h.flush();
    }

    #[test]
    fn id_exhaustion_triggers_reset() {
        let mut rng = Rng(13);
        let mut h = Harness::new(cfg(Phase::Tiles), (1, 1));
        let fb = TextScreen::new(&mut rng).draw();
        h.step(&fb, &[full()]);
        h.renderer.set_next_id_for_test(u32::MAX - 100);
        let fb2 = TextScreen::new(&mut rng).draw();
        h.step(&fb2, &[full()]);
        h.assert_shows(&fb2, "after id wrap");
        assert!(h.sim.image_ids().iter().all(|&id| id < 1000));
    }

    #[test]
    fn smaller_frame_is_padded_with_black() {
        let mut h = Harness::new(cfg(Phase::Tiles), (1, 1));
        let mut small = FrameBuf::new(GEOM.px_width() - 5, GEOM.px_height() - 7);
        let b = small.bounds();
        noise_rect(&mut small, &mut Rng(21), b);
        h.step(&small, &[full()]);
        let mut expect = FrameBuf::new(GEOM.px_width(), GEOM.px_height());
        for y in 0..small.height() {
            for x in 0..small.width() {
                expect.set_pixel(x, y, small.pixel(x, y));
            }
        }
        h.assert_shows(&expect, "padded");
    }

    #[test]
    fn ephemeral_area_is_compacted() {
        let mut rng = Rng(17);
        let mut h = Harness::new(cfg(Phase::Damage), (1, 1));
        let mut fb = FrameBuf::new(GEOM.px_width(), GEOM.px_height());
        noise_rect(&mut fb, &mut rng, full());
        h.step(&fb, &[full()]);
        // Overlapping large rects keep many partially visible images alive.
        for i in 0..40u32 {
            let r = Rect::new((i % 5) * 4, (i % 3) * 6, GEOM.px_width() - 20, GEOM.px_height() - 12);
            noise_rect(&mut fb, &mut rng, r);
            h.step(&fb, &[r]);
            assert!(h.renderer.ephemeral_area <= (MAX_EPHEMERAL_SCREENS + 1) * h.renderer.grid().count() as u64);
        }
        h.assert_shows(&fb, "compacted");
    }

    /// Ghostty 1.3 can crash inflating zlib images over [`PNG_MIN_BYTES`] (the model
    /// flags them); with `large_png` they go as PNG and still show the exact pixels.
    #[test]
    fn large_images_go_as_png() {
        let g = CellGeom { cell_w: 8, cell_h: 16, cols: 40, rows: 24 };
        let page = text_page(&mut Rng(5), g.px_width(), 1200);
        let viewport = Rect::new(0, 12, g.px_width(), g.px_height() - 24);
        for large_png in [false, true] {
            let mut h = Harness::with_geom(RenderConfig { large_png, ..cfg(Phase::Progressive) }, (1, 1), g);
            h.sim.zlib_limit = Some(PNG_MIN_BYTES);
            let first = browser_frame(g, &page, 0);
            let mut all = DirtyMap::new(h.renderer.grid().tiles_x(), h.renderer.grid().tiles_y());
            all.mark_all();
            h.renderer.render(&first, &all, h.now, &mut h.vt);
            let bytes = h.vt.take();
            h.sim.feed(&bytes);
            if !large_png {
                assert!(h.sim.errors.iter().any(|e| e.contains("Ghostty 1.3")), "{:?}", h.sim.errors);
                continue;
            }
            assert!(bytes.windows(6).any(|w| w == b"f=100,"), "a PNG was sent");
            h.flush();
            h.settle(&first);
            h.assert_shows(&first, "first frame");
            for o in [4, 9, 15, 22, 30, 31, 60, 200] {
                let fb = browser_frame(g, &page, o);
                h.step(&fb, &[viewport]);
                if h.renderer.lowres_pending == 0 {
                    h.assert_shows(&fb, &format!("offset {o}"));
                }
            }
            let last = browser_frame(g, &page, 200);
            h.settle(&last);
            h.assert_shows(&last, "settled");
            assert!(h.total.scroll_shifts > 0, "scroll pieces were exercised: {:?}", h.total);
        }
    }
}
