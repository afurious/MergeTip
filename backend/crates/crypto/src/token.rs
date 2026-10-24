use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use crate::Redacted;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("system random number generator failed")]
pub struct RandomError;

pub fn random_bytes<const N: usize>() -> Result<[u8; N], RandomError> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|_| RandomError)?;
    Ok(bytes)
}

/// An opaque bearer value (session, claim link, extension token).
///
/// Only [`Token::hash`] is stored; the raw value goes to the user once and is never persisted,
/// so a database leak does not reveal usable tokens.
#[derive(Debug)]
pub struct Token {
    pub raw: Redacted<String>,
    pub hash: String,
}

/// 32 random bytes, base64url-encoded for URLs and cookies.
pub fn random_token() -> Result<Token, RandomError> {
    let raw = URL_SAFE_NO_PAD.encode(random_bytes::<32>()?);
    let hash = hash_token(&raw);
    Ok(Token {
        raw: Redacted::new(raw),
        hash,
    })
}

/// Hex SHA-256 of a raw token, as stored in `*_token_hash` columns. A plain hash is enough here
/// because tokens carry 256 bits of entropy; short codes (OTPs) must use an HMAC instead.
pub fn hash_token(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_url_safe_and_hashed() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_ne!(a.raw.expose(), b.raw.expose());
        assert_eq!(a.raw.expose().len(), 43);
        assert!(
            a.raw
                .expose()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(a.hash, hash_token(a.raw.expose()));
        assert_eq!(a.hash.len(), 64);
        assert_eq!(format!("{a:?}").matches(a.raw.expose().as_str()).count(), 0);
    }
}
