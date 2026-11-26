//! Packed RGB framebuffer mirrored from the compositor's 32-bit shm buffer.

use crate::geom::Rect;

/// Rows per worker below which a conversion stays on one thread.
const PAR_MIN_ROWS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Xrgb8888,
    Argb8888,
    Xbgr8888,
    Abgr8888,
}

impl PixelFormat {
    /// Maps a `wl_shm` format code; only 32-bit little-endian RGB layouts are supported.
    pub fn from_wl_shm(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Argb8888),
            1 => Some(Self::Xrgb8888),
            0x3432_4258 => Some(Self::Xbgr8888),
            0x3432_4241 => Some(Self::Abgr8888),
            _ => None,
        }
    }

    /// Byte offsets of R, G and B within a pixel in memory.
    fn rgb_offsets(self) -> [usize; 3] {
        match self {
            Self::Xrgb8888 | Self::Argb8888 => [2, 1, 0],
            Self::Xbgr8888 | Self::Abgr8888 => [0, 1, 2],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    BufferTooSmall { needed: usize, got: usize },
    BadStride { stride: usize, width: u32 },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BufferTooSmall { needed, got } => write!(f, "shm buffer too small: need {needed} bytes, got {got}"),
            Self::BadStride { stride, width } => write!(f, "stride {stride} too small for width {width}"),
        }
    }
}

impl std::error::Error for FrameError {}

#[derive(Clone, Debug, Default)]
pub struct FrameBuf {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl FrameBuf {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, data: vec![0; width as usize * height as usize * 3] }
    }

    pub fn from_rgb(width: u32, height: u32, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), width as usize * height as usize * 3);
        Self { width, height, data }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, rgb: [u8; 3]) {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        self.data[i..i + 3].copy_from_slice(&rgb);
    }

    /// Copies `rect` from a 32-bit shm buffer of the same dimensions.
    pub fn update_from_shm(
        &mut self,
        src: &[u8],
        stride: usize,
        fmt: PixelFormat,
        y_invert: bool,
        rect: Rect,
        threads: usize,
    ) -> Result<(), FrameError> {
        if stride < self.width as usize * 4 {
            return Err(FrameError::BadStride { stride, width: self.width });
        }
        let needed = stride * self.height as usize;
        if src.len() < needed {
            return Err(FrameError::BufferTooSmall { needed, got: src.len() });
        }
        let Some(r) = rect.intersect(&self.bounds()) else { return Ok(()) };
        let [ro, go, bo] = fmt.rgb_offsets();
        let (w, h) = (self.width as usize, self.height);
        let row_bytes = w * 3;
        let rows = &mut self.data[r.y as usize * row_bytes..(r.y + r.h) as usize * row_bytes];
        let (x0, x1) = (r.x as usize, (r.x + r.w) as usize);
        crate::par::for_chunks_mut(rows, row_bytes, threads, PAR_MIN_ROWS, |start, chunk| {
            for (i, d) in chunk.chunks_exact_mut(row_bytes).enumerate() {
                let y = r.y + (start / row_bytes + i) as u32;
                let sy = if y_invert { h - 1 - y } else { y } as usize;
                let s = &src[sy * stride + x0 * 4..sy * stride + x1 * 4];
                for (dp, sp) in d[x0 * 3..x1 * 3].chunks_exact_mut(3).zip(s.chunks_exact(4)) {
                    dp[0] = sp[ro];
                    dp[1] = sp[go];
                    dp[2] = sp[bo];
                }
            }
        });
        Ok(())
    }

    /// Packed RGB of `r`; pixels outside the framebuffer read as black.
    pub fn extract(&self, r: Rect, out: &mut Vec<u8>) {
        out.clear();
        out.resize(r.w as usize * r.h as usize * 3, 0);
        let Some(inside) = r.intersect(&self.bounds()) else { return };
        let w = self.width as usize;
        let row_len = inside.w as usize * 3;
        let dx = (inside.x - r.x) as usize * 3;
        for y in inside.y..inside.y + inside.h {
            let s = (y as usize * w + inside.x as usize) * 3;
            let d = (y - r.y) as usize * r.w as usize * 3 + dx;
            out[d..d + row_len].copy_from_slice(&self.data[s..s + row_len]);
        }
    }

    /// Writes the framebuffer as binary PPM (P6).
    pub fn write_ppm(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        write!(f, "P6\n{} {}\n255\n", self.width, self.height)?;
        f.write_all(&self.data)?;
        f.flush()
    }

    /// 2x2 box-filtered RGB of `r`, returning the output size (rounded up).
    pub fn extract_half(&self, r: Rect, full: &mut Vec<u8>, out: &mut Vec<u8>) -> (u32, u32) {
        if r.intersect(&self.bounds()) != Some(r) {
            self.extract(r, full);
            return downsample_half(full, r.w, r.h, out);
        }
        // Common case: read the rows in place instead of copying the rectangle first.
        let stride = self.width as usize * 3;
        let (x0, x1) = (r.x as usize * 3, (r.x + r.w) as usize * 3);
        let row = |y: u32| {
            let base = (r.y + y) as usize * stride;
            &self.data[base + x0..base + x1]
        };
        downsample_rows(r.w, r.h, row, out)
    }
}

