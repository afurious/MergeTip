//! Envelope encryption for secrets stored in Postgres.
//!
//! Every value gets its own AES-256-GCM data key (DEK). The DEK is wrapped by the master key and
//! stored next to the ciphertext in a single blob:
//!
//! ```text
//! version(1) ‖ key_id(1) ‖ dek_nonce(12) ‖ wrapped_dek(48) ‖ data_nonce(12) ‖ ciphertext+tag
//! ```
//!
//! Callers pass associated data (AAD) such as `"wallet_connections.nwc_uri:{row_id}"`, so a
//! ciphertext copied into another row or column fails to decrypt. Rotating the master key only
//! re-wraps the 48-byte DEK ([`Keyring::rewrap`]); the data ciphertext is left untouched.

use aes_gcm::{
    Aes256Gcm,
    aead::{self, Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{Redacted, random_bytes};

const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const WRAPPED_DEK_LEN: usize = KEY_LEN + TAG_LEN;
const HEADER_LEN: usize = 2;
const DATA_OFFSET: usize = HEADER_LEN + NONCE_LEN + WRAPPED_DEK_LEN;
const MIN_BLOB_LEN: usize = DATA_OFFSET + NONCE_LEN + TAG_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("master key must be 32 bytes of hex (64 characters)")]
    InvalidMasterKey,
    #[error("current and previous master keys share a key id; generate a new key")]
    KeyIdCollision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("ciphertext is malformed or has an unsupported version")]
    Malformed,
    #[error("ciphertext was sealed with a master key that is not configured")]
    UnknownKey,
    /// Wrong key, wrong AAD or tampered bytes; GCM cannot tell these apart, by design.
    #[error("ciphertext failed authentication")]
    Decrypt,
    #[error("system random number generator failed")]
    Random,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct MasterKey {
    #[zeroize(skip)]
    id: u8,
    bytes: [u8; KEY_LEN],
}

impl MasterKey {
    fn from_hex(hex_key: &str) -> Result<Self, KeyError> {
        let mut decoded = hex::decode(hex_key.trim()).map_err(|_| KeyError::InvalidMasterKey)?;
        let result = <[u8; KEY_LEN]>::try_from(decoded.as_slice())
            .map(|bytes| Self {
                id: key_id(&bytes),
                bytes,
            })
            .map_err(|_| KeyError::InvalidMasterKey);
        decoded.zeroize();
        result
    }

    fn cipher(&self) -> Aes256Gcm {
        cipher(&self.bytes)
    }
}

fn cipher(key: &[u8; KEY_LEN]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).expect("key is exactly 32 bytes")
}

fn nonce(bytes: &[u8]) -> aead::Nonce<Aes256Gcm> {
    aead::Nonce::<Aes256Gcm>::try_from(bytes).expect("nonce slices are exactly 12 bytes")
}

/// A short, non-secret fingerprint so a blob records which master key wrapped its DEK.
fn key_id(bytes: &[u8; KEY_LEN]) -> u8 {
    let mut hasher = Sha256::new();
    hasher.update(b"mergetip-master-key-id");
    hasher.update(bytes);
    hasher.finalize()[0]
}

/// The master key, plus the previous one while a rotation is in progress.
pub struct Keyring {
    current: MasterKey,
    previous: Option<MasterKey>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("current_id", &self.current.id)
            .field("previous_id", &self.previous.as_ref().map(|k| k.id))
            .finish()
    }
}

impl Keyring {
    pub fn from_hex(current: &str, previous: Option<&str>) -> Result<Self, KeyError> {
        let current = MasterKey::from_hex(current)?;
        let previous = previous.map(MasterKey::from_hex).transpose()?;
        if previous.as_ref().is_some_and(|p| p.id == current.id) {
            return Err(KeyError::KeyIdCollision);
        }
        Ok(Self { current, previous })
    }

