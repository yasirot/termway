//! Kitty graphics protocol command encoding.
//!
//! Every command carries `q=2` so the terminal never writes responses into our stdin,
//! and placements use `C=1` so the cursor never moves (no scrolling at the bottom row).

use crate::base64;
use std::io::Write;

/// Maximum base64 payload per escape sequence, as required by the protocol.
pub const CHUNK_SIZE: usize = 4096;
const RAW_PER_CHUNK: usize = CHUNK_SIZE / 4 * 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// Scale the image to this many cells (`c`/`r` keys); `None` keeps the natural size.
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub z: i32,
    pub placement_id: Option<u32>,
    /// Part of the image to show (`x`,`y`,`w`,`h` keys, image pixels).
    pub source: Option<(u32, u32, u32, u32)>,
    /// Pixel offset inside the cursor cell (`X`/`Y` keys), below the cell size.
    pub offset: (u32, u32),
}

impl Placement {
    fn write_keys(&self, out: &mut Vec<u8>) {
        if let Some((x, y, w, h)) = self.source {
            if x != 0 {
                let _ = write!(out, ",x={x}");
            }
            if y != 0 {
                let _ = write!(out, ",y={y}");
            }
            let _ = write!(out, ",w={w},h={h}");
        }
        if self.offset.0 != 0 {
            let _ = write!(out, ",X={}", self.offset.0);
        }
        if self.offset.1 != 0 {
            let _ = write!(out, ",Y={}", self.offset.1);
        }
        if let Some(c) = self.cols {
            let _ = write!(out, ",c={c}");
        }
        if let Some(r) = self.rows {
            let _ = write!(out, ",r={r}");
        }
        if self.z != 0 {
            let _ = write!(out, ",z={}", self.z);
        }
        if let Some(p) = self.placement_id {
            let _ = write!(out, ",p={p}");
        }
    }
}

/// How [`Image::data`] encodes the pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// 24-bit RGB (`f=24`).
    Raw,
    /// zlib-compressed 24-bit RGB (`f=24,o=z`).
    Zlib,
    /// A PNG file (`f=100`), which carries its own size.
    Png,
}

#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
    pub encoding: Encoding,
}

/// Transmits an image (`a=t`), or transmits and displays it at the cursor (`a=T`).
pub fn transmit(out: &mut Vec<u8>, img: &Image, place: Option<&Placement>) {
    let action = if place.is_some() { 'T' } else { 't' };
    match img.encoding {
        Encoding::Png => {
            let _ = write!(out, "\x1b_Ga={action},f=100,i={}", img.id);
        }
        Encoding::Raw | Encoding::Zlib => {
            let _ = write!(out, "\x1b_Ga={action},f=24,s={},v={},i={}", img.width, img.height, img.id);
        }
    }
    if img.encoding == Encoding::Zlib {
        out.extend_from_slice(b",o=z");
    }
    out.extend_from_slice(b",q=2");
    if let Some(p) = place {
        out.extend_from_slice(b",C=1");
        p.write_keys(out);
    }
    let mut chunks = img.data.chunks(RAW_PER_CHUNK).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(b";\x1b\\");
        return;
    }
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = chunks.peek().is_some();
        if !first {
            out.extend_from_slice(b"\x1b_Gq=2");
        }
        if more {
            out.extend_from_slice(b",m=1");
        } else if !first {
            out.extend_from_slice(b",m=0");
        }
        out.push(b';');
        base64::encode_into(chunk, out);
        out.extend_from_slice(b"\x1b\\");
        first = false;
    }
}

