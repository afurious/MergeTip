//! Test and local-dev helpers (feature `testing`): mint real, signed regtest invoices whose
//! preimage can be re-derived from the invoice alone, so a mock wallet can "pay" them.
//!
//! The preimage is `HMAC-SHA256(seed, payment_secret)`. The payment secret travels inside the
//! invoice, so anyone holding `seed` (the mock LNURL server and the mock wallet share it) can
//! produce a preimage that hashes to the invoice's payment hash, without any shared state.

use std::time::Duration;

use bitcoin::{
    hashes::{Hash, sha256},
    secp256k1::{Secp256k1, SecretKey},
};
use lightning_invoice::{Bolt11Invoice, InvoiceBuilder, PaymentSecret};
use mergetip_core::Msats;
use time::OffsetDateTime;

use crate::Network;

/// The node key every minted invoice is signed with. Test-only, never a real node.
const NODE_KEY: [u8; 32] = [0x42; 32];

#[derive(Debug, Clone)]
pub struct MintParams {
    pub network: Network,
    pub amount: Msats,
    /// The LNURL metadata the invoice's description hash commits to.
    pub metadata: String,
    pub created_at: OffsetDateTime,
    pub expiry: Duration,
    pub seed: [u8; 32],
    /// Random when `None`.
    pub payment_secret: Option<[u8; 32]>,
}

impl Default for MintParams {
    fn default() -> Self {
        Self {
            network: Network::Regtest,
            amount: Msats::new(1_000),
            metadata: "[]".into(),
            created_at: OffsetDateTime::now_utc(),
            expiry: Duration::from_secs(600),
            seed: [7; 32],
            payment_secret: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedInvoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub preimage: String,
}

pub fn mint_invoice(params: &MintParams) -> MintedInvoice {
    let secret = params
        .payment_secret
        .unwrap_or_else(|| mergetip_crypto::random_bytes::<32>().expect("system randomness"));
    let preimage = preimage_for(&params.seed, &secret);
    let payment_hash = sha256::Hash::hash(&preimage);
    let since_epoch = Duration::from_secs(
        u64::try_from(params.created_at.unix_timestamp()).expect("timestamps after 1970"),
    );
    let key = SecretKey::from_slice(&NODE_KEY).expect("a valid test key");
    let secp = Secp256k1::new();
    let invoice = InvoiceBuilder::new(params.network.currency())
        .description_hash(sha256::Hash::hash(params.metadata.as_bytes()))
        .payment_hash(payment_hash)
        .payment_secret(PaymentSecret(secret))
        .duration_since_epoch(since_epoch)
        .min_final_cltv_expiry_delta(144)
        .amount_milli_satoshis(params.amount.get())
        .expiry_time(params.expiry)
        .build_signed(|message| secp.sign_ecdsa_recoverable(message, &key))
        .expect("a well-formed test invoice");
    MintedInvoice {
        bolt11: invoice.to_string(),
        payment_hash: hex::encode(payment_hash.to_byte_array()),
        preimage: hex::encode(preimage),
    }
}

pub fn preimage_for(seed: &[u8; 32], payment_secret: &[u8; 32]) -> [u8; 32] {
    mergetip_crypto::hmac_sha256(seed, payment_secret)
}

/// The preimage of an invoice minted with `seed`, derived from the invoice alone.
pub fn preimage_from_invoice(seed: &[u8; 32], bolt11: &str) -> Option<[u8; 32]> {
    let invoice: Bolt11Invoice = bolt11.parse().ok()?;
    Some(preimage_for(seed, &invoice.payment_secret().0))
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn minted_preimage_hashes_to_the_payment_hash() {
        let minted = mint_invoice(&MintParams::default());
        let preimage = hex::decode(&minted.preimage).unwrap();
        assert_eq!(hex::encode(Sha256::digest(&preimage)), minted.payment_hash);
        let derived = preimage_from_invoice(&[7; 32], &minted.bolt11).unwrap();
        assert_eq!(hex::encode(derived), minted.preimage);
        assert_ne!(
            preimage_from_invoice(&[8; 32], &minted.bolt11).map(hex::encode),
            Some(minted.preimage),
            "another seed derives another preimage"
        );
    }
}
