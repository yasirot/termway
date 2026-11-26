//! VT output buffer that tracks the cursor so moves can use the shortest sequence.

use std::io::Write;

pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

#[derive(Debug, Default)]
pub struct Vt {
    pub buf: Vec<u8>,
    /// (row, col), 0-based; `None` when unknown.
    cursor: Option<(u32, u32)>,
    bg: Option<[u8; 3]>,
}

impl Vt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Takes the pending bytes; cursor/SGR tracking stays valid.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// Call after emitting anything whose effect on cursor or SGR state is not tracked.
    pub fn forget_state(&mut self) {
        self.cursor = None;
        self.bg = None;
    }

    pub fn move_to(&mut self, row: u32, col: u32) {
        let mut best: Vec<u8> = Vec::with_capacity(16);
        match (row, col) {
            (0, 0) => best.extend_from_slice(b"\x1b[H"),
            (r, 0) => {
                let _ = write!(best, "\x1b[{}H", r + 1);
            }
            (r, c) => {
                let _ = write!(best, "\x1b[{};{}H", r + 1, c + 1);
            }
        }
        if let Some((cr, cc)) = self.cursor {
            if cr == row {
                if cc == col {
                    return;
                }
                let mut rel = Vec::with_capacity(8);
                let (d, f) = if col > cc { (col - cc, 'C') } else { (cc - col, 'D') };
                if d == 1 {
                    let _ = write!(rel, "\x1b[{f}");
                } else {
                    let _ = write!(rel, "\x1b[{d}{f}");
                }
                if rel.len() < best.len() {
                    best = rel;
                }
            }
        }
        self.buf.extend_from_slice(&best);
        self.cursor = Some((row, col));
    }

    /// Paints `n` cells starting at (row, col) with a solid background using ECH,
    /// which erases with the current background colour and does not move the cursor.
    pub fn fill(&mut self, row: u32, col: u32, n: u32, rgb: [u8; 3]) {
        if n == 0 {
            return;
        }
        self.move_to(row, col);
        if self.bg != Some(rgb) {
            let _ = write!(self.buf, "\x1b[48;2;{};{};{}m", rgb[0], rgb[1], rgb[2]);
            self.bg = Some(rgb);
        }
        if n == 1 {
            self.buf.extend_from_slice(b"\x1b[X");
        } else {
            let _ = write!(self.buf, "\x1b[{n}X");
        }
    }

    /// Erases `n` cells starting at (row, col) to the default background (ECH).
    pub fn erase(&mut self, row: u32, col: u32, n: u32) {
        if n == 0 {
            return;
        }
        self.move_to(row, col);
        if self.bg.is_some() {
            self.buf.extend_from_slice(b"\x1b[49m");
            self.bg = None;
        }
        if n == 1 {
            self.buf.extend_from_slice(b"\x1b[X");
        } else {
            let _ = write!(self.buf, "\x1b[{n}X");
        }
    }

    pub fn reset_sgr(&mut self) {
        self.buf.extend_from_slice(b"\x1b[0m");
        self.bg = None;
    }

    /// Clears the whole screen with the default background.
    pub fn clear_screen(&mut self) {
        self.reset_sgr();
        self.buf.extend_from_slice(b"\x1b[2J");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &Vt) -> &str {
        std::str::from_utf8(&v.buf).unwrap()
    }

    #[test]
    fn absolute_moves() {
        let mut v = Vt::new();
        v.move_to(0, 0);
        v.move_to(4, 0);
        v.move_to(9, 19);
        assert_eq!(s(&v), "\x1b[H\x1b[5H\x1b[10;20H");
    }

    #[test]
    fn relative_moves_when_shorter() {
        let mut v = Vt::new();
        v.move_to(44, 199);
        v.buf.clear();
        v.move_to(44, 200);
        v.move_to(44, 209);
        v.move_to(44, 205);
        v.move_to(44, 205);
        assert_eq!(s(&v), "\x1b[C\x1b[9C\x1b[4D");
        v.buf.clear();
        v.forget_state();
        v.move_to(44, 206);
        assert_eq!(s(&v), "\x1b[45;207H");
    }

    #[test]
    fn erase_restores_default_background() {
        let mut v = Vt::new();
        v.fill(0, 0, 2, [1, 2, 3]);
        v.buf.clear();
        v.erase(0, 0, 3);
        v.erase(1, 0, 1);
        assert_eq!(s(&v), "\x1b[49m\x1b[3X\x1b[2H\x1b[X", "the cursor already sits at the fill");
    }

    #[test]
    fn fill_uses_ech_and_caches_sgr() {
        let mut v = Vt::new();
        v.fill(0, 0, 5, [1, 2, 3]);
        v.fill(1, 0, 1, [1, 2, 3]);
        v.fill(1, 3, 2, [4, 5, 6]);
        v.fill(1, 3, 0, [7, 7, 7]);
        assert_eq!(s(&v), "\x1b[H\x1b[48;2;1;2;3m\x1b[5X\x1b[2H\x1b[X\x1b[3C\x1b[48;2;4;5;6m\x1b[2X");
        v.clear_screen();
        v.buf.clear();
        v.fill(1, 3, 1, [4, 5, 6]);
        assert!(s(&v).contains("48;2;4;5;6"), "SGR must be re-sent after reset");
    }
}