/// Displays an already transmitted image at the cursor.
pub fn place(out: &mut Vec<u8>, id: u32, p: &Placement) {
    let _ = write!(out, "\x1b_Ga=p,i={id},q=2,C=1");
    p.write_keys(out);
    out.extend_from_slice(b"\x1b\\");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delete {
    /// One placement; the image data stays cached in the terminal.
    Placement { id: u32, placement_id: u32 },
    /// An image with all its placements, freeing its data.
    Image { id: u32 },
    /// Every placement intersecting the cursor cell, keeping data.
    AtCursor,
    /// Placements with z-index `z` intersecting cell (`col`, `row`), 0-based, keeping data.
    AtCellZ { col: u32, row: u32, z: i32 },
    /// All visible placements; `free` also releases their image data.
    AllVisible { free: bool },
    /// All images whose id is in `from..=to`; `free` also releases their data.
    Range { from: u32, to: u32, free: bool },
    /// All placements with z-index `z`, keeping data.
    ZIndex { z: i32 },
    /// All placements intersecting cell row `row` (0-based), keeping data.
    Row { row: u32 },
}

pub fn delete(out: &mut Vec<u8>, d: Delete) {
    let _ = match d {
        Delete::Placement { id, placement_id } => write!(out, "\x1b_Ga=d,d=i,i={id},p={placement_id},q=2\x1b\\"),
        Delete::Image { id } => write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\"),
        Delete::AtCursor => write!(out, "\x1b_Ga=d,d=c,q=2\x1b\\"),
        Delete::AtCellZ { col, row, z } => write!(out, "\x1b_Ga=d,d=q,x={},y={},z={z},q=2\x1b\\", col + 1, row + 1),
        Delete::AllVisible { free } => write!(out, "\x1b_Ga=d,d={},q=2\x1b\\", if free { 'A' } else { 'a' }),
        Delete::Range { from, to, free } => {
            write!(out, "\x1b_Ga=d,d={},x={from},y={to},q=2\x1b\\", if free { 'R' } else { 'r' })
        }
        Delete::ZIndex { z } => write!(out, "\x1b_Ga=d,d=z,z={z},q=2\x1b\\"),
        Delete::Row { row } => write!(out, "\x1b_Ga=d,d=y,y={},q=2\x1b\\", row + 1),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Splits a byte stream into (control keys, payload) of each `ESC _ G ... ESC \`.
    fn parse(out: &[u8]) -> Vec<(HashMap<String, String>, Vec<u8>)> {
        let s = std::str::from_utf8(out).unwrap();
        let mut cmds = Vec::new();
        let mut rest = s;
        while let Some(start) = rest.find("\x1b_G") {
            let body_start = start + 3;
            let end = rest[body_start..].find("\x1b\\").expect("unterminated APC") + body_start;
            let body = &rest[body_start..end];
            let (ctrl, payload) = body.split_once(';').unwrap_or((body, ""));
            let keys = ctrl
                .split(',')
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap();
                    (k.to_string(), v.to_string())
                })
                .collect();
            cmds.push((keys, payload.as_bytes().to_vec()));
            rest = &rest[end + 2..];
        }
        assert!(!rest.contains('\x1b'), "stray escape after last command");
        cmds
    }

    fn image(data: &[u8]) -> Image<'_> {
        Image { id: 42, width: 3, height: 2, data, encoding: Encoding::Zlib }
    }

    #[test]
    fn single_chunk_transmit_has_all_keys() {
        let mut out = Vec::new();
        transmit(&mut out, &image(b"hello"), Some(&Placement::default()));
        let cmds = parse(&out);
        assert_eq!(cmds.len(), 1);
        let (k, payload) = &cmds[0];
        for (key, val) in
            [("a", "T"), ("f", "24"), ("s", "3"), ("v", "2"), ("i", "42"), ("o", "z"), ("q", "2"), ("C", "1")]
        {
            assert_eq!(k.get(key).map(String::as_str), Some(val), "key {key}");
        }
        assert!(!k.contains_key("m"), "single chunk needs no m key");
        assert_eq!(base64::decode(payload).unwrap(), b"hello");
    }

    #[test]
    fn transmit_only_omits_cursor_key() {
        let mut out = Vec::new();
        transmit(&mut out, &Image { encoding: Encoding::Raw, ..image(b"x") }, None);
        let (k, _) = &parse(&out)[0];
        assert_eq!(k["a"], "t");
        assert!(!k.contains_key("C") && !k.contains_key("o"));
    }

    #[test]
    fn png_carries_no_size_or_compression_keys() {
        let mut out = Vec::new();
        transmit(&mut out, &Image { encoding: Encoding::Png, ..image(b"png") }, Some(&Placement::default()));
        let (k, payload) = &parse(&out)[0];
        assert_eq!((k["a"].as_str(), k["f"].as_str(), k["i"].as_str()), ("T", "100", "42"));
        assert!(!k.contains_key("s") && !k.contains_key("v") && !k.contains_key("o"));
        assert_eq!(base64::decode(payload).unwrap(), b"png");
    }

    #[test]
    fn chunking_rules() {
        for len in [RAW_PER_CHUNK - 1, RAW_PER_CHUNK, RAW_PER_CHUNK + 1, 3 * RAW_PER_CHUNK + 17] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            let mut out = Vec::new();
            transmit(&mut out, &image(&data), Some(&Placement::default()));
            let cmds = parse(&out);
            assert_eq!(cmds.len(), len.div_ceil(RAW_PER_CHUNK), "len {len}");
            let mut joined = Vec::new();
            for (i, (k, payload)) in cmds.iter().enumerate() {
                let last = i + 1 == cmds.len();
                assert!(payload.len() <= CHUNK_SIZE);
                if !last {
                    assert_eq!(payload.len() % 4, 0, "non-final chunks must be a multiple of 4");
                }
                if cmds.len() > 1 {
                    assert_eq!(k["m"], if last { "0" } else { "1" });
                }
                if i > 0 {
                    let mut keys: Vec<_> = k.keys().map(String::as_str).collect();
                    keys.sort();
                    assert_eq!(keys, ["m", "q"], "continuation chunks carry only m and q");
                }
                assert_eq!(k["q"], "2");
                joined.extend_from_slice(payload);
            }
            assert_eq!(base64::decode(&joined).unwrap(), data);
        }
    }

    #[test]
    fn empty_payload_is_well_formed() {
        let mut out = Vec::new();
        transmit(&mut out, &image(b""), None);
        assert_eq!(parse(&out).len(), 1);
    }

    #[test]
    fn placement_keys() {
        let mut out = Vec::new();
        place(
            &mut out,
            7,
            &Placement { cols: Some(4), rows: Some(2), z: 1, placement_id: Some(9), ..Default::default() },
        );
        let (k, payload) = &parse(&out)[0];
        assert!(payload.is_empty());
        for (key, val) in
            [("a", "p"), ("i", "7"), ("c", "4"), ("r", "2"), ("z", "1"), ("p", "9"), ("q", "2"), ("C", "1")]
        {
            assert_eq!(k[key], val, "key {key}");
        }
        let mut out = Vec::new();
        place(&mut out, 7, &Placement::default());
        let (k, _) = &parse(&out)[0];
        assert!(!k.contains_key("z") && !k.contains_key("c") && !k.contains_key("p"));
        let mut out = Vec::new();
        place(&mut out, 7, &Placement { source: Some((0, 30, 800, 12)), offset: (0, 5), ..Default::default() });
        let (k, _) = &parse(&out)[0];
        for (key, val) in [("y", "30"), ("w", "800"), ("h", "12"), ("Y", "5")] {
            assert_eq!(k[key], val, "key {key}");
        }
        assert!(!k.contains_key("x") && !k.contains_key("X"), "zero values are left out");
    }

    #[test]
    fn delete_variants() {
        let cases: [(Delete, &[(&str, &str)]); 9] = [
            (Delete::ZIndex { z: 1 }, &[("d", "z"), ("z", "1")]),
            (Delete::Row { row: 0 }, &[("d", "y"), ("y", "1")]),
            (Delete::Placement { id: 3, placement_id: 4 }, &[("d", "i"), ("i", "3"), ("p", "4")]),
            (Delete::Image { id: 3 }, &[("d", "I"), ("i", "3")]),
            (Delete::AtCursor, &[("d", "c")]),
            (Delete::AtCellZ { col: 0, row: 4, z: 1 }, &[("d", "q"), ("x", "1"), ("y", "5"), ("z", "1")]),
            (Delete::AllVisible { free: true }, &[("d", "A")]),
            (Delete::AllVisible { free: false }, &[("d", "a")]),
            (Delete::Range { from: 1, to: 99, free: true }, &[("d", "R"), ("x", "1"), ("y", "99")]),
        ];
        for (d, expect) in cases {
            let mut out = Vec::new();
            delete(&mut out, d);
            let (k, _) = &parse(&out)[0];
            assert_eq!(k["a"], "d");
            assert_eq!(k["q"], "2");
            for (key, val) in expect {
                assert_eq!(k[*key], *val, "{d:?} key {key}");
            }
        }
    }
}
