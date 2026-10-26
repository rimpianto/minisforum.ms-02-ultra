//! Decoders for the Intel AMT KVM encodings.
//!
//! The reference (MeshCommander, Apache-2.0) works in 64x64 tiles. A rect with
//! encoding 16 (RLE) carries a zlib stream, or an "uncompressed zlib block"
//! (a 5-byte header then raw tile data). The decompressed stream is a sequence
//! of tiles, each starting with a sub-encoding byte:
//!
//!   0        raw pixels (bpp bytes each)
//!   1        solid colour (bpp bytes)
//!   2..=16   packed palette with N entries, indices bit-packed
//!   128      run-length: colour, then length = 1 + sum of bytes, 255 continues

use flate2::read::ZlibDecoder;
use flate2::{Decompress, FlushDecompress, Status};
use std::io::Read;

use crate::framebuffer::Framebuffer;

pub const TILE: usize = 64;

pub fn decode_zlib(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| format!("zlib: {e}"))?;
    Ok(out)
}

/// Intel AMT flushes the deflate stream at the end of every tile block instead
/// of closing it, so `read_to_end` fails with "incomplete deflate stream" even
/// when all the data is there. Decompress what arrives and accept a stream
/// without a final block. Tries zlib framing first, then raw deflate.
pub fn inflate_partial(data: &[u8]) -> Result<Vec<u8>, String> {
    fn attempt(data: &[u8], zlib_header: bool) -> Result<Vec<u8>, flate2::DecompressError> {
        let mut d = Decompress::new(zlib_header);
        let mut out = Vec::new();
        let mut buf = [0u8; 65536];
        let mut input = data;
        loop {
            let before_in = d.total_in();
            let before_out = d.total_out();
            let status = d.decompress(input, &mut buf, FlushDecompress::Sync)?;
            let produced = (d.total_out() - before_out) as usize;
            let consumed = (d.total_in() - before_in) as usize;
            out.extend_from_slice(&buf[..produced]);
            input = &input[consumed..];
            if input.is_empty() || status == Status::StreamEnd || (produced == 0 && consumed == 0) {
                break;
            }
        }
        Ok(out)
    }

    match attempt(data, true) {
        Ok(out) if !out.is_empty() => Ok(out),
        _ => attempt(data, false).map_err(|e| format!("inflate: {e}")),
    }
}

/// Rect payload of encoding 16: u32 length followed by either an uncompressed
/// zlib block or a real zlib stream. Returns the tile stream.
pub fn rle_stream(block: &[u8]) -> Result<Vec<u8>, String> {
    if block.len() < 4 {
        return Err("rle block shorter than its length field".into());
    }
    let datalen = u32::from_le_bytes([block[0], block[1], block[2], block[3]]) as usize;
    if block.len() < 4 + datalen {
        return Err(format!(
            "rle block truncated: need {} have {}",
            4 + datalen,
            block.len()
        ));
    }
    let data = &block[4..4 + datalen];
    // Uncompressed zlib block, as produced by some firmware: 0x00, len-5 (u16)
    if datalen > 5 && data[0] == 0 && u16::from_le_bytes([data[1], data[2]]) as usize == datalen - 5
    {
        return Ok(data[5..].to_vec());
    }
    if std::env::var("AMT_TRACE").is_ok() {
        let head: Vec<String> = data.iter().take(8).map(|b| format!("{b:02x}")).collect();
        eprintln!("[amt] rle block {} bytes, head {}", datalen, head.join(" "));
    }
    inflate_partial(data)
}

fn put(fb: &mut Framebuffer, bpp: usize, x: usize, y: usize, v: u16) {
    if bpp == 2 {
        fb.put_rgb565(x, y, v);
    } else {
        fb.put_rgb332(x, y, v as u8);
    }
}

