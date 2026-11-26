//! zlib (RFC 1950) compression. Large inputs are deflated in parallel the way pigz does
//! it: independent raw-deflate chunks that end on a byte boundary (sync flush), joined
//! under one header and followed by the Adler-32 of the whole input.

use crate::par;
use flate2::{Compress, Compression, FlushCompress, Status};

/// Smallest chunk worth a thread; smaller chunks also cost compression ratio.
const PAR_MIN_CHUNK: usize = 256 << 10;
const ZLIB_HEADER: [u8; 2] = [0x78, 0x9c];
const ADLER_BASE: u32 = 65521;

pub struct Deflater {
    level: u32,
    serial: Compress,
}

impl Deflater {
    pub fn new(level: u32) -> Self {
        let level = level.min(9);
        Self { level, serial: Compress::new(Compression::new(level), true) }
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    /// Replaces `out` with the zlib stream of `data`.
    pub fn compress(&mut self, data: &[u8], out: &mut Vec<u8>, threads: usize) {
        out.clear();
        if threads > 1 && data.len() >= 2 * PAR_MIN_CHUNK {
            self.compress_parallel(data, out, threads);
            return;
        }
        self.serial.reset();
        deflate_into(&mut self.serial, data, out, FlushCompress::Finish);
    }

    fn compress_parallel(&self, data: &[u8], out: &mut Vec<u8>, threads: usize) {
        let n = data.len();
        let parts = par::map_ranges(n, threads, PAR_MIN_CHUNK, |r| {
            let flush = if r.end == n { FlushCompress::Finish } else { FlushCompress::Sync };
            let mut c = Compress::new(Compression::new(self.level), false);
            let mut z = Vec::new();
            deflate_into(&mut c, &data[r.clone()], &mut z, flush);
            (z, adler32(&data[r.clone()]), r.len())
        });
        out.reserve(parts.iter().map(|p| p.0.len()).sum::<usize>() + 6);
        out.extend_from_slice(&ZLIB_HEADER);
        let mut adler = 1;
        for (z, a, len) in &parts {
            out.extend_from_slice(z);
            adler = adler32_combine(adler, *a, *len);
        }
        out.extend_from_slice(&adler.to_be_bytes());
    }
}

fn deflate_into(c: &mut Compress, input: &[u8], out: &mut Vec<u8>, flush: FlushCompress) {
    let start = c.total_in();
    out.reserve(input.len() / 4 + 64);
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(4096));
        }
        let consumed = (c.total_in() - start) as usize;
        let status = c.compress_vec(&input[consumed..], out, flush).expect("in-memory deflate cannot fail");
        let done_input = (c.total_in() - start) as usize == input.len();
        match status {
            Status::StreamEnd => return,
            // A flush is complete once deflate stops with room left in the output.
            _ if done_input && out.len() < out.capacity() && flush != FlushCompress::Finish => return,
            _ => {}
        }
    }
}

pub fn adler32(data: &[u8]) -> u32 {
    // Largest block before the sums can overflow u32.
    const NMAX: usize = 5552;
    let (mut a, mut b) = (1u32, 0u32);
    for block in data.chunks(NMAX) {
        for &x in block {
            a += x as u32;
            b += a;
        }
        a %= ADLER_BASE;
        b %= ADLER_BASE;
    }
    (b << 16) | a
}

/// Adler-32 of `A ++ B` from the checksums of A and B (zlib's `adler32_combine`).
pub fn adler32_combine(adler_a: u32, adler_b: u32, len_b: usize) -> u32 {
    let rem = (len_b % ADLER_BASE as usize) as u32;
    let mut sum1 = adler_a & 0xffff;
    let mut sum2 = rem * sum1 % ADLER_BASE;
    sum1 += (adler_b & 0xffff) + ADLER_BASE - 1;
    sum2 += ((adler_a >> 16) & 0xffff) + ((adler_b >> 16) & 0xffff) + ADLER_BASE - rem;
    if sum1 >= ADLER_BASE {
        sum1 -= ADLER_BASE;
    }
    if sum1 >= ADLER_BASE {
        sum1 -= ADLER_BASE;
    }
    if sum2 >= ADLER_BASE << 1 {
        sum2 -= ADLER_BASE << 1;
    }
    if sum2 >= ADLER_BASE {
        sum2 -= ADLER_BASE;
    }
    sum1 | (sum2 << 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    fn inflate(z: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ZlibDecoder::new(z).read_to_end(&mut out).expect("valid zlib stream");
        out
    }

    fn sample(n: usize) -> Vec<u8> {
        let mut s = 0x9e37_79b9u32;
        (0..n)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                // Compressible with some noise, like screen content.
                if i % 97 < 60 {
                    (i / 7) as u8
                } else {
                    s as u8
                }
            })
            .collect()
    }

    #[test]
    fn adler32_known_value_and_combine() {
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        let data = sample(100_000);
        let (a, b) = data.split_at(33_333);
        assert_eq!(adler32_combine(adler32(a), adler32(b), b.len()), adler32(&data));
        assert_eq!(adler32_combine(1, adler32(b), b.len()), adler32(b));
    }

    #[test]
    fn parallel_stream_is_valid_zlib() {
        let data = sample(3 * PAR_MIN_CHUNK + 12_345);
        let mut d = Deflater::new(6);
        let (mut serial, mut parallel) = (Vec::new(), Vec::new());
        d.compress(&data, &mut serial, 1);
        d.compress(&data, &mut parallel, 4);
        assert_ne!(serial, parallel, "the parallel path must actually run");
        assert_eq!(inflate(&serial), data);
        assert_eq!(inflate(&parallel), data);
        assert!(parallel.len() < serial.len() * 11 / 10, "chunking costs little ratio");
    }

    #[test]
    fn small_and_empty_inputs() {
        let mut d = Deflater::new(1);
        let mut out = Vec::new();
        for n in [0, 1, 100, 70_000] {
            let data = sample(n);
            d.compress(&data, &mut out, 8);
            assert_eq!(inflate(&out), data, "n={n}");
        }
    }
}
