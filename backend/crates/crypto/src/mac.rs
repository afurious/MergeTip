use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("signature header is missing or malformed")]
    Malformed,
    #[error("signature does not match")]
    Mismatch,
}

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// Checks GitHub's `X-Hub-Signature-256: sha256=<hex>` header over the raw request body.
///
/// The comparison is constant-time (`Mac::verify_slice`) so the expected signature cannot be
/// recovered byte by byte from response timings.
pub fn verify_github_signature(
    secret: &[u8],
    body: &[u8],
    header: &str,
) -> Result<(), SignatureError> {
    let hex_sig = header
        .strip_prefix("sha256=")
        .ok_or(SignatureError::Malformed)?;
    let expected = hex::decode(hex_sig).map_err(|_| SignatureError::Malformed)?;
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(body);
    mac.verify_slice(&expected)
        .map_err(|_| SignatureError::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test vector from GitHub's "Validating webhook deliveries" documentation.
    const SECRET: &[u8] = b"It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    const HEADER: &str = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn accepts_githubs_documented_vector() {
        assert_eq!(verify_github_signature(SECRET, BODY, HEADER), Ok(()));
    }

    #[test]
    fn rejects_a_tampered_body() {
        assert_eq!(
            verify_github_signature(SECRET, b"Hello, World?", HEADER),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn rejects_the_wrong_secret() {
        assert_eq!(
            verify_github_signature(b"guess", BODY, HEADER),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn rejects_malformed_headers() {
        for header in [
            "",
            "sha1=abc",
            "sha256=zz",
            "757107ea0eb2509fc211221cce984b8a",
        ] {
            assert_eq!(
                verify_github_signature(SECRET, BODY, header),
                Err(SignatureError::Malformed),
                "{header:?}"
            );
        }
    }
}
