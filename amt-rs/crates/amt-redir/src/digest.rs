use md5;

pub fn compute_digest(challenge: &[u8], password: &str) -> [u8; 16] {
    let mut data = Vec::with_capacity(challenge.len() + password.len());
    data.extend_from_slice(challenge);
    data.extend_from_slice(password.as_bytes());
    let digest = md5::compute(&data);
    *digest
}
