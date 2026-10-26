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

/// How far a Svix timestamp may be from now before the delivery counts as a replay.
pub const SVIX_TOLERANCE_SECS: i64 = 300;

/// Checks a Svix-signed webhook (Resend's delivery events): HMAC-SHA256 over
/// `"{svix-id}.{svix-timestamp}.{body}"` keyed by the base64 secret after `whsec_`, compared in
/// constant time against every `v1,<base64>` entry in `svix-signature` (several during secret
/// rotation). Timestamps more than [`SVIX_TOLERANCE_SECS`] from `now_unix` are refused, so a
/// captured delivery can't be replayed later.
pub fn verify_svix_signature(
    secret: &str,
    id: &str,
    timestamp: &str,
    body: &[u8],
    signatures: &str,
    now_unix: i64,
) -> Result<(), SignatureError> {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let key = STANDARD
        .decode(secret.strip_prefix("whsec_").unwrap_or(secret))
        .map_err(|_| SignatureError::Malformed)?;
    let sent_at: i64 = timestamp
        .trim()
        .parse()
        .map_err(|_| SignatureError::Malformed)?;
    if (now_unix - sent_at).abs() > SVIX_TOLERANCE_SECS {
        return Err(SignatureError::Mismatch);
    }
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts keys of any length");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.trim().as_bytes());
    mac.update(b".");
    mac.update(body);
    let candidates: Vec<Vec<u8>> = signatures
        .split_whitespace()
        .filter_map(|entry| entry.strip_prefix("v1,"))
        .filter_map(|sig| STANDARD.decode(sig).ok())
        .collect();
    if candidates.is_empty() {
        return Err(SignatureError::Malformed);
    }
    if candidates
        .iter()
        .any(|sig| mac.clone().verify_slice(sig).is_ok())
    {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Svix's documented example delivery.
    const SVIX_SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
    const SVIX_ID: &str = "msg_p5jXN8AQM9LWM0D4loKWxJek";
    const SVIX_TS: &str = "1614265330";
    const SVIX_BODY: &[u8] = br#"{"test": 2432232314}"#;
    const SVIX_SIG: &str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";

    #[test]
    fn accepts_svix_documented_vector_among_rotated_signatures() {
        let header = format!("v1,bm90LWl0 {SVIX_SIG}");
        assert_eq!(
            verify_svix_signature(
                SVIX_SECRET,
                SVIX_ID,
                SVIX_TS,
                SVIX_BODY,
                &header,
                1_614_265_330
            ),
            Ok(())
        );
    }

    #[test]
    fn svix_rejects_tampering_old_deliveries_and_junk() {
        let at = 1_614_265_330;
        let check = |id: &str, body: &[u8], sig: &str, now: i64| {
            verify_svix_signature(SVIX_SECRET, id, SVIX_TS, body, sig, now)
        };
        assert_eq!(
            check(SVIX_ID, b"{}", SVIX_SIG, at),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            check("msg_other", SVIX_BODY, SVIX_SIG, at),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            check(SVIX_ID, SVIX_BODY, SVIX_SIG, at + 301),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            check(SVIX_ID, SVIX_BODY, "", at),
            Err(SignatureError::Malformed)
        );
        assert_eq!(
            check(SVIX_ID, SVIX_BODY, "v2,abc", at),
            Err(SignatureError::Malformed)
        );
    }

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
