//! Nostr Wallet Connect (NIP-47): asking the sender's own wallet to pay.

use std::fmt;

/// What the wallet says it can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletInfo {
    /// NIP-47 methods the connection may use, e.g. `pay_invoice`.
    pub methods: Vec<String>,
    /// `nip44` when the wallet supports NIP-44, otherwise `nip04`.
    pub encryption: Encryption,
    pub wallet_pubkey: String,
    pub relays: Vec<String>,
}

impl WalletInfo {
    pub fn supports(&self, method: &str) -> bool {
        self.methods.iter().any(|m| m == method)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encryption {
    Nip44,
    Nip04,
}

impl Encryption {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nip44 => "nip44",
            Self::Nip04 => "nip04",
        }
    }
}

/// The wallet's answer to `lookup_invoice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Paid; the preimage proves it.
    Settled { preimage: String },
    /// Still in flight (or not yet known to the wallet).
    Pending,
    /// The wallet gave up or never paid.
    Failed,
}

/// NIP-47 error codes, plus the ones a client can hit itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletErrorCode {
    RateLimited,
    NotImplemented,
    InsufficientBalance,
    PaymentFailed,
    NotFound,
    QuotaExceeded,
    Restricted,
    Unauthorized,
    Internal,
    Other,
}

impl WalletErrorCode {
    /// The NIP-47 name, stored in `payment_attempts.nwc_error_code`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "RATE_LIMITED",
            Self::NotImplemented => "NOT_IMPLEMENTED",
            Self::InsufficientBalance => "INSUFFICIENT_BALANCE",
            Self::PaymentFailed => "PAYMENT_FAILED",
            Self::NotFound => "NOT_FOUND",
            Self::QuotaExceeded => "QUOTA_EXCEEDED",
            Self::Restricted => "RESTRICTED",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Internal => "INTERNAL",
            Self::Other => "OTHER",
        }
    }

    /// Terminal errors won't change by retrying: the sender has to act (top up, raise the
    /// budget, reconnect).
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::QuotaExceeded
                | Self::InsufficientBalance
                | Self::Restricted
                | Self::Unauthorized
                | Self::NotImplemented
        )
    }
}

impl fmt::Display for WalletErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayError {
    #[error("not a valid NWC connection string")]
    InvalidUri,
    /// The wallet answered with an error.
    #[error("wallet error {code}: {message}")]
    Wallet {
        code: WalletErrorCode,
        message: String,
    },
    /// No answer in time. For `pay_invoice` the payment may still have gone through, so the
    /// outcome is unknown and must be reconciled, never blindly retried.
    #[error("the wallet did not answer in time")]
    Timeout,
    /// Couldn't reach the relay (before or while sending).
    #[error("relay error: {0}")]
    Relay(String),
    /// The wallet answered something we can't use (bad JSON, a preimage that doesn't match).
    #[error("unexpected wallet response: {0}")]
    Protocol(String),
}