/// Decode the tile stream of one rect and paint it at (rx, ry).
pub fn draw_rle_tiles(
    stream: &[u8],
    bpp: usize,
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    fb: &mut Framebuffer,
) -> Result<(), String> {
    let mut p = 0usize;
    let mut ty = 0usize;
    while ty < rh {
        let th = (rh - ty).min(TILE);
        let mut tx = 0usize;
        while tx < rw {
            let tw = (rw - tx).min(TILE);
            // The encoder always packs a full 64x64 tile, even at the right
            // and bottom edges; consuming only tw*th leaves bytes behind and
            // desynchronises the next tile. Paint outside the rect is clipped
            // by the framebuffer.
            let area = TILE * TILE;
            if p >= stream.len() {
                return Ok(());
            }
            let sub = stream[p];
            p += 1;
            match sub {
                0 => {
                    let need = area * bpp;
                    if p + need > stream.len() {
                        return Err("raw tile truncated".into());
                    }
                    for i in 0..area {
                        let v = if bpp == 2 {
                            let v = u16::from_le_bytes([stream[p], stream[p + 1]]);
                            p += 2;
                            v
                        } else {
                            let v = stream[p] as u16;
                            p += 1;
                            v
                        };
                        put(fb, bpp, rx + tx + i % tw, ry + ty + i / tw, v);
                    }
                }
                1 => {
                    let v = if bpp == 2 {
                        if p + 2 > stream.len() {
                            return Err("solid tile truncated".into());
                        }
                        let v = u16::from_le_bytes([stream[p], stream[p + 1]]);
                        p += 2;
                        v
                    } else {
                        if p >= stream.len() {
                            return Err("solid tile truncated".into());
                        }
                        let v = stream[p] as u16;
                        p += 1;
                        v
                    };
                    for i in 0..area {
                        put(fb, bpp, rx + tx + i % tw, ry + ty + i / tw, v);
                    }
                }
                2..=16 => {
                    let n = sub as usize;
                    let mut palette: Vec<u16> = Vec::with_capacity(n);
                    for _ in 0..n {
                        let v = if bpp == 2 {
                            if p + 2 > stream.len() {
                                return Err("palette truncated".into());
                            }
                            let v = u16::from_le_bytes([stream[p], stream[p + 1]]);
                            p += 2;
                            v
                        } else {
                            if p >= stream.len() {
                                return Err("palette truncated".into());
                            }
                            let v = stream[p] as u16;
                            p += 1;
                            v
                        };
                        palette.push(v);
                    }
                    let (bits, mask): (u8, u8) = if n == 2 {
                        (1, 1)
                    } else if n <= 4 {
                        (2, 3)
                    } else {
                        (4, 15)
                    };
                    let mut count = 0usize;
                    while count < area && p < stream.len() {
                        let byte = stream[p];
                        p += 1;
                        let mut shift = 8 - bits;
                        loop {
                            let idx = ((byte >> shift) & mask) as usize;
                            let v = *palette.get(idx).unwrap_or(&0);
                            put(fb, bpp, rx + tx + count % tw, ry + ty + count / tw, v);
                            count += 1;
                            if count >= area || shift < bits {
                                break;
                            }
                            shift -= bits;
                        }
                    }
                }
                128 => {
                    let mut count = 0usize;
                    while count < area && p < stream.len() {
                        let v = if bpp == 2 {
                            if p + 2 > stream.len() {
                                return Err("run colour truncated".into());
                            }
                            let v = u16::from_le_bytes([stream[p], stream[p + 1]]);
                            p += 2;
                            v
                        } else {
                            let v = stream[p] as u16;
                            p += 1;
                            v
                        };
                        let mut run = 1usize;
                        loop {
                            if p >= stream.len() {
                                break;
                            }
                            let b = stream[p];
                            p += 1;
                            run += b as usize;
                            if b != 255 {
                                break;
                            }
                        }
                        for _ in 0..run {
                            if count >= area {
                                break;
                            }
                            put(fb, bpp, rx + tx + count % tw, ry + ty + count / tw, v);
                            count += 1;
                        }
                    }
                }
                // Palette RLE: the firmware uses this for every tile that
                // contains text. subencoding-128 colours live in the palette,
                // then each entry is an index byte; an index above 127 starts a
                // run whose length follows (255 continues).
                sub if sub > 129 => {
                    let n = (sub - 128) as usize;
                    let mut palette: Vec<u16> = Vec::with_capacity(n);
                    for _ in 0..n {
                        let v = if bpp == 2 {
                            if p + 2 > stream.len() {
                                return Err("palette-rle palette truncated".into());
                            }
                            let v = u16::from_le_bytes([stream[p], stream[p + 1]]);
                            p += 2;
                            v
                        } else {
                            if p >= stream.len() {
                                return Err("palette-rle palette truncated".into());
                            }
                            let v = stream[p] as u16;
                            p += 1;
                            v
                        };
                        palette.push(v);
                    }
                    let mut count = 0usize;
                    while count < area && p < stream.len() {
                        let index = stream[p];
                        p += 1;
                        let v = *palette.get((index % 128) as usize).unwrap_or(&0);
                        let mut run = 1usize;
                        if index > 127 {
                            loop {
                                if p >= stream.len() {
                                    break;
                                }
                                let b = stream[p];
                                p += 1;
                                run += b as usize;
                                if b != 255 {
                                    break;
                                }
                            }
                        }
                        for _ in 0..run {
                            if count >= area {
                                break;
                            }
                            put(fb, bpp, rx + tx + count % tw, ry + ty + count / tw, v);
                            count += 1;
                        }
                    }
                }
                other => return Err(format!("unsupported tile sub-encoding {other}")),
            }
            tx += TILE;
        }
        ty += TILE;
    }
    Ok(())
}

