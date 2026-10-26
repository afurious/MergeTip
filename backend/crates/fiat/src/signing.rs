//! Bitnob request signing and webhook verification, as pure functions.
//!
//! Requests (Bitnob "Authentication"): `X-Auth-Signature` is the hex HMAC-SHA256, keyed by the
//! client secret, of `"{client_id}:{timestamp}:{nonce}:{body}"`, where `body` is the exact JSON
//! sent (empty without a body), `timestamp` Unix seconds and `nonce` 16 random bytes in hex.
//!
//! Webhooks (Bitnob "Verifying Events"): `x-bitnob-signature` is an HMAC-SHA512 of the payload
//! with the secret key.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Sha256, Sha512};

/// The four auth headers of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthHeaders {
    pub client: String,
    pub timestamp: String,
    pub nonce: String,
    pub signature: String,
}

/// Signs a request body. Pure: the caller supplies the time and nonce.
pub fn sign_request(
    client_id: &str,
    client_secret: &str,
    timestamp: i64,
    nonce: &str,
    body: &str,
) -> AuthHeaders {
    let mut mac = Hmac::<Sha256>::new_from_slice(client_secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(format!("{client_id}:{timestamp}:{nonce}:{body}").as_bytes());
    AuthHeaders {
        client: client_id.to_owned(),
        timestamp: timestamp.to_string(),
        nonce: nonce.to_owned(),
        signature: hex::encode(mac.finalize().into_bytes()),
    }
}

/// Checks a webhook's `x-bitnob-signature` over the raw body, in constant time.
// ASSUMPTION(bitnob): the docs say "HMAC SHA512 … of the event payload with your secret key"
// without naming the encoding or whether the raw body is signed; hex over the raw body is the
// convention of Bitnob's other signatures.
pub fn verify_webhook(secret: &str, body: &[u8], signature: &str) -> bool {
    let Ok(expected) = hex::decode(signature.trim()) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha512>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bitnob publishes the algorithm but no test vector; these were computed independently with
    // Python's `hmac` module from the documented string-to-sign.
    #[test]
    fn request_signatures_match_an_independent_implementation() {
        let signed = sign_request(
            "test_client",
            "test_secret",
            1_700_000_000,
            "00112233445566778899aabbccddeeff",
            r#"{"amount":"0.00021"}"#,
        );
        assert_eq!(
            signed.signature,
            "f7b65406250885d7e34ff702408db575f0cfffc49994b774852fedf46d71465e"
        );
        assert_eq!(signed.timestamp, "1700000000");
        let no_body = sign_request(
            "test_client",
            "test_secret",
            1_700_000_000,
            "00112233445566778899aabbccddeeff",
            "",
        );
        assert_eq!(
            no_body.signature,
            "837c4a097671fae22c0e81b4c8525160dc45997fe772ad935955882b2c9a1ac2"
        );
    }

    #[test]
    fn webhook_signatures_verify_and_reject_tampering() {
        let body = br#"{"event":"payouts.processing"}"#;
        let signature = "fcf5bd93afc28ef68b795cfcc1fe7257e0a49eccc530dc83681d641e429b45cc68432f7bf2cfa3c05bc1ae3108b1ef295427c38e4a4145aa186b9b53eb65b871";
        assert!(verify_webhook("whsec", body, signature));
        assert!(!verify_webhook(
            "whsec",
            br#"{"event":"payouts.withdrawal.success"}"#,
            signature
        ));
        assert!(!verify_webhook("other", body, signature));
        assert!(!verify_webhook("whsec", body, "not-hex"));
        assert!(!verify_webhook("whsec", body, ""));
    }
}
