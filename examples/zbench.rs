//! Compression micro-benchmark on a real frame: `cargo run --release --example zbench FRAME.ppm`
//! Compares zlib levels for 1x1-cell tiles (Phase 3 misses) and for the whole frame (Phase 1).

use flate2::{Compress, Compression, FlushCompress, Status};
use std::time::Instant;
use termway::frame::{solid_color, FrameBuf};
use termway::geom::Rect;

fn read_ppm(path: &str) -> FrameBuf {
    let data = std::fs::read(path).expect("read ppm");
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while data[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&data[s..i]).into_owned());
    }
    let (w, h): (u32, u32) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    FrameBuf::from_rgb(w, h, data[i + 1..i + 1 + (w * h * 3) as usize].to_vec())
}

fn compress(c: &mut Compress, data: &[u8], out: &mut Vec<u8>) {
    c.reset();
    out.clear();
    out.reserve(data.len() + 64);
    loop {
        let consumed = c.total_in() as usize;
        if out.len() == out.capacity() {
            out.reserve(4096);
        }
        if c.compress_vec(&data[consumed..], out, FlushCompress::Finish).unwrap() == Status::StreamEnd {
            break;
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: zbench FRAME.ppm");
    let fb = read_ppm(&path);
    let (cw, ch) = (9, 21);
    let mut tiles = Vec::new();
    let mut buf = Vec::new();
    for ty in 0..fb.height() / ch {
        for tx in 0..fb.width() / cw {
            fb.extract(Rect::new(tx * cw, ty * ch, cw, ch), &mut buf);
            if solid_color(&buf).is_none() {
                tiles.push(buf.clone());
            }
        }
    }
    println!("{} non-uniform 9x21 tiles in {}x{}", tiles.len(), fb.width(), fb.height());
    let mut out = Vec::new();
    for level in [1, 3, 6, 9] {
        let mut c = Compress::new(Compression::new(level), true);
        let t = Instant::now();
        let mut total = 0;
        for tile in &tiles {
            compress(&mut c, tile, &mut out);
            total += out.len().min(tile.len());
        }
        let el = t.elapsed();
        println!(
            "tiles level {level}: {:.1} us/tile, {:.1} bytes/tile (raw {})",
            el.as_secs_f64() * 1e6 / tiles.len() as f64,
            total as f64 / tiles.len() as f64,
            tiles[0].len()
        );
    }
    for level in [1, 3, 6] {
        let mut c = Compress::new(Compression::new(level), true);
        let t = Instant::now();
        compress(&mut c, fb.data(), &mut out);
        println!("full frame level {level}: {:.1} ms, {} bytes", t.elapsed().as_secs_f64() * 1e3, out.len());
    }
}