    /// Encrypts `plaintext` under a fresh data key bound to `aad`.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        let dek = Redacted::new(random_bytes::<KEY_LEN>().map_err(|_| EnvelopeError::Random)?);
        let data_nonce = random_bytes::<NONCE_LEN>().map_err(|_| EnvelopeError::Random)?;
        let ciphertext = cipher(dek.expose())
            .encrypt(
                &nonce(&data_nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| EnvelopeError::Decrypt)?;

        let mut blob = Vec::with_capacity(MIN_BLOB_LEN + plaintext.len());
        self.wrap_dek_into(&mut blob, dek.expose(), aad)?;
        blob.extend_from_slice(&data_nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypts a blob produced by [`Keyring::seal`] with the same `aad`.
    pub fn open(&self, blob: &[u8], aad: &[u8]) -> Result<Redacted<Vec<u8>>, EnvelopeError> {
        let dek = self.unwrap_dek(blob, aad)?;
        let data_nonce = &blob[DATA_OFFSET..DATA_OFFSET + NONCE_LEN];
        let ciphertext = &blob[DATA_OFFSET + NONCE_LEN..];
        cipher(dek.expose())
            .decrypt(
                &nonce(data_nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map(Redacted::new)
            .map_err(|_| EnvelopeError::Decrypt)
    }

    /// Re-wraps the blob's data key under the current master key. Run over every encrypted
    /// column after rotating `MASTER_KEY`, before the previous key is removed from config.
    pub fn rewrap(&self, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        let dek = self.unwrap_dek(blob, aad)?;
        let mut rewrapped = Vec::with_capacity(blob.len());
        self.wrap_dek_into(&mut rewrapped, dek.expose(), aad)?;
        rewrapped.extend_from_slice(&blob[DATA_OFFSET..]);
        Ok(rewrapped)
    }

    /// Whether the blob's data key is already wrapped by the current master key.
    pub fn is_current(&self, blob: &[u8]) -> bool {
        blob.len() >= HEADER_LEN && blob[0] == VERSION && blob[1] == self.current.id
    }

    /// A 32-byte key derived from the current master key for one purpose (e.g. the OTP pepper).
    /// Distinct purposes give independent keys, so one leaked subkey exposes nothing else.
    pub fn subkey(&self, purpose: &str) -> Redacted<[u8; KEY_LEN]> {
        let hkdf = Hkdf::<Sha256>::new(Some(b"mergetip-subkey-v1"), &self.current.bytes);
        let mut okm = [0_u8; KEY_LEN];
        hkdf.expand(purpose.as_bytes(), &mut okm)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Redacted::new(okm)
    }

    fn wrap_dek_into(
        &self,
        out: &mut Vec<u8>,
        dek: &[u8; KEY_LEN],
        aad: &[u8],
    ) -> Result<(), EnvelopeError> {
        let header = [VERSION, self.current.id];
        let dek_nonce = random_bytes::<NONCE_LEN>().map_err(|_| EnvelopeError::Random)?;
        let wrapped = self
            .current
            .cipher()
            .encrypt(
                &nonce(&dek_nonce),
                Payload {
                    msg: dek,
                    aad: &wrap_aad(header, aad),
                },
            )
            .map_err(|_| EnvelopeError::Decrypt)?;
        out.extend_from_slice(&header);
        out.extend_from_slice(&dek_nonce);
        out.extend_from_slice(&wrapped);
        Ok(())
    }

    fn unwrap_dek(
        &self,
        blob: &[u8],
        aad: &[u8],
    ) -> Result<Redacted<[u8; KEY_LEN]>, EnvelopeError> {
        if blob.len() < MIN_BLOB_LEN || blob[0] != VERSION {
            return Err(EnvelopeError::Malformed);
        }
        let header = [blob[0], blob[1]];
        let key = [Some(&self.current), self.previous.as_ref()]
            .into_iter()
            .flatten()
            .find(|key| key.id == header[1])
            .ok_or(EnvelopeError::UnknownKey)?;
        let dek_nonce = &blob[HEADER_LEN..HEADER_LEN + NONCE_LEN];
        let wrapped = &blob[HEADER_LEN + NONCE_LEN..DATA_OFFSET];
        let mut dek = key
            .cipher()
            .decrypt(
                &nonce(dek_nonce),
                Payload {
                    msg: wrapped,
                    aad: &wrap_aad(header, aad),
                },
            )
            .map_err(|_| EnvelopeError::Decrypt)?;
        let result = <[u8; KEY_LEN]>::try_from(dek.as_slice())
            .map(Redacted::new)
            .map_err(|_| EnvelopeError::Malformed);
        dek.zeroize();
        result
    }
}

/// The wrapped DEK authenticates the blob header too, so the version and key id cannot be
/// altered without detection.
fn wrap_aad(header: [u8; HEADER_LEN], aad: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + aad.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(aad);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const KEY_B: &str = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";
    const AAD: &[u8] = b"wallet_connections.nwc_uri:7f9c";

    fn keyring(current: &str, previous: Option<&str>) -> Keyring {
        Keyring::from_hex(current, previous).unwrap()
    }

    #[test]
    fn round_trips() {
        let ring = keyring(KEY_A, None);
        let blob = ring.seal(b"nostr+walletconnect://abc", AAD).unwrap();
        assert_eq!(blob.len(), MIN_BLOB_LEN + 25);
        assert_eq!(
            ring.open(&blob, AAD).unwrap().expose().as_slice(),
            b"nostr+walletconnect://abc"
        );
    }

    #[test]
    fn same_plaintext_seals_differently() {
        let ring = keyring(KEY_A, None);
        assert_ne!(ring.seal(b"x", AAD).unwrap(), ring.seal(b"x", AAD).unwrap());
    }

    #[test]
    fn wrong_aad_fails() {
        let ring = keyring(KEY_A, None);
        let blob = ring.seal(b"secret", AAD).unwrap();
        assert_eq!(
            ring.open(&blob, b"users.email:7f9c").map(|_| ()),
            Err(EnvelopeError::Decrypt)
        );
    }

    #[test]
    fn any_tampered_byte_fails() {
        let ring = keyring(KEY_A, None);
        let blob = ring.seal(b"secret", AAD).unwrap();
        for i in 0..blob.len() {
            let mut tampered = blob.clone();
            tampered[i] ^= 0x01;
            assert!(
                ring.open(&tampered, AAD).is_err(),
                "byte {i} not authenticated"
            );
        }
    }

    #[test]
    fn unknown_key_is_reported() {
        let blob = keyring(KEY_A, None).seal(b"secret", AAD).unwrap();
        assert_eq!(
            keyring(KEY_B, None).open(&blob, AAD).map(|_| ()),
            Err(EnvelopeError::UnknownKey)
        );
    }

    #[test]
    fn truncated_blob_is_malformed() {
        let ring = keyring(KEY_A, None);
        let blob = ring.seal(b"secret", AAD).unwrap();
        assert_eq!(
            ring.open(&blob[..MIN_BLOB_LEN - 1], AAD).map(|_| ()),
            Err(EnvelopeError::Malformed)
        );
    }

    #[test]
    fn rotation_reads_old_blobs_and_rewraps_them() {
        let old = keyring(KEY_A, None);
        let blob = old.seal(b"secret", AAD).unwrap();

        let rotating = keyring(KEY_B, Some(KEY_A));
        assert!(!rotating.is_current(&blob));
        assert_eq!(
            rotating.open(&blob, AAD).unwrap().expose().as_slice(),
            b"secret"
        );

        let rewrapped = rotating.rewrap(&blob, AAD).unwrap();
        assert!(rotating.is_current(&rewrapped));
        assert_eq!(rewrapped[DATA_OFFSET..], blob[DATA_OFFSET..]);

        let rotated = keyring(KEY_B, None);
        assert_eq!(
            rotated.open(&rewrapped, AAD).unwrap().expose().as_slice(),
            b"secret"
        );
    }

    #[test]
    fn invalid_master_keys_are_rejected() {
        for key in ["", "zz", &KEY_A[..62], &format!("{KEY_A}00")] {
            assert_eq!(
                Keyring::from_hex(key, None).map(|_| ()),
                Err(KeyError::InvalidMasterKey),
                "{key:?}"
            );
        }
        assert_eq!(
            Keyring::from_hex(KEY_A, Some(KEY_A)).map(|_| ()),
            Err(KeyError::KeyIdCollision)
        );
    }

    #[test]
    fn subkeys_are_stable_and_purpose_separated() {
        let ring = keyring(KEY_A, None);
        assert_eq!(ring.subkey("otp").expose(), ring.subkey("otp").expose());
        assert_ne!(
            ring.subkey("otp").expose(),
            ring.subkey("unsubscribe").expose()
        );
        assert_ne!(
            ring.subkey("otp").expose(),
            keyring(KEY_B, None).subkey("otp").expose()
        );
    }

    #[test]
    fn debug_does_not_print_key_material() {
        let debug = format!("{:?}", keyring(KEY_A, None));
        assert!(!debug.contains("0001020304"));
        assert!(debug.starts_with("Keyring"));
    }
}
