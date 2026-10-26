//! Envelope encryption (AES-256-GCM data keys wrapped by a master key), HMAC helpers, tokens and
//! the [`Redacted`] wrapper that keeps secrets out of logs.

mod envelope;
mod mac;
mod redacted;
mod token;

pub use envelope::{EnvelopeError, KeyError, Keyring};
pub use mac::{SignatureError, hmac_sha256, verify_github_signature};
pub use redacted::Redacted;
pub use token::{RandomError, Token, constant_time_eq, hash_token, random_bytes, random_token};
