//! Minimal PNG (8-bit RGB, not interlaced, filter type 0) around a zlib stream.
//!
//! Ghostty 1.3 inflates `o=z` payloads with Zig 0.15's `std.compress.flate`, whose
//! fixed-Huffman path writes a match without checking the room left in its 64 KiB
//! window and reaches `unreachable` (a crash in release builds). PNG payloads are
//! decoded by wuffs instead, so large images go to Ghostty as PNG.

use flate2::read::ZlibDecoder;
use std::io::Read;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
/// IHDR bytes after width and height: 8-bit depth, RGB, deflate, filter method 0, no interlace.
const IHDR_RGB8: [u8; 5] = [8, 2, 0, 0, 0];
const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}

/// CRC-32 (ISO 3309) of `data` continued from `crc`, like zlib's `crc32(crc, buf)`.
fn crc32(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// The zlib input for an RGB image: every row prefixed with filter type 0 (none).
pub fn scanlines(rgb: &[u8], width: u32, out: &mut Vec<u8>) {
    let stride = width as usize * 3;
    out.clear();
    if stride == 0 {
        return;
    }
    out.reserve(rgb.len() + rgb.len() / stride);
    for row in rgb.chunks_exact(stride) {
        out.push(0);
        out.extend_from_slice(row);
    }
}

/// Replaces `out` with a PNG whose image data is `zlib`, the stream of [`scanlines`].
pub fn encode(width: u32, height: u32, zlib: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(zlib.len() + 57);
    out.extend_from_slice(&SIGNATURE);
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8..].copy_from_slice(&IHDR_RGB8);
    chunk(out, b"IHDR", &ihdr);
    chunk(out, b"IDAT", zlib);
    chunk(out, b"IEND", &[]);
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).ok().filter(|&n| n < 1 << 31).expect("PNG chunk over 2 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(crc32(0, kind), data).to_be_bytes());
}

/// Decodes what [`encode`] writes into `(width, height, rgb)`; anything else is an error.
pub fn decode(png: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut rest = png.strip_prefix(&SIGNATURE).ok_or("not a PNG (bad signature)")?;
    let mut size = None;
    let mut idat = Vec::new();
    loop {
        if rest.len() < 12 {
            return Err("truncated PNG chunk".into());
        }
        let len = u32::from_be_bytes(rest[..4].try_into().expect("4 bytes")) as usize;
        let kind: [u8; 4] = rest[4..8].try_into().expect("4 bytes");
        let name = String::from_utf8_lossy(&kind).into_owned();
        let body = rest.get(8..8 + len + 4).ok_or_else(|| format!("truncated PNG chunk {name}"))?;
        let (data, crc) = body.split_at(len);
        if u32::from_be_bytes(crc.try_into().expect("4 bytes")) != crc32(crc32(0, &kind), data) {
            return Err(format!("bad CRC in PNG chunk {name}"));
        }
        rest = &rest[12 + len..];
        if size.is_none() && &kind != b"IHDR" {
            return Err(format!("PNG chunk {name} before IHDR"));
        }
        match &kind {
            b"IHDR" if size.is_some() => return Err("second IHDR".into()),
            b"IHDR" => {
                if len != 13 || data[8..] != IHDR_RGB8 {
                    return Err("only 8-bit RGB PNG without interlace is modelled".into());
                }
                let w = u32::from_be_bytes(data[..4].try_into().expect("4 bytes"));
                let h = u32::from_be_bytes(data[4..8].try_into().expect("4 bytes"));
                if w == 0 || h == 0 {
                    return Err(format!("PNG of {w}x{h} pixels"));
                }
                size = Some((w, h));
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ if kind[0] & 0x20 == 0 => return Err(format!("unknown critical PNG chunk {name}")),
            _ => {}
        }
    }
    if !rest.is_empty() {
        return Err("data after IEND".into());
    }
    let (w, h) = size.expect("IHDR seen");
    let mut raw = Vec::new();
    ZlibDecoder::new(&idat[..]).read_to_end(&mut raw).map_err(|e| format!("PNG zlib: {e}"))?;
    let stride = w as usize * 3;
    if raw.len() != (stride + 1) * h as usize {
        return Err(format!("PNG data is {} bytes for {w}x{h}", raw.len()));
    }
    let mut rgb = Vec::with_capacity(stride * h as usize);
    for line in raw.chunks_exact(stride + 1) {
        if line[0] != 0 {
            return Err(format!("PNG filter type {} is not modelled", line[0]));
        }
        rgb.extend_from_slice(&line[1..]);
    }
    Ok((w, h, rgb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zlib::Deflater;

    fn image(w: u32, h: u32) -> Vec<u8> {
        (0..w * h * 3).map(|i| ((i % 251) ^ (i / 997)) as u8).collect()
    }

    fn png_of(rgb: &[u8], w: u32, h: u32, threads: usize) -> Vec<u8> {
        let (mut rows, mut z, mut png) = (Vec::new(), Vec::new(), Vec::new());
        scanlines(rgb, w, &mut rows);
        Deflater::new(6).compress(&rows, &mut z, threads);
        encode(w, h, &z, &mut png);
        png
    }

    #[test]
    fn crc32_known_values() {
        assert_eq!(crc32(0, b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xcbf4_3926);
        // The CRC every PNG ends with.
        assert_eq!(crc32(0, b"IEND"), 0xae42_6082);
    }

    #[test]
    fn round_trip_serial_and_parallel() {
        for (w, h, threads) in [(1, 1, 1), (37, 5, 1), (1500, 400, 8)] {
            let rgb = image(w, h);
            let png = png_of(&rgb, w, h, threads);
            assert_eq!(&png[..8], &SIGNATURE);
            assert_eq!(png[12..16], *b"IHDR");
            assert_eq!(decode(&png), Ok((w, h, rgb)), "{w}x{h}");
        }
    }

    #[test]
    fn decode_rejects_damage() {
        let png = png_of(&image(20, 10), 20, 10, 1);
        let mut bad_crc = png.clone();
        let last = bad_crc.len() - 20;
        bad_crc[last] ^= 1;
        assert!(decode(&bad_crc).unwrap_err().contains("bad CRC"));
        assert!(decode(&png[..png.len() - 1]).unwrap_err().contains("truncated"));
        let mut trailing = png.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        assert!(decode(b"GIF89a").unwrap_err().contains("signature"));
    }
}
