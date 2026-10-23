pub fn parse_frame(data: &[u8]) -> Option<(&[u8], &[u8])> {
    // Placeholder: return header and payload slices
    if data.len() < 4 { return None; }
    let header = &data[0..4];
    let payload = &data[4..];
    Some((header, payload))
}
