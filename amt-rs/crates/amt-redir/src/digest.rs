//! Intel AMT redirection digest: MD5, same formula as MeshCommander's amt-redir.

use md5::compute;

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn md5_hex(input: &str) -> String {
    hex(&compute(input.as_bytes()).0)
}

pub fn compute_digest(challenge: &[u8], password: &str) -> [u8; 16] {
    let mut data = Vec::with_capacity(challenge.len() + password.len());
    data.extend_from_slice(challenge);
    data.extend_from_slice(password.as_bytes());
    compute(&data).0
}

/// Intel AMT redirection digest.
///
/// HA1 = md5(user:realm:pass), HA2 = md5("POST:" + authuri),
/// response = md5(HA1:nonce:[snc:cnonce:qop:]HA2)
#[allow(clippy::too_many_arguments)]
pub fn intel_digest(
    user: &str,
    realm: &str,
    pass: &str,
    nonce: &str,
    authuri: &str,
    cnonce: &str,
    snc: &str,
    qop: Option<&str>,
) -> String {
    let ha1 = md5_hex(&format!("{}:{}:{}", user, realm, pass));
    let ha2 = md5_hex(&format!("POST:{}", authuri));
    let extra = match qop {
        Some(q) => format!("{}:{}:{}:", snc, cnonce, q),
        None => String::new(),
    };
    md5_hex(&format!("{}:{}:{}{}", ha1, nonce, extra, ha2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_of_known_string() {
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn digest_matches_manual_computation() {
        let ha1 = md5_hex("admin:Digest:secret");
        let ha2 = md5_hex("POST:/RedirectionService");
        let expected = md5_hex(&format!("{}:nonce123:00000002:cnonce42:auth:{}", ha1, ha2));
        assert_eq!(
            intel_digest(
                "admin",
                "Digest",
                "secret",
                "nonce123",
                "/RedirectionService",
                "cnonce42",
                "00000002",
                Some("auth")
            ),
            expected
        );
    }
}
