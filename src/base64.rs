//! Standard base64 (RFC 4648, with padding).

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

pub fn encode_into(input: &[u8], out: &mut Vec<u8>) {
    out.reserve(encoded_len(input.len()));
    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        let v = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        out.extend_from_slice(&[
            ALPHABET[(v >> 18) as usize & 63],
            ALPHABET[(v >> 12) as usize & 63],
            ALPHABET[(v >> 6) as usize & 63],
            ALPHABET[v as usize & 63],
        ]);
    }
    match *chunks.remainder() {
        [a] => {
            let v = (a as u32) << 16;
            out.extend_from_slice(&[ALPHABET[(v >> 18) as usize & 63], ALPHABET[(v >> 12) as usize & 63], b'=', b'=']);
        }
        [a, b] => {
            let v = (a as u32) << 16 | (b as u32) << 8;
            out.extend_from_slice(&[
                ALPHABET[(v >> 18) as usize & 63],
                ALPHABET[(v >> 12) as usize & 63],
                ALPHABET[(v >> 6) as usize & 63],
                b'=',
            ]);
        }
        _ => {}
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError;

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid base64 data")
    }
}

impl std::error::Error for DecodeError {}

fn value(c: u8) -> Result<u32, DecodeError> {
    Ok(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return Err(DecodeError),
    } as u32)
}

pub fn decode(input: &[u8]) -> Result<Vec<u8>, DecodeError> {
    if input.len() % 4 != 0 {
        return Err(DecodeError);
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let n = input.len() / 4;
    for (i, q) in input.chunks_exact(4).enumerate() {
        let last = i + 1 == n;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err(DecodeError);
        }
        let mut v = 0u32;
        for &c in &q[..4 - pad] {
            v = v << 6 | value(c)?;
        }
        v <<= 6 * pad as u32;
        let bytes = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(s: &[u8]) -> String {
        let mut v = Vec::new();
        encode_into(s, &mut v);
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn rfc4648_vectors() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(enc(plain.as_bytes()), coded);
            assert_eq!(decode(coded.as_bytes()).unwrap(), plain.as_bytes());
            assert_eq!(encoded_len(plain.len()), coded.len());
        }
    }

    #[test]
    fn roundtrip_all_bytes() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let mut v = Vec::new();
        encode_into(&data, &mut v);
        assert_eq!(decode(&v).unwrap(), data);
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(decode(b"abc"), Err(DecodeError));
        assert_eq!(decode(b"ab!d"), Err(DecodeError));
        assert_eq!(decode(b"a===",), Err(DecodeError));
        assert_eq!(decode(b"Zg==Zm8="), Err(DecodeError));
    }
}