pub fn downsample_half(src: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> (u32, u32) {
    let row_len = w as usize * 3;
    downsample_rows(w, h, |y| &src[y as usize * row_len..(y as usize + 1) * row_len], out)
}

/// Averages each 2x2 block, rounding half up. An odd last row or column averages the
/// pixels it has; repeating the lone row gives the same result as dividing by two.
fn downsample_rows<'a>(w: u32, h: u32, row: impl Fn(u32) -> &'a [u8], out: &mut Vec<u8>) -> (u32, u32) {
    let (w2, h2) = (w.div_ceil(2), h.div_ceil(2));
    out.clear();
    out.resize(w2 as usize * h2 as usize * 3, 0);
    if w2 == 0 {
        return (w2, h2);
    }
    let pairs = (w / 2) as usize;
    for (oy, o) in out.chunks_exact_mut(w2 as usize * 3).enumerate() {
        let y = oy as u32 * 2;
        let a = row(y);
        let b = if y + 1 < h { row(y + 1) } else { a };
        let (o_pairs, o_last) = o.split_at_mut(pairs * 3);
        for ((d, pa), pb) in o_pairs.chunks_exact_mut(3).zip(a.chunks_exact(6)).zip(b.chunks_exact(6)) {
            for c in 0..3 {
                let sum = pa[c] as u32 + pa[c + 3] as u32 + pb[c] as u32 + pb[c + 3] as u32;
                d[c] = ((sum + 2) >> 2) as u8;
            }
        }
        if w % 2 == 1 {
            let s = pairs * 6;
            for c in 0..3 {
                o_last[c] = ((a[s + c] as u32 + b[s + c] as u32 + 1) >> 1) as u8;
            }
        }
    }
    (w2, h2)
}

