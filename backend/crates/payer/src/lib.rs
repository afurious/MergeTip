//! Paying tips: LNURL-pay to get an invoice from the receiver's Lightning address, validation of
//! that invoice, and NWC (NIP-47) to have the sender's own wallet pay it (FR-5).

mod address;
pub mod invoice;
pub mod lnurl;
pub mod nwc;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::{fmt, str::FromStr, time::Duration};

use async_trait::async_trait;
use mergetip_core::{Msats, Sats};
use time::OffsetDateTime;

pub use address::LightningAddress;
pub use invoice::InvoiceError;
pub use lnurl::{LnurlClient, ResolveError};

/// The Lightning network invoices must be for. Production is `bitcoin`; the local mocks use
/// `regtest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Bitcoin,
    Testnet,
    Signet,
    Regtest,
}

impl Network {
    pub(crate) fn currency(self) -> lightning_invoice::Currency {
        match self {
            Self::Bitcoin => lightning_invoice::Currency::Bitcoin,
            Self::Testnet => lightning_invoice::Currency::BitcoinTestnet,
            Self::Signet => lightning_invoice::Currency::Signet,
            Self::Regtest => lightning_invoice::Currency::Regtest,
        }
    }
}

impl FromStr for Network {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bitcoin" | "mainnet" => Ok(Self::Bitcoin),
            "testnet" => Ok(Self::Testnet),
            "signet" => Ok(Self::Signet),
            "regtest" => Ok(Self::Regtest),
            other => Err(format!(
                "expected bitcoin, testnet, signet or regtest, got {other:?}"
            )),
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bitcoin => "bitcoin",
            Self::Testnet => "testnet",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        })
    }
}

/// An invoice that passed every check in [`invoice::validate`]: safe to hand to the wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedInvoice {
    pub bolt11: String,
    /// Hex.
    pub payment_hash: String,
    pub amount: Msats,
    pub expires_at: OffsetDateTime,
}

/// Proof of payment: the preimage, already checked to hash to the payment hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentProof {
    pub payment_hash: String,
    pub preimage: String,
}

/// The receiver side: turns a tip into a validated invoice.
#[async_trait]
pub trait ReceiverEndpoint: Send + Sync {
    async fn invoice_for(
        &self,
        address: &LightningAddress,
        amount: Sats,
        comment: Option<&str>,
    ) -> Result<ValidatedInvoice, ResolveError>;

    /// Checks that an address resolves to a usable LNURL-pay endpoint (when a user links it).
    async fn probe(&self, address: &LightningAddress) -> Result<(), ResolveError>;
}

/// The sender side: one connected wallet.
#[async_trait]
pub trait SenderWallet: Send + Sync {
    async fn get_info(&self) -> Result<nwc::WalletInfo, nwc::PayError>;

    async fn pay_invoice(
        &self,
        bolt11: &str,
        payment_hash: &str,
        timeout: Duration,
    ) -> Result<PaymentProof, nwc::PayError>;

    async fn lookup_invoice(&self, payment_hash: &str) -> Result<nwc::Lookup, nwc::PayError>;
}

/// Opens a [`SenderWallet`] from a decrypted NWC URI. Behind a trait so tests use a fake wallet.
#[async_trait]
pub trait WalletConnector: Send + Sync {
    async fn connect(&self, nwc_uri: &str) -> Result<Box<dyn SenderWallet>, nwc::PayError>;
}
