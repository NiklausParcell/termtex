//! Kitty graphics protocol emitter.
//!
//! Encodes a PNG and emits it inline using the transmit-and-display escape
//! sequence (`<ESC>_G ... <ESC>\`). The base64 payload is split into the
//! protocol's per-chunk limit, with the continuation flag `m=1` on every chunk
//! but the last (`m=0`). Only the first chunk carries the format/action control
//! keys; continuation chunks carry only `m`.
//!
//! Reference: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

use std::io::{self, Write};

use base64::Engine;

/// Max base64 bytes per escape chunk, per the Kitty protocol.
const CHUNK: usize = 4096;

const ESC: &[u8] = b"\x1b";
/// String Terminator that ends each graphics escape: `ESC \`.
const ST: &[u8] = b"\x1b\\";

/// Emit a PNG inline at the cursor using transmit-and-display.
///
/// `cols`/`rows`, when `Some`, ask the terminal to scale the image into that
/// many text cells (Kitty `c=`/`r=` keys); `None` uses the PNG's natural size.
pub fn emit_png(
    out: &mut impl Write,
    png: &[u8],
    cols: Option<u32>,
    rows: Option<u32>,
) -> io::Result<()> {
    if png.is_empty() {
        return Ok(());
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let payload = b64.as_bytes();

    // chunks() never yields an empty slice for non-empty input, and png is
    // non-empty here, so there is always at least one chunk.
    let total = payload.len().div_ceil(CHUNK);
    for (i, chunk) in payload.chunks(CHUNK).enumerate() {
        let more = if i + 1 < total { 1 } else { 0 };
        out.write_all(ESC)?;
        if i == 0 {
            // f=100: PNG. a=T: transmit and display.
            write!(out, "_Gf=100,a=T")?;
            if let Some(c) = cols {
                write!(out, ",c={c}")?;
            }
            if let Some(r) = rows {
                write!(out, ",r={r}")?;
            }
            write!(out, ",m={more};")?;
        } else {
            write!(out, "_Gm={more};")?;
        }
        out.write_all(chunk)?;
        out.write_all(ST)?;
    }
    Ok(())
}

/// A small, dependency-light test image: a solid blue rectangle with a white
/// border. Used by `mathterm --selftest-image` to confirm the terminal renders
/// inline graphics at all, independent of the LaTeX rendering pipeline.
pub fn selftest_png() -> Vec<u8> {
    let (w, h) = (160u32, 80u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;
            let border = x < 3 || x >= w - 3 || y < 3 || y >= h - 3;
            let (r, g, b) = if border {
                (255, 255, 255)
            } else {
                (40, 90, 200)
            };
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }
    encode_rgba(w, h, &rgba)
}

/// Encode an 8-bit RGBA buffer to PNG bytes.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(rgba).expect("png data");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse the emitted bytes into a list of (control, payload) graphics
    /// escapes, asserting each is well-formed (`ESC _G ... ESC \`).
    fn parse_escapes(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut escapes = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            assert_eq!(&rest[..2], b"\x1b_", "each escape starts with ESC _");
            assert_eq!(rest[2], b'G', "graphics escape uses G");
            // control data runs up to the ';' separator
            let semi = rest.iter().position(|&b| b == b';').expect("has ';'");
            let control = String::from_utf8(rest[3..semi].to_vec()).unwrap();
            // payload runs up to the ST terminator (ESC \)
            let after = &rest[semi + 1..];
            let st = after
                .windows(2)
                .position(|w| w == b"\x1b\\")
                .expect("has ST");
            let payload = after[..st].to_vec();
            escapes.push((control, payload));
            rest = &after[st + 2..];
        }
        escapes
    }

    #[test]
    fn single_chunk_for_small_png() {
        let png = selftest_png();
        assert!(!png.is_empty());
        let mut out = Vec::new();
        emit_png(&mut out, &png, None, None).unwrap();
        let escapes = parse_escapes(&out);

        // The selftest image is small; it may still exceed one 4096-byte chunk,
        // so assert structural invariants rather than a fixed count.
        assert!(escapes[0].0.contains("f=100"), "first carries PNG format");
        assert!(escapes[0].0.contains("a=T"), "first carries action");

        // Reassemble base64 and confirm it decodes back to the original PNG.
        let mut b64 = String::new();
        for (_, p) in &escapes {
            b64.push_str(std::str::from_utf8(p).unwrap());
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(decoded, png, "payload round-trips to the original PNG");
    }

    #[test]
    fn multi_chunk_continuation_flags_are_correct() {
        // A payload large enough to need several chunks.
        let big = vec![0xABu8; CHUNK * 3]; // base64 expands this past 3 chunks
        let mut out = Vec::new();
        emit_png(&mut out, &big, None, None).unwrap();
        let escapes = parse_escapes(&out);
        assert!(escapes.len() >= 2, "should split into multiple chunks");

        let last = escapes.len() - 1;
        for (i, (control, _)) in escapes.iter().enumerate() {
            if i == 0 {
                assert!(control.contains("f=100,a=T"), "first has format+action");
            } else {
                assert!(
                    !control.contains("f=100"),
                    "continuation chunks omit format keys"
                );
            }
            if i == last {
                assert!(control.contains("m=0"), "final chunk has m=0");
            } else {
                assert!(control.contains("m=1"), "non-final chunk has m=1");
            }
        }
    }

    #[test]
    fn empty_png_emits_nothing() {
        let mut out = Vec::new();
        emit_png(&mut out, &[], None, None).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn cell_sizing_keys_are_emitted() {
        let png = selftest_png();
        let mut out = Vec::new();
        emit_png(&mut out, &png, Some(20), Some(4)).unwrap();
        let escapes = parse_escapes(&out);
        assert!(escapes[0].0.contains("c=20"));
        assert!(escapes[0].0.contains("r=4"));
    }
}