/// `Some(colour)` if every pixel of packed RGB data is identical.
pub fn solid_color(rgb: &[u8]) -> Option<[u8; 3]> {
    if rgb.len() < 3 || rgb.len() % 3 != 0 {
        return None;
    }
    // Period-3 data is exactly "all pixels equal"; this compiles to a memcmp.
    (rgb[3..] == rgb[..rgb.len() - 3]).then(|| [rgb[0], rgb[1], rgb[2]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xrgb(pixels: &[[u8; 3]]) -> Vec<u8> {
        pixels.iter().flat_map(|p| [p[2], p[1], p[0], 0xff]).collect()
    }

    #[test]
    fn converts_xrgb_and_xbgr() {
        let px = [[10, 20, 30], [40, 50, 60]];
        let mut fb = FrameBuf::new(2, 1);
        fb.update_from_shm(&xrgb(&px), 8, PixelFormat::Xrgb8888, false, fb.bounds(), 1).unwrap();
        assert_eq!(fb.data(), &[10, 20, 30, 40, 50, 60]);
        let xbgr: Vec<u8> = px.iter().flat_map(|p| [p[0], p[1], p[2], 0]).collect();
        let mut fb = FrameBuf::new(2, 1);
        fb.update_from_shm(&xbgr, 8, PixelFormat::Abgr8888, false, fb.bounds(), 1).unwrap();
        assert_eq!(fb.data(), &[10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn updates_only_damaged_rect_with_stride_and_invert() {
        // 2x2 image with 4 bytes of row padding.
        let rows = [[[1, 1, 1], [2, 2, 2]], [[3, 3, 3], [4, 4, 4]]];
        let mut src = Vec::new();
        for r in &rows {
            src.extend(xrgb(r));
            src.extend([0u8; 4]);
        }
        let mut fb = FrameBuf::new(2, 2);
        fb.update_from_shm(&src, 12, PixelFormat::Xrgb8888, false, Rect::new(1, 1, 5, 5), 1).unwrap();
        assert_eq!(fb.pixel(0, 0), [0, 0, 0]);
        assert_eq!(fb.pixel(1, 1), [4, 4, 4]);
        fb.update_from_shm(&src, 12, PixelFormat::Xrgb8888, true, fb.bounds(), 1).unwrap();
        assert_eq!(fb.pixel(0, 0), [3, 3, 3], "y_invert flips rows");
        assert_eq!(fb.pixel(1, 1), [2, 2, 2]);
    }

    #[test]
    fn rejects_bad_buffers() {
        let mut fb = FrameBuf::new(2, 2);
        assert_eq!(
            fb.update_from_shm(&[0; 8], 4, PixelFormat::Xrgb8888, false, fb.bounds(), 1),
            Err(FrameError::BadStride { stride: 4, width: 2 })
        );
        assert_eq!(
            fb.update_from_shm(&[0; 8], 8, PixelFormat::Xrgb8888, false, fb.bounds(), 1),
            Err(FrameError::BufferTooSmall { needed: 16, got: 8 })
        );
        assert_eq!(PixelFormat::from_wl_shm(0x3231_5258), None);
    }

    #[test]
    fn extract_pads_outside_with_black() {
        let mut fb = FrameBuf::new(2, 2);
        fb.set_pixel(1, 1, [9, 9, 9]);
        let mut out = Vec::new();
        fb.extract(Rect::new(1, 1, 2, 2), &mut out);
        assert_eq!(out, [9, 9, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        fb.extract(Rect::new(5, 5, 1, 1), &mut out);
        assert_eq!(out, [0, 0, 0]);
    }

    #[test]
    fn half_resolution_box_filter() {
        // 3x1: pixels 0,100 average to 50; the odd last pixel stays alone.
        let src = [0, 0, 0, 100, 100, 100, 7, 8, 9];
        let mut out = Vec::new();
        assert_eq!(downsample_half(&src, 3, 1, &mut out), (2, 1));
        assert_eq!(out, [50, 50, 50, 7, 8, 9]);
    }

    /// The straightforward per-pixel average the fast path must reproduce exactly.
    fn reference_half(src: &[u8], w: u32, h: u32) -> Vec<u8> {
        let (w, h) = (w as usize, h as usize);
        let mut out = Vec::new();
        for oy in 0..h.div_ceil(2) {
            for ox in 0..w.div_ceil(2) {
                let (mut acc, mut n) = ([0u32; 3], 0);
                for y in oy * 2..(oy * 2 + 2).min(h) {
                    for x in ox * 2..(ox * 2 + 2).min(w) {
                        (0..3).for_each(|c| acc[c] += src[(y * w + x) * 3 + c] as u32);
                        n += 1;
                    }
                }
                out.extend(acc.iter().map(|&a| ((a + n / 2) / n) as u8));
            }
        }
        out
    }

    #[test]
    fn half_resolution_matches_reference_for_odd_sizes_and_offsets() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let fb = FrameBuf::from_rgb(13, 9, (0..13 * 9 * 3).map(|_| rnd() as u8).collect());
        let (mut full, mut out) = (Vec::new(), Vec::new());
        for r in [Rect::new(0, 0, 13, 9), Rect::new(1, 2, 7, 5), Rect::new(3, 1, 6, 8), Rect::new(12, 8, 1, 1)] {
            fb.extract(r, &mut full);
            let want = reference_half(&full, r.w, r.h);
            assert_eq!(fb.extract_half(r, &mut Vec::new(), &mut out), (r.w.div_ceil(2), r.h.div_ceil(2)));
            assert_eq!(out, want, "{r:?}");
        }
        // Partly outside: padded with black like extract.
        let r = Rect::new(10, 6, 5, 5);
        fb.extract(r, &mut full);
        let want = reference_half(&full, r.w, r.h);
        fb.extract_half(r, &mut full, &mut out);
        assert_eq!(out, want);
    }

    #[test]
    fn parallel_conversion_matches_serial() {
        let (w, h) = (37u32, 300u32);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let rect = Rect::new(3, 5, 30, 290);
        let mut serial = FrameBuf::new(w, h);
        let mut parallel = FrameBuf::new(w, h);
        serial.update_from_shm(&src, w as usize * 4, PixelFormat::Xrgb8888, true, rect, 1).unwrap();
        parallel.update_from_shm(&src, w as usize * 4, PixelFormat::Xrgb8888, true, rect, 4).unwrap();
        assert!(serial.data() == parallel.data());
        assert_eq!(
            serial.pixel(3, 5),
            [src[(294 * w as usize + 3) * 4 + 2], src[(294 * w as usize + 3) * 4 + 1], src[(294 * w as usize + 3) * 4]]
        );
    }

    #[test]
    fn solid_detection() {
        assert_eq!(solid_color(&[1, 2, 3, 1, 2, 3, 1, 2, 3]), Some([1, 2, 3]));
        assert_eq!(solid_color(&[1, 2, 3]), Some([1, 2, 3]));
        assert_eq!(solid_color(&[1, 2, 3, 1, 2, 4]), None);
        assert_eq!(solid_color(&[3, 2, 1, 1, 2, 3]), None);
        assert_eq!(solid_color(&[]), None);
    }
}
