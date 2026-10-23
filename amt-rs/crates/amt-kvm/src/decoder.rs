use flate2::read::ZlibDecoder;
use std::io::Read;

pub fn decode_rle8(data: &[u8]) -> Vec<u8> {
    // Very naive placeholder: return raw data
    data.to_vec()
}

pub fn decode_rle16(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}

pub fn decode_raw(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}

pub fn decode_zlib(data: &[u8]) -> Vec<u8> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap_or(0);
    out
}
