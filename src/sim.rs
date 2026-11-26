//! Terminal model for the subset of VT and Kitty graphics that termway emits.
//!
//! Used by tests and the benchmark harness to reconstruct what a terminal would
//! display and to flag protocol misuse (missing `q=2`/`C=1`, bad chunking, ...).

use crate::{base64, png};
use flate2::read::ZlibDecoder;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;

/// Requests from the program that a real terminal would answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    CellSize,
    WindowSize,
    TextAreaChars,
    PrimaryDeviceAttributes,
    Mode(u32),
    Graphics {
        id: u32,
    },
    /// XTVERSION (`CSI > q`)
    Version,
}

/// Which terminal's image model to follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flavor {
    /// The Kitty graphics specification (kitty, Ghostty).
    #[default]
    Kitty,
    /// WezTerm 20240203 (unchanged in main as of 2026-09): an image is cut into one
    /// slice per cell of its size (`X`/`Y` shift the slices but add no cells), attached
    /// to those cells. Only `d=a`/`d=i` deletes are implemented, a placement without `p`
    /// drops the image's other placements, ECH keeps only the slices of images with a
    /// placement id while EL/ED drop all. Flagged as errors: overlapping same-z slices
    /// (drawn in no defined order) and placing an image over a cell holding a slice
    /// with a placement id, which WezTerm copies back into the cell on every placement
    /// until the slices and quads grow without bound and it hangs and crashes.
    WezTerm,
}