/// Raw rect payload (encoding 0): rw*rh pixels, bpp bytes each.
pub fn draw_raw(
    payload: &[u8],
    bpp: usize,
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    fb: &mut Framebuffer,
) -> Result<(), String> {
    let need = rw * rh * bpp;
    if payload.len() < need {
        return Err(format!(
            "raw rect truncated: need {need} have {}",
            payload.len()
        ));
    }
    let mut p = 0usize;
    for y in 0..rh {
        for x in 0..rw {
            let v = if bpp == 2 {
                let v = u16::from_le_bytes([payload[p], payload[p + 1]]);
                p += 2;
                v
            } else {
                let v = payload[p] as u16;
                p += 1;
                v
            };
            put(fb, bpp, rx + x, ry + y, v);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb() -> Framebuffer {
        Framebuffer::new(8, 4)
    }

    #[test]
    fn solid_tile_rgb332() {
        let mut f = fb();
        // sub-encoding 1 (solid), colour 0xE0 = pure red in RGB332
        draw_rle_tiles(&[1, 0xE0], 1, 0, 0, 8, 4, &mut f).unwrap();
        assert_eq!(&f.pixels[0..4], &[0xE0, 0, 0, 255]);
        assert_eq!(&f.pixels[(7 * 4)..(7 * 4 + 4)], &[0xE0, 0, 0, 255]);
    }

    #[test]
    fn run_tile_rgb565() {
        let mut f = fb();
        // sub-encoding 128 (runs), colour 0x0007 (blue-ish), run length 8+32 = 40
        let mut stream = vec![128u8];
        stream.extend_from_slice(&0x0007u16.to_le_bytes());
        stream.push(7); // 1 + 7 = 8
        stream.extend_from_slice(&0x0007u16.to_le_bytes());
        stream.push(31); // 1 + 31 = 32
        draw_rle_tiles(&stream, 2, 0, 0, 8, 4, &mut f).unwrap();
        // 8+32 = 40 pixels > 32 available: everything must be painted
        for px in f.pixels.chunks_exact(4) {
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn palette_tile_4_colours() {
        let mut f = fb();
        // sub-encoding 4 (4 colours, 2 bits each), palette 0,1,2,3
        let mut stream = vec![4u8, 0x00, 0x40, 0x80, 0xC0];
        // indices 0,1,2,3 repeated: 0b00011011 = 0x1B
        stream.push(0x1B);
        while stream.len() < 17 {
            stream.push(0x1B); // keep feeding indices for the remaining pixels
        }
        draw_rle_tiles(&stream, 1, 0, 0, 8, 4, &mut f).unwrap();
        // first four pixels: colours 0,1,2,3 -> r = 0x00,0x40,0x80,0xC0
        assert_eq!(f.pixels[0], 0x00);
        assert_eq!(f.pixels[4], 0x40);
        assert_eq!(f.pixels[8], 0x80);
        assert_eq!(f.pixels[12], 0xC0);
    }

    #[test]
    fn uncompressed_block_is_detected() {
        // u32 datalen, then: 0x00, payload length (u16), two filler bytes,
        // then the tile stream (exactly what the firmware emits for a stored
        // block).
        let payload = [1u8, 0xFF]; // solid white tile, RGB332
        let datalen: u32 = 5 + payload.len() as u32;
        let mut block = datalen.to_le_bytes().to_vec();
        block.push(0x00);
        block.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        block.extend_from_slice(&[0x00, 0x00]);
        block.extend_from_slice(&payload);
        let stream = rle_stream(&block).unwrap();
        assert_eq!(stream, payload.to_vec());
    }
}
