//! Just enough PNG to validate what a backend returned and to fabricate a
//! deterministic image for tests. No decoding beyond the header.

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Header facts of a PNG byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PngHeader {
    pub width: u32,
    pub height: u32,
}

/// Checks the signature, the IHDR chunk, and that an IEND chunk closes the
/// stream. A truncated or non-PNG body fails closed.
pub(crate) fn validate(bytes: &[u8]) -> Result<PngHeader, &'static str> {
    if bytes.len() < 8 + 25 + 12 {
        return Err("shorter than the smallest valid PNG");
    }
    if bytes[..8] != SIGNATURE {
        return Err("missing PNG signature");
    }
    let length = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if length != 13 || &bytes[12..16] != b"IHDR" {
        return Err("first chunk is not IHDR");
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width == 0 || height == 0 {
        return Err("IHDR declares an empty image");
    }
    let expected = crc(&bytes[12..29]);
    let stored = u32::from_be_bytes([bytes[29], bytes[30], bytes[31], bytes[32]]);
    if expected != stored {
        return Err("IHDR checksum mismatch");
    }
    if !bytes.ends_with(&iend()) {
        return Err("stream does not end with IEND");
    }
    Ok(PngHeader { width, height })
}

/// A valid, deterministic `width`×`height` opaque PNG whose single colour is
/// derived from `seed`. Stored (uncompressed) deflate blocks keep this free of
/// any compression dependency; every byte is a pure function of the inputs.
pub(crate) fn synthesize(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let rgb = [
        (seed & 0xff) as u8,
        ((seed >> 8) & 0xff) as u8,
        ((seed >> 16) & 0xff) as u8,
    ];
    let row_len = 1 + usize::try_from(width).expect("u32 fits usize") * 3;
    let mut raw = Vec::with_capacity(row_len * usize::try_from(height).expect("u32 fits usize"));
    for _ in 0..height {
        raw.push(0); // filter type: none
        raw.extend((0..width).flat_map(|_| rgb));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&SIGNATURE);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, no interlace
    push_chunk(&mut out, *b"IHDR", &ihdr);
    push_chunk(&mut out, *b"IDAT", &zlib_stored(&raw));
    out.extend_from_slice(&iend());
    out
}

fn iend() -> Vec<u8> {
    let mut chunk = Vec::with_capacity(12);
    push_chunk(&mut chunk, *b"IEND", &[]);
    chunk
}

fn push_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(
        &u32::try_from(data.len())
            .expect("chunk fits u32")
            .to_be_bytes(),
    );
    let start = out.len();
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let checksum = crc(&out[start..]);
    out.extend_from_slice(&checksum.to_be_bytes());
}

/// zlib stream of stored deflate blocks (no compression).
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(u8::from(last));
        let len = u16::try_from(chunk.len()).expect("chunk bounded to u16");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc(data: &[u8]) -> u32 {
    let mut value = 0xffff_ffffu32;
    for byte in data {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = if value & 1 == 1 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
        }
    }
    !value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_image_validates_and_is_deterministic() {
        let first = synthesize(4, 3, 0x00ab_cdef);
        let second = synthesize(4, 3, 0x00ab_cdef);
        assert_eq!(first, second);
        assert_eq!(
            validate(&first).unwrap(),
            PngHeader {
                width: 4,
                height: 3
            }
        );
        assert_ne!(first, synthesize(4, 3, 0x0012_3456));
    }

    #[test]
    fn corrupt_or_foreign_bytes_fail_closed() {
        assert!(validate(b"not a png").is_err());
        let mut broken = synthesize(2, 2, 1);
        broken[20] = 0xff; // height byte without fixing the CRC
        assert_eq!(validate(&broken).unwrap_err(), "IHDR checksum mismatch");
        let mut truncated = synthesize(2, 2, 1);
        truncated.truncate(truncated.len() - 1);
        assert_eq!(
            validate(&truncated).unwrap_err(),
            "stream does not end with IEND"
        );
        let mut jpeg_like = synthesize(2, 2, 1);
        jpeg_like[1] = b'J';
        assert_eq!(validate(&jpeg_like).unwrap_err(), "missing PNG signature");
    }
}