#[derive(Clone, Debug)]
struct SimImage {
    w: u32,
    h: u32,
    rgb: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct SimPlacement {
    image: u32,
    pid: u32,
    /// Cells the placement belongs to: those it covers (kitty), or one per cell of the
    /// image size, ignoring the `X`/`Y` shift (WezTerm slices).
    row: u32,
    col: u32,
    rows: u32,
    cols: u32,
    z: i32,
    /// Explicit `c`/`r` given: the image is scaled to the cell extent.
    scaled: bool,
    /// Shown part of the image (`x`,`y`,`w`,`h`).
    src: (u32, u32, u32, u32),
    /// Pixel offset inside the first cell (`X`,`Y`).
    off: (u32, u32),
}

impl SimPlacement {
    /// Screen pixels covered: (x, y, w, h).
    fn pixel_rect(&self, cell_w: u32, cell_h: u32) -> (u32, u32, u32, u32) {
        let (x, y) = (self.col * cell_w + self.off.0, self.row * cell_h + self.off.1);
        if self.scaled {
            (self.col * cell_w, self.row * cell_h, self.cols * cell_w, self.rows * cell_h)
        } else {
            (x, y, self.src.2, self.src.3)
        }
    }
}

fn intersects(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

#[derive(Debug)]
enum State {
    Ground,
    Esc,
    Csi(Vec<u8>),
    Apc(Vec<u8>),
    ApcEsc(Vec<u8>),
    Str,
    StrEsc,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SimStats {
    pub transmissions: u64,
    pub placements_made: u64,
    pub deletes: u64,
    pub max_placements: usize,
    pub max_images: usize,
    /// Peak decoded image storage (4 bytes per pixel, like terminals that keep RGBA).
    pub max_image_bytes: u64,
}

pub struct SimTerm {
    pub cols: u32,
    pub rows: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    pub default_bg: [u8; 3],
    pub flavor: Flavor,
    /// Largest zlib-decoded image accepted, modelling Ghostty 1.3 (see [`crate::png`]):
    /// its inflater can crash on larger payloads, so they are flagged as errors.
    pub zlib_limit: Option<usize>,
    cursor: (u32, u32),
    sgr_bg: Option<[u8; 3]>,
    cells: Vec<Option<[u8; 3]>>,
    images: HashMap<u32, SimImage>,
    image_bytes: u64,
    placements: BTreeMap<u64, SimPlacement>,
    by_cell: Vec<Vec<u64>>,
    next_uid: u64,
    upload: Option<(HashMap<u8, String>, Vec<u8>)>,
    state: State,
    pub alt_screen: bool,
    pub sync_depth: i32,
    pub errors: Vec<String>,
    pub queries: Vec<Query>,
    pub stats: SimStats,
}

fn parse_keys(ctrl: &[u8]) -> Result<HashMap<u8, String>, String> {
    let mut keys = HashMap::new();
    for kv in ctrl.split(|&b| b == b',').filter(|kv| !kv.is_empty()) {
        match kv {
            [k, b'=', v @ ..] => {
                let v = std::str::from_utf8(v).map_err(|_| "non-UTF-8 key value".to_string())?;
                keys.insert(*k, v.to_string());
            }
            _ => return Err(format!("malformed key {:?}", String::from_utf8_lossy(kv))),
        }
    }
    Ok(keys)
}

fn num<T: std::str::FromStr + Default>(keys: &HashMap<u8, String>, k: u8) -> Result<T, String> {
    match keys.get(&k) {
        None => Ok(T::default()),
        Some(v) => v.parse().map_err(|_| format!("bad value {v:?} for key {}", k as char)),
    }
}

impl SimTerm {
    pub fn new(cols: u32, rows: u32, cell_w: u32, cell_h: u32) -> Self {
        Self {
            cols,
            rows,
            cell_w,
            cell_h,
            default_bg: [0, 0, 0],
            flavor: Flavor::Kitty,
            zlib_limit: None,
            cursor: (0, 0),
            sgr_bg: None,
            cells: vec![None; (cols * rows) as usize],
            images: HashMap::new(),
            image_bytes: 0,
            placements: BTreeMap::new(),
            by_cell: vec![Vec::new(); (cols * rows) as usize],
            next_uid: 0,
            upload: None,
            state: State::Ground,
            alt_screen: false,
            sync_depth: 0,
            errors: Vec::new(),
            queries: Vec::new(),
            stats: SimStats::default(),
        }
    }

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    pub fn image_ids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.images.keys().copied().collect();
        v.sort_unstable();
        v
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        let state = std::mem::replace(&mut self.state, State::Ground);
        self.state = match state {
            State::Ground => match b {
                0x1b => State::Esc,
                b'\r' => {
                    self.cursor.1 = 0;
                    State::Ground
                }
                b'\n' => {
                    self.cursor.0 = (self.cursor.0 + 1).min(self.rows - 1);
                    State::Ground
                }
                0x08 => {
                    self.cursor.1 = self.cursor.1.saturating_sub(1);
                    State::Ground
                }
                0x20..=0x7e | 0xc0..=0xff => {
                    self.print();
                    State::Ground
                }
                _ => State::Ground,
            },
            State::Esc => match b {
                b'[' => State::Csi(Vec::new()),
                b'_' => State::Apc(Vec::new()),
                b']' | b'P' | b'^' => State::Str,
                _ => State::Ground,
            },
            State::Csi(mut v) => {
                if (0x40..=0x7e).contains(&b) {
                    self.csi(&v, b);
                    State::Ground
                } else {
                    v.push(b);
                    State::Csi(v)
                }
            }
            State::Apc(mut v) => match b {
                0x1b => State::ApcEsc(v),
                _ => {
                    v.push(b);
                    State::Apc(v)
                }
            },
            State::ApcEsc(mut v) => {
                if b == b'\\' {
                    if v.first() == Some(&b'G') {
                        self.graphics(&v[1..]);
                    }
                    State::Ground
                } else {
                    v.push(0x1b);
                    v.push(b);
                    State::Apc(v)
                }
            }
            State::Str => match b {
                0x07 => State::Ground,
                0x1b => State::StrEsc,
                _ => State::Str,
            },
            State::StrEsc => {
                if b == b'\\' {
                    State::Ground
                } else {
                    State::Str
                }
            }
        };
    }

    fn print(&mut self) {
        let (r, c) = self.cursor;
        if r < self.rows && c < self.cols {
            self.set_cell(r, c, true);
        }
        self.cursor.1 = (c + 1).min(self.cols - 1);
    }

    fn erase(&mut self, row: u32, from: u32, to: u32, keep_pid_slices: bool) {
        for c in from..to.min(self.cols) {
            self.set_cell(row, c, keep_pid_slices);
        }
    }

    /// Replaces a cell by a blank one with the current background. In WezTerm the
    /// slices attached to the cell go with it, except those of images with a placement
    /// id when the cell is written cell by cell (ECH, text) rather than cleared (EL, ED).
    fn set_cell(&mut self, row: u32, col: u32, keep_pid_slices: bool) {
        let i = (row * self.cols + col) as usize;
        self.cells[i] = self.sgr_bg;
        if self.flavor == Flavor::WezTerm {
            let placements = &self.placements;
            self.by_cell[i].retain(|u| keep_pid_slices && placements[u].pid != 0);
        }
    }

    fn csi(&mut self, body: &[u8], fin: u8) {
        let private = body.first().copied().filter(|b| b"?<>=".contains(b));
        let rest = if private.is_some() { &body[1..] } else { body };
        let inter: Vec<u8> = rest.iter().copied().filter(|b| (0x20..=0x2f).contains(b)).collect();
        let params: Vec<u32> = rest
            .iter()
            .copied()
            .filter(|b| !(0x20..=0x2f).contains(b))
            .collect::<Vec<u8>>()
            .split(|&b| b == b';' || b == b':')
            .map(|p| std::str::from_utf8(p).ok().and_then(|s| s.parse().ok()).unwrap_or(0))
            .collect();
        let p = |i: usize, d: u32| params.get(i).copied().filter(|&v| v != 0).unwrap_or(d);
        let (row, col) = self.cursor;
        match (private, inter.as_slice(), fin) {
            (None, [], b'H' | b'f') => {
                self.cursor = ((p(0, 1) - 1).min(self.rows - 1), (p(1, 1) - 1).min(self.cols - 1));
            }
            (None, [], b'A') => self.cursor.0 = row.saturating_sub(p(0, 1)),
            (None, [], b'B') => self.cursor.0 = (row + p(0, 1)).min(self.rows - 1),
            (None, [], b'C') => self.cursor.1 = (col + p(0, 1)).min(self.cols - 1),
            (None, [], b'D') => self.cursor.1 = col.saturating_sub(p(0, 1)),
            (None, [], b'G') => self.cursor.1 = (p(0, 1) - 1).min(self.cols - 1),
            (None, [], b'd') => self.cursor.0 = (p(0, 1) - 1).min(self.rows - 1),
            (None, [], b'X') => self.erase(row, col, col + p(0, 1), true),
            (None, [], b'K') => match params.first().copied().unwrap_or(0) {
                0 => self.erase(row, col, self.cols, false),
                1 => self.erase(row, 0, col + 1, false),
                _ => self.erase(row, 0, self.cols, false),
            },
            (None, [], b'J') => {
                let mode = params.first().copied().unwrap_or(0);
                for r in 0..self.rows {
                    let whole = mode >= 2 || (mode == 0 && r > row) || (mode == 1 && r < row);
                    if whole {
                        self.erase(r, 0, self.cols, false);
                    } else if r == row {
                        match mode {
                            0 => self.erase(r, col, self.cols, false),
                            _ => self.erase(r, 0, col + 1, false),
                        }
                    }
                }
            }
            (None, [], b'm') => self.sgr(&params),
            (Some(b'?'), [], b'h' | b'l') => {
                let on = fin == b'h';
                for &m in &params {
                    match m {
                        2026 => self.sync_depth += if on { 1 } else { -1 },
                        1049 => self.set_alt_screen(on),
                        _ => {}
                    }
                }
            }
            (None, [], b't') => match params.first() {
                Some(16) => self.queries.push(Query::CellSize),
                Some(14) => self.queries.push(Query::WindowSize),
                Some(18) => self.queries.push(Query::TextAreaChars),
                _ => {}
            },
            (None, [], b'c') => self.queries.push(Query::PrimaryDeviceAttributes),
            (Some(b'?'), [b'$'], b'p') => self.queries.push(Query::Mode(p(0, 0))),
            (Some(b'>'), [], b'q') => self.queries.push(Query::Version),
            _ => {}
        }
    }

    fn sgr(&mut self, params: &[u32]) {
        let mut i = 0;
        if params.is_empty() {
            self.sgr_bg = None;
        }
        while i < params.len() {
            match params[i] {
                0 => self.sgr_bg = None,
                49 => self.sgr_bg = None,
                48 | 38 => {
                    let is_bg = params[i] == 48;
                    match params.get(i + 1) {
                        Some(2) if i + 4 < params.len() => {
                            if is_bg {
                                self.sgr_bg = Some([params[i + 2] as u8, params[i + 3] as u8, params[i + 4] as u8]);
                            }
                            i += 4;
                        }
                        Some(5) => i += 2,
                        _ => self.errors.push("truncated extended colour SGR".into()),
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn set_alt_screen(&mut self, on: bool) {
        if on == self.alt_screen {
            return;
        }
        if !on && (!self.images.is_empty() || !self.placements.is_empty()) {
            self.errors.push(format!(
                "left alternate screen with {} images / {} placements still alive",
                self.images.len(),
                self.placements.len()
            ));
        }
        self.alt_screen = on;
        self.images.clear();
        self.image_bytes = 0;
        self.placements.clear();
        self.by_cell.iter_mut().for_each(Vec::clear);
        self.cells.fill(None);
        self.cursor = (0, 0);
    }

    fn graphics(&mut self, body: &[u8]) {
        let (ctrl, payload) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &[][..]),
        };
        let keys = match parse_keys(ctrl) {
            Ok(k) => k,
            Err(e) => return self.errors.push(e),
        };
        if let Err(e) = self.graphics_cmd(keys, payload) {
            self.errors.push(e);
        }
    }

    fn graphics_cmd(&mut self, keys: HashMap<u8, String>, payload: &[u8]) -> Result<(), String> {
        // Only support queries (a=q) may ask for a reply.
        let is_query = keys.get(&b'a').map(String::as_str) == Some("q");
        if !is_query && keys.get(&b'q').map(String::as_str) != Some("2") {
            return Err("graphics command without q=2 would make the terminal reply".into());
        }
        let more = keys.get(&b'm').map(String::as_str) == Some("1");
        if let Some((first, mut data)) = self.upload.take() {
            if keys.keys().any(|k| *k != b'm' && *k != b'q') {
                return Err("continuation chunk carries keys other than m/q".into());
            }
            if data.len() % 4 != 0 {
                return Err("non-final chunk size is not a multiple of 4".into());
            }
            if payload.len() > crate::kitty::CHUNK_SIZE {
                return Err("chunk larger than 4096 bytes".into());
            }
            data.extend_from_slice(payload);
            if more {
                self.upload = Some((first, data));
                return Ok(());
            }
            return self.transmit(&first, &data);
        }
        let action = keys.get(&b'a').map(String::as_str).unwrap_or("t");
        match action {
            "t" | "T" | "q" => {
                if payload.len() > crate::kitty::CHUNK_SIZE {
                    return Err("chunk larger than 4096 bytes".into());
                }
                if more {
                    self.upload = Some((keys, payload.to_vec()));
                    Ok(())
                } else {
                    self.transmit(&keys, payload)
                }
            }
            "p" => {
                let id: u32 = num(&keys, b'i')?;
                if !self.images.contains_key(&id) {
                    return Err(format!("placement of unknown image {id}"));
                }
                self.place(id, &keys)
            }
            "d" => {
                self.stats.deletes += 1;
                self.delete(&keys)
            }
            other => Err(format!("unsupported action a={other}")),
        }
    }

    fn transmit(&mut self, keys: &HashMap<u8, String>, b64: &[u8]) -> Result<(), String> {
        let action = keys.get(&b'a').map(String::as_str).unwrap_or("t");
        let id: u32 = num(keys, b'i')?;
        let fmt: u32 = keys.get(&b'f').map_or(Ok(32), |v| v.parse().map_err(|_| "bad f".to_string()))?;
        if keys.get(&b't').is_some_and(|t| t != "d") {
            return Err("only direct transmission is modelled".into());
        }
        let raw = base64::decode(b64).map_err(|e| e.to_string())?;
        let (w, h, rgb) = if fmt == 100 {
            if keys.contains_key(&b'o') {
                return Err(format!("image {id}: compressed PNG is not modelled"));
            }
            let (w, h, rgb) = png::decode(&raw).map_err(|e| format!("image {id}: {e}"))?;
            for (k, v) in [(b's', w), (b'v', h)] {
                if keys.contains_key(&k) && num::<u32>(keys, k)? != v {
                    return Err(format!("image {id}: {}= does not match the {w}x{h} PNG", k as char));
                }
            }
            (w, h, rgb)
        } else {
            let (w, h): (u32, u32) = (num(keys, b's')?, num(keys, b'v')?);
            let data = match keys.get(&b'o').map(String::as_str) {
                None => raw,
                Some("z") => {
                    let mut out = Vec::new();
                    ZlibDecoder::new(&raw[..]).read_to_end(&mut out).map_err(|e| format!("zlib: {e}"))?;
                    if let Some(limit) = self.zlib_limit.filter(|&l| out.len() > l) {
                        return Err(format!(
                            "image {id}: zlib payload of {} bytes; Ghostty 1.3 can crash inflating ones over {limit}",
                            out.len()
                        ));
                    }
                    out
                }
                Some(o) => return Err(format!("unknown compression o={o}")),
            };
            let bpp = match fmt {
                24 => 3,
                32 => 4,
                _ => return Err(format!("unsupported format f={fmt}")),
            };
            if w == 0 || h == 0 || data.len() != (w * h) as usize * bpp {
                return Err(format!("image {id}: {} bytes for {w}x{h} f={fmt}", data.len()));
            }
            let rgb = if bpp == 3 { data } else { data.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect() };
            (w, h, rgb)
        };
        if action == "q" {
            self.queries.push(Query::Graphics { id });
            return Ok(());
        }
        if id == 0 {
            return Err("image without id".into());
        }
        if self.images.contains_key(&id) {
            // Retransmission replaces the image and removes its placements.
            self.remove_placements(|p| p.image == id);
            self.free_image(id);
        }
        self.images.insert(id, SimImage { w, h, rgb });
        self.image_bytes += w as u64 * h as u64 * 4;
        self.stats.transmissions += 1;
        self.stats.max_images = self.stats.max_images.max(self.images.len());
        self.stats.max_image_bytes = self.stats.max_image_bytes.max(self.image_bytes);
        if action == "T" {
            self.place(id, keys)?;
        }
        Ok(())
    }

    fn free_image(&mut self, id: u32) {
        if let Some(img) = self.images.remove(&id) {
            self.image_bytes -= img.w as u64 * img.h as u64 * 4;
        }
    }

    fn place(&mut self, id: u32, keys: &HashMap<u8, String>) -> Result<(), String> {
        if keys.get(&b'C').map(String::as_str) != Some("1") {
            return Err("placement without C=1 would move the cursor".into());
        }
        let img = &self.images[&id];
        let (sx, sy): (u32, u32) = (num(keys, b'x')?, num(keys, b'y')?);
        let sw = keys.get(&b'w').map_or(Ok(img.w.saturating_sub(sx)), |_| num(keys, b'w'))?;
        let sh = keys.get(&b'h').map_or(Ok(img.h.saturating_sub(sy)), |_| num(keys, b'h'))?;
        if sw == 0 || sh == 0 || sx + sw > img.w || sy + sh > img.h {
            return Err(format!("image {id}: source {sx},{sy} {sw}x{sh} outside {}x{}", img.w, img.h));
        }
        let off: (u32, u32) = (num(keys, b'X')?, num(keys, b'Y')?);
        if off.0 >= self.cell_w || off.1 >= self.cell_h {
            return Err(format!("image {id}: cell offset {off:?} not below the cell size"));
        }
        let scaled = keys.contains_key(&b'c') || keys.contains_key(&b'r');
        if scaled && off != (0, 0) {
            return Err("cell offsets with c/r are not modelled".into());
        }
        let cols: u32 = num(keys, b'c')?;
        let rows: u32 = num(keys, b'r')?;
        let pid: u32 = num(keys, b'p')?;
        let z: i32 = num(keys, b'z')?;
        if pid != 0 {
            self.remove_placements(|p| p.image == id && p.pid == pid);
        } else if self.flavor == Flavor::WezTerm {
            self.remove_placements(|p| p.image == id);
        }
        let (row, col) = self.cursor;
        let covered = |n: u32, off: u32, len: u32, cell: u32| if n == 0 { (off + len).div_ceil(cell) } else { n };
        let (vis_cols, vis_rows) = (covered(cols, off.0, sw, self.cell_w), covered(rows, off.1, sh, self.cell_h));
        if row + vis_rows > self.rows || col + vis_cols > self.cols {
            return Err(format!("image {id} placed at {row},{col} extends past the screen"));
        }
        let (cols, rows) = match self.flavor {
            Flavor::Kitty => (vis_cols, vis_rows),
            Flavor::WezTerm => (covered(cols, 0, sw, self.cell_w), covered(rows, 0, sh, self.cell_h)),
        };
        let pl = SimPlacement { image: id, pid, row, col, rows, cols, z, scaled, src: (sx, sy, sw, sh), off };
        if self.flavor == Flavor::WezTerm {
            self.check_wezterm_placement(&pl)?;
        }
        let uid = self.next_uid;
        self.next_uid += 1;
        for r in row..row + rows {
            for c in col..col + cols {
                self.by_cell[(r * self.cols + c) as usize].push(uid);
            }
        }
        self.placements.insert(uid, pl);
        self.stats.placements_made += 1;
        self.stats.max_placements = self.stats.max_placements.max(self.placements.len());
        Ok(())
    }

    /// Screen pixels of the slice of `p` attached to cell (`row`, `col`) (WezTerm).
    fn slice_rect(&self, p: &SimPlacement, row: u32, col: u32) -> (u32, u32, u32, u32) {
        let (x, y, w, h) = p.pixel_rect(self.cell_w, self.cell_h);
        let (kx, ky) = ((col - p.col) * self.cell_w, (row - p.row) * self.cell_h);
        (x + kx, y + ky, self.cell_w.min(w - kx), self.cell_h.min(h - ky))
    }

    fn check_wezterm_placement(&self, pl: &SimPlacement) -> Result<(), String> {
        let id = pl.image;
        for r in pl.row..pl.row + pl.rows {
            for c in pl.col..pl.col + pl.cols {
                let held = &self.by_cell[(r * self.cols + c) as usize];
                if let Some(o) = held.iter().map(|u| &self.placements[u]).find(|o| o.pid != 0) {
                    return Err(format!(
                        "image {id} placed over cell {r},{c} holding image {} placement {}: WezTerm \
                         duplicates placement-id images on every placement over them",
                        o.image, o.pid
                    ));
                }
            }
        }
        // Slices are shifted by less than a cell, so only neighbours of the covered
        // cells can reach into the image.
        let (cw, ch) = (self.cell_w, self.cell_h);
        let mine = pl.pixel_rect(cw, ch);
        let (r1, c1) = ((mine.1 + mine.3).div_ceil(ch).min(self.rows), (mine.0 + mine.2).div_ceil(cw).min(self.cols));
        for r in (mine.1 / ch).saturating_sub(1)..r1 {
            for c in (mine.0 / cw).saturating_sub(1)..c1 {
                for u in &self.by_cell[(r * self.cols + c) as usize] {
                    let o = &self.placements[u];
                    if o.z == pl.z && intersects(self.slice_rect(o, r, c), mine) {
                        return Err(format!(
                            "image {id} placed over another z={} image in cell {r},{c}: WezTerm's order is undefined",
                            pl.z
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn remove_uid(&mut self, uid: u64) -> Option<SimPlacement> {
        let p = self.placements.remove(&uid)?;
        for r in p.row..p.row + p.rows {
            for c in p.col..p.col + p.cols {
                self.by_cell[(r * self.cols + c) as usize].retain(|&u| u != uid);
            }
        }
        Some(p)
    }

    /// Removes matching placements, returning the images they belonged to.
    fn remove_placements(&mut self, f: impl Fn(&SimPlacement) -> bool) -> Vec<u32> {
        let uids: Vec<u64> = self.placements.iter().filter(|(_, p)| f(p)).map(|(u, _)| *u).collect();
        uids.into_iter().filter_map(|u| self.remove_uid(u)).map(|p| p.image).collect()
    }

    fn remove_at_cell(&mut self, row: u32, col: u32, z: Option<i32>) -> Vec<u32> {
        if row >= self.rows || col >= self.cols {
            return Vec::new();
        }
        let uids: Vec<u64> = self.by_cell[(row * self.cols + col) as usize]
            .iter()
            .copied()
            .filter(|u| z.is_none_or(|z| self.placements[u].z == z))
            .collect();
        uids.into_iter().filter_map(|u| self.remove_uid(u)).map(|p| p.image).collect()
    }

    fn delete(&mut self, keys: &HashMap<u8, String>) -> Result<(), String> {
        let d = keys.get(&b'd').map(String::as_str).unwrap_or("a");
        if self.flavor == Flavor::WezTerm && !matches!(d, "a" | "A" | "i" | "I") {
            return Err(format!("WezTerm ignores d={d} deletes"));
        }
        let free = d.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        let id: u32 = num(keys, b'i')?;
        let pid: u32 = num(keys, b'p')?;
        let x: u32 = num(keys, b'x')?;
        let y: u32 = num(keys, b'y')?;
        let z: i32 = num(keys, b'z')?;
        let touched = match d {
            "A" if self.flavor == Flavor::WezTerm => {
                self.remove_placements(|_| true);
                let ids: Vec<u32> = self.images.keys().copied().collect();
                ids.into_iter().for_each(|i| self.free_image(i));
                Vec::new()
            }
            "a" | "A" => self.remove_placements(|_| true),
            "i" | "I" => {
                let v = self.remove_placements(|p| p.image == id && (pid == 0 || p.pid == pid));
                if free && pid == 0 {
                    self.free_image(id);
                }
                v
            }
            "c" | "C" => self.remove_at_cell(self.cursor.0, self.cursor.1, None),
            "p" | "P" => self.remove_at_cell(y.wrapping_sub(1), x.wrapping_sub(1), None),
            "q" | "Q" => self.remove_at_cell(y.wrapping_sub(1), x.wrapping_sub(1), Some(z)),
            "z" | "Z" => self.remove_placements(|p| p.z == z),
            "x" | "X" => self.remove_placements(|p| x >= 1 && (p.col..p.col + p.cols).contains(&(x - 1))),
            "y" | "Y" => self.remove_placements(|p| y >= 1 && (p.row..p.row + p.rows).contains(&(y - 1))),
            "r" | "R" => {
                let v = self.remove_placements(|p| (x..=y).contains(&p.image));
                if free {
                    let ids: Vec<u32> = self.images.keys().copied().filter(|i| (x..=y).contains(i)).collect();
                    ids.into_iter().for_each(|i| self.free_image(i));
                }
                v
            }
            other => return Err(format!("unsupported delete d={other}")),
        };
        if free {
            for img in touched {
                if !self.placements.values().any(|p| p.image == img) {
                    self.free_image(img);
                }
            }
        }
        Ok(())
    }

    /// Composites the screen: cell backgrounds, then images ordered by (z, id), minus
    /// the slices WezTerm erased.
    pub fn render(&self) -> Vec<u8> {
        let (w, h) = (self.cols * self.cell_w, self.rows * self.cell_h);
        let mut out = vec![0u8; (w * h) as usize * 3];
        for r in 0..self.rows {
            for c in 0..self.cols {
                let bg = self.cells[(r * self.cols + c) as usize].unwrap_or(self.default_bg);
                for y in r * self.cell_h..(r + 1) * self.cell_h {
                    for x in c * self.cell_w..(c + 1) * self.cell_w {
                        let i = ((y * w + x) * 3) as usize;
                        out[i..i + 3].copy_from_slice(&bg);
                    }
                }
            }
        }
        let mut order: Vec<(&u64, &SimPlacement)> = self.placements.iter().collect();
        order.sort_by_key(|(_, p)| (p.z, p.image));
        let wezterm = self.flavor == Flavor::WezTerm;
        for (uid, p) in order {
            let img = &self.images[&p.image];
            let (dx, dy, tw, th) = p.pixel_rect(self.cell_w, self.cell_h);
            let (sx0, sy0, sw, sh) = p.src;
            let natural = !p.scaled;
            for y in 0..th.min(h.saturating_sub(dy)) {
                for x in 0..tw.min(w.saturating_sub(dx)) {
                    if wezterm {
                        let cell = ((p.row + y / self.cell_h) * self.cols + p.col + x / self.cell_w) as usize;
                        if !self.by_cell[cell].contains(uid) {
                            continue;
                        }
                    }
                    let (sx, sy) = if natural { (sx0 + x, sy0 + y) } else { (sx0 + x * sw / tw, sy0 + y * sh / th) };
                    let s = ((sy * img.w + sx) * 3) as usize;
                    let d = (((dy + y) * w + dx + x) * 3) as usize;
                    out[d..d + 3].copy_from_slice(&img.rgb[s..s + 3]);
                }
            }
        }
        out
    }

    /// Checks that every chunked upload has been completed.
    pub fn upload_in_progress(&self) -> bool {
        self.upload.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::{self, Delete, Encoding, Image, Placement};

    fn tiny() -> SimTerm {
        SimTerm::new(4, 3, 2, 2)
    }

    fn rgb_image(w: u32, h: u32, c: [u8; 3]) -> Vec<u8> {
        c.repeat((w * h) as usize)
    }

    #[test]
    fn transmit_place_and_render() {
        let mut t = tiny();
        let data = rgb_image(2, 2, [9, 8, 7]);
        let mut out = b"\x1b[2;3H".to_vec();
        kitty::transmit(
            &mut out,
            &Image { id: 5, width: 2, height: 2, data: &data, encoding: Encoding::Raw },
            Some(&Placement::default()),
        );
        t.feed(&out);
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        let fb = t.render();
        let px = |x: u32, y: u32| &fb[((y * 8 + x) * 3) as usize..((y * 8 + x) * 3 + 3) as usize];
        assert_eq!(px(4, 2), [9, 8, 7]);
        assert_eq!(px(3, 2), [0, 0, 0]);
    }

    #[test]
    fn flags_missing_quiet_and_cursor_keys() {
        let mut t = tiny();
        t.feed(b"\x1b_Ga=T,f=24,s=1,v=1,i=1;AAAA\x1b\\");
        assert!(t.errors[0].contains("q=2"));
        let mut t = tiny();
        t.feed(b"\x1b_Ga=T,f=24,s=1,v=1,i=1,q=2;AAAA\x1b\\");
        assert!(t.errors[0].contains("C=1"));
    }

    #[test]
    fn same_z_orders_by_image_id_not_placement_order() {
        let mut t = tiny();
        let mut out = Vec::new();
        let hi = rgb_image(2, 2, [200, 0, 0]);
        let lo = rgb_image(2, 2, [0, 200, 0]);
        kitty::transmit(
            &mut out,
            &Image { id: 9, width: 2, height: 2, data: &hi, encoding: Encoding::Raw },
            Some(&Placement::default()),
        );
        kitty::transmit(
            &mut out,
            &Image { id: 3, width: 2, height: 2, data: &lo, encoding: Encoding::Raw },
            Some(&Placement::default()),
        );
        t.feed(&out);
        assert_eq!(&t.render()[..3], &[200, 0, 0], "higher id wins even if placed first");
    }

    #[test]
    fn deletes_by_cursor_and_frees_data() {
        let mut t = tiny();
        let mut out = Vec::new();
        let d = rgb_image(2, 2, [1, 1, 1]);
        kitty::transmit(
            &mut out,
            &Image { id: 1, width: 2, height: 2, data: &d, encoding: Encoding::Raw },
            Some(&Placement::default()),
        );
        out.extend_from_slice(b"\x1b[1;2H");
        kitty::place(&mut out, 1, &Placement::default());
        t.feed(&out);
        assert_eq!(t.placement_count(), 2);
        let mut out = Vec::new();
        kitty::delete(&mut out, Delete::AtCursor);
        t.feed(&out);
        assert_eq!((t.placement_count(), t.image_count()), (1, 1));
        let mut out = Vec::new();
        kitty::delete(&mut out, Delete::AllVisible { free: true });
        t.feed(&out);
        assert_eq!((t.placement_count(), t.image_count()), (0, 0));
        assert!(t.errors.is_empty(), "{:?}", t.errors);
    }

    #[test]
    fn records_queries() {
        let mut t = tiny();
        t.feed(b"\x1b[16t\x1b[14t\x1b[?1016$p\x1b[>0q\x1b[c\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
        assert_eq!(
            t.queries,
            vec![
                Query::CellSize,
                Query::WindowSize,
                Query::Mode(1016),
                Query::Version,
                Query::PrimaryDeviceAttributes,
                Query::Graphics { id: 31 }
            ]
        );
    }

    #[test]
    fn ech_paints_current_background() {
        let mut t = tiny();
        t.feed(b"\x1b[2;2H\x1b[48;2;5;6;7m\x1b[2X\x1b[0m");
        let fb = t.render();
        let px = |x: u32, y: u32| {
            [fb[((y * 8 + x) * 3) as usize], fb[((y * 8 + x) * 3 + 1) as usize], fb[((y * 8 + x) * 3 + 2) as usize]]
        };
        assert_eq!(px(2, 2), [5, 6, 7]);
        assert_eq!(px(5, 3), [5, 6, 7]);
        assert_eq!(px(6, 2), [0, 0, 0]);
    }

    fn wezterm() -> SimTerm {
        let mut t = tiny();
        t.flavor = Flavor::WezTerm;
        t
    }

    /// Transmits a 1x1-cell image with id `id` and places it at (row, col).
    fn put(t: &mut SimTerm, id: u32, row: u32, col: u32, place: Placement) {
        let mut out = format!("\x1b[{};{}H", row + 1, col + 1).into_bytes();
        if !t.image_ids().contains(&id) {
            let data = rgb_image(2, 2, [id as u8; 3]);
            kitty::transmit(&mut out, &Image { id, width: 2, height: 2, data: &data, encoding: Encoding::Raw }, None);
        }
        kitty::place(&mut out, id, &place);
        t.feed(&out);
    }

    fn px(t: &SimTerm, row: u32, col: u32) -> u8 {
        t.render()[((row * 2 * 8 + col * 2) * 3) as usize]
    }

    #[test]
    fn wezterm_placement_without_id_drops_earlier_ones() {
        let mut t = wezterm();
        put(&mut t, 7, 0, 0, Placement::default());
        put(&mut t, 7, 1, 1, Placement::default());
        assert_eq!((px(&t, 0, 0), px(&t, 1, 1)), (0, 7), "only the last placement survives");
        put(&mut t, 7, 2, 2, Placement { placement_id: Some(2), ..Default::default() });
        put(&mut t, 7, 2, 3, Placement { placement_id: Some(3), ..Default::default() });
        assert_eq!((px(&t, 2, 2), px(&t, 2, 3)), (7, 7), "distinct placement ids coexist");
        assert!(t.errors.is_empty(), "{:?}", t.errors);
    }

    #[test]
    fn wezterm_rejects_positional_deletes_and_same_z_overlap() {
        let mut t = wezterm();
        put(&mut t, 7, 0, 0, Placement::default());
        let mut out = Vec::new();
        kitty::delete(&mut out, Delete::AtCursor);
        t.feed(&out);
        assert!(t.errors.last().is_some_and(|e| e.contains("ignores d=c")), "{:?}", t.errors);
        assert_eq!(px(&t, 0, 0), 7, "the ignored delete keeps the image");
        put(&mut t, 8, 0, 0, Placement::default());
        assert!(t.errors.last().is_some_and(|e| e.contains("order is undefined")), "{:?}", t.errors);
    }

    #[test]
    fn wezterm_erase_drops_cell_images() {
        let mut t = wezterm();
        put(&mut t, 7, 0, 0, Placement { cols: Some(2), rows: Some(1), ..Default::default() });
        t.feed(b"\x1b[1;2H\x1b[X");
        assert_eq!((px(&t, 0, 0), px(&t, 0, 1)), (7, 0), "only the erased cell loses the image");
        put(&mut t, 8, 0, 1, Placement::default());
        let mut out = Vec::new();
        kitty::delete(&mut out, Delete::AllVisible { free: true });
        t.feed(&out);
        assert_eq!((t.placement_count(), t.image_count()), (0, 0), "d=A frees every image in WezTerm");
        assert!(t.errors.is_empty(), "{:?}", t.errors);

        let mut k = tiny();
        put(&mut k, 7, 0, 0, Placement::default());
        k.feed(b"\x1b[1;1H\x1b[X");
        assert_eq!(px(&k, 0, 0), 7, "kitty keeps images above erased text");
    }

    #[test]
    fn wezterm_ech_keeps_placement_id_slices_but_el_drops_them() {
        let mut t = wezterm();
        put(&mut t, 7, 0, 0, Placement { placement_id: Some(1), ..Default::default() });
        t.feed(b"\x1b[1;1H\x1b[X");
        assert_eq!(px(&t, 0, 0), 7, "WezTerm copies placement-id slices into the erased cell");
        t.feed(b"\x1b[1;1H\x1b[K");
        assert_eq!(px(&t, 0, 0), 0, "clearing the line drops every slice");
        assert!(t.errors.is_empty(), "{:?}", t.errors);
    }

    /// WezTerm copies the placement-id slices of a cell back into it whenever an image is
    /// placed over the cell, so they double with every such placement.
    #[test]
    fn wezterm_rejects_placing_over_placement_id_slices() {
        let mut t = wezterm();
        put(&mut t, 7, 0, 0, Placement { placement_id: Some(1), ..Default::default() });
        put(&mut t, 8, 0, 0, Placement { z: 1, ..Default::default() });
        assert!(t.errors.last().is_some_and(|e| e.contains("duplicates")), "{:?}", t.errors);

        let mut t = wezterm();
        put(&mut t, 7, 0, 0, Placement::default());
        put(&mut t, 8, 0, 0, Placement { z: 1, placement_id: Some(1), ..Default::default() });
        put(&mut t, 7, 0, 0, Placement::default());
        assert!(t.errors.last().is_some_and(|e| e.contains("duplicates")), "moving an image under one: {:?}", t.errors);

        let mut t = wezterm();
        put(&mut t, 8, 0, 0, Placement { placement_id: Some(1), ..Default::default() });
        put(&mut t, 8, 0, 0, Placement { placement_id: Some(1), ..Default::default() });
        assert!(t.errors.is_empty(), "re-placing the same placement removes it first: {:?}", t.errors);
    }

    /// A shifted image covers one cell more than its size, but WezTerm attaches it only
    /// to the cells of its size; the last slice draws into the next cell.
    #[test]
    fn wezterm_slices_ignore_the_cell_offset() {
        let mut t = wezterm();
        let data = rgb_image(2, 2, [9; 3]);
        let mut out = b"\x1b[1;1H".to_vec();
        kitty::transmit(&mut out, &Image { id: 3, width: 2, height: 2, data: &data, encoding: Encoding::Raw }, None);
        kitty::place(&mut out, 3, &Placement { offset: (0, 1), ..Default::default() });
        t.feed(&out);
        let col0 = |t: &SimTerm| (0..4).map(|y| t.render()[(y * 8 * 3) as usize]).collect::<Vec<_>>();
        assert_eq!(col0(&t), [0, 9, 9, 0], "drawn 1 px lower");
        t.feed(b"\x1b[2;1H\x1b[K");
        assert_eq!(col0(&t), [0, 9, 9, 0], "the slice belongs to row 0, so clearing row 1 keeps it");
        t.feed(b"\x1b[1;1H\x1b[K");
        assert_eq!(col0(&t), [0; 4], "clearing row 0 drops the whole slice");
        assert!(t.errors.is_empty(), "{:?}", t.errors);
    }

    #[test]
    fn source_crop_and_cell_offset() {
        for flavor in [Flavor::Kitty, Flavor::WezTerm] {
            let mut t = tiny();
            t.flavor = flavor;
            // 8x6 image: row y has colour 10+y.
            let data: Vec<u8> = (0..6u8).flat_map(|y| [10 + y; 3].repeat(8)).collect();
            let mut out = Vec::new();
            kitty::transmit(
                &mut out,
                &Image { id: 3, width: 8, height: 6, data: &data, encoding: Encoding::Raw },
                None,
            );
            // Rows 2..5 of the image, 1 px below the top of cell row 0; then rows 0..1 right under it.
            out.extend_from_slice(b"\x1b[1;1H");
            let top =
                Placement { source: Some((0, 2, 8, 3)), offset: (0, 1), placement_id: Some(1), ..Default::default() };
            kitty::place(&mut out, 3, &top);
            out.extend_from_slice(b"\x1b[3;1H");
            let below = Placement { source: Some((0, 0, 8, 2)), placement_id: Some(2), ..Default::default() };
            kitty::place(&mut out, 3, &below);
            t.feed(&out);
            assert!(t.errors.is_empty(), "{flavor:?}: {:?}", t.errors);
            let fb = t.render();
            let col = |y: u32| fb[(y * 8 * 3) as usize];
            assert_eq!((0..6).map(col).collect::<Vec<_>>(), [0, 12, 13, 14, 10, 11], "{flavor:?}");
        }
    }

    #[test]
    fn wezterm_allows_pixel_disjoint_images_in_one_cell() {
        let mut t = wezterm();
        let data = rgb_image(8, 3, [5, 5, 5]);
        let mut out = Vec::new();
        for id in [1, 2] {
            kitty::transmit(&mut out, &Image { id, width: 8, height: 3, data: &data, encoding: Encoding::Raw }, None);
        }
        out.extend_from_slice(b"\x1b[1;1H");
        kitty::place(&mut out, 1, &Placement { source: Some((0, 0, 8, 1)), ..Default::default() });
        kitty::place(&mut out, 2, &Placement { source: Some((0, 0, 8, 1)), offset: (0, 1), ..Default::default() });
        t.feed(&out);
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        let mut out = b"\x1b[1;1H".to_vec();
        kitty::transmit(&mut out, &Image { id: 4, width: 8, height: 3, data: &data, encoding: Encoding::Raw }, None);
        kitty::place(&mut out, 4, &Placement { source: Some((0, 0, 8, 2)), ..Default::default() });
        t.feed(&out);
        assert!(t.errors.last().is_some_and(|e| e.contains("order is undefined")), "{:?}", t.errors);
    }
}
