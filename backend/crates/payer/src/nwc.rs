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

use std::time::Duration;

use async_trait::async_trait;
use nostr::nips::nip47::{
    ErrorCode, LookupInvoiceRequest, NIP47Error, Nip47Ciphers, Nip47Tag, NostrWalletConnectUri,
    PayInvoiceRequest, Request, Response, ResponseResult, TransactionState,
};
use nostr_sdk::prelude::{Client, Event, Filter, Kind, ReqExitPolicy, StreamExt, Timestamp};
use sha2::{Digest, Sha256};

use crate::{PaymentProof, SenderWallet, WalletConnector};

/// How long to wait for relays to connect and for the wallet's info event.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// `get_info` and `lookup_invoice` are quick; `pay_invoice` gets the caller's timeout.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// A live NIP-47 connection to one wallet.
pub struct NwcWallet {
    uri: NostrWalletConnectUri,
    client: Client,
    cipher: Nip47Ciphers,
    /// Methods from the wallet's info event, used when `get_info` itself isn't permitted.
    advertised: Vec<String>,
}

impl std::fmt::Debug for NwcWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NwcWallet")
            .field("wallet", &self.uri.public_key.to_hex())
            .finish_non_exhaustive()
    }
}

impl NwcWallet {
    /// Parses the URI, connects to its relays and reads the wallet's info event (supported
    /// methods and encryption). The URI holds a secret: it is never logged.
    pub async fn connect(nwc_uri: &str) -> Result<Self, PayError> {
        let uri = NostrWalletConnectUri::parse(nwc_uri.trim()).map_err(|_| PayError::InvalidUri)?;
        let client = Client::default();
        for relay in &uri.relays {
            client
                .add_relay(relay)
                .await
                .map_err(|err| PayError::Relay(err.to_string()))?;
        }
        client.connect().and_wait(CONNECT_TIMEOUT).await;

        let filter = Filter::new()
            .kind(Kind::WalletConnectInfo)
            .author(uri.public_key)
            .limit(1);
        let info: Option<Event> = client
            .fetch_events(filter)
            .timeout(CONNECT_TIMEOUT)
            .max_events(1)
            .await
            .map_err(|err| PayError::Relay(err.to_string()))?
            .into_iter()
            .next();
        let (cipher, advertised) = match &info {
            Some(event) => (cipher_from(event), methods_from(&event.content)),
            None => (Nip47Ciphers::NIP04, Vec::new()),
        };
        Ok(Self {
            uri,
            client,
            cipher,
            advertised,
        })
    }

    async fn request(&self, request: Request, timeout: Duration) -> Result<Response, PayError> {
        let event = request
            .to_event(&self.uri, self.cipher)
            .map_err(|err| PayError::Protocol(err.to_string()))?;
        let filter = Filter::new()
            .author(self.uri.public_key)
            .kind(Kind::WalletConnectResponse)
            .event(event.id)
            .since(Timestamp::now() - Duration::from_secs(60));
        let mut stream = self
            .client
            .stream_events(filter)
            .timeout(timeout)
            .policy(ReqExitPolicy::WaitForEvents(1))
            .await
            .map_err(|err| PayError::Relay(err.to_string()))?;
        self.client
            .send_event(&event)
            .await
            .map_err(|err| PayError::Relay(err.to_string()))?;
        let (_, received) = stream.next().await.ok_or(PayError::Timeout)?;
        let received = received.map_err(|err| PayError::Relay(err.to_string()))?;
        Response::from_event(&self.uri, &received, self.cipher)
            .map_err(|err| PayError::Protocol(err.to_string()))
    }
}

fn cipher_from(info: &Event) -> Nip47Ciphers {
    info.tags
        .iter()
        .find_map(|tag| match Nip47Tag::try_from(tag).ok()? {
            Nip47Tag::Encryption(ciphers) => Some(ciphers.latest()),
        })
        .unwrap_or(Nip47Ciphers::NIP04)
}

/// The info event's content is a space-separated list of supported methods (NIP-47).
fn methods_from(content: &str) -> Vec<String> {
    content.split_whitespace().map(str::to_owned).collect()
}

fn wallet_error(error: NIP47Error) -> PayError {
    let code = match error.code {
        ErrorCode::RateLimited => WalletErrorCode::RateLimited,
        ErrorCode::NotImplemented => WalletErrorCode::NotImplemented,
        ErrorCode::InsufficientBalance => WalletErrorCode::InsufficientBalance,
        ErrorCode::PaymentFailed => WalletErrorCode::PaymentFailed,
        ErrorCode::NotFound => WalletErrorCode::NotFound,
        ErrorCode::QuotaExceeded => WalletErrorCode::QuotaExceeded,
        ErrorCode::Restricted => WalletErrorCode::Restricted,
        ErrorCode::Unauthorized => WalletErrorCode::Unauthorized,
        ErrorCode::Internal => WalletErrorCode::Internal,
        ErrorCode::Other => WalletErrorCode::Other,
    };
    PayError::Wallet {
        code,
        message: error.message.chars().take(200).collect(),
    }
}

/// A wallet's preimage is only proof if it hashes to the invoice's payment hash.
fn check_preimage(preimage: &str, payment_hash: &str) -> Result<(), PayError> {
    let bytes = hex::decode(preimage.trim())
        .map_err(|_| PayError::Protocol("preimage is not hex".into()))?;
    if hex::encode(Sha256::digest(&bytes)).eq_ignore_ascii_case(payment_hash) {
        Ok(())
    } else {
        Err(PayError::Protocol(
            "preimage does not match the payment hash".into(),
        ))
    }
}

#[async_trait]
impl SenderWallet for NwcWallet {
    async fn get_info(&self) -> Result<WalletInfo, PayError> {
        let methods = match self
            .request(Request::get_info(), QUERY_TIMEOUT)
            .await?
            .to_get_info()
        {
            Ok(info) => info.methods.iter().map(|m| m.as_str().to_owned()).collect(),
            // A connection may be allowed to pay without being allowed to ask for its info.
            Err(_) if !self.advertised.is_empty() => self.advertised.clone(),
            Err(err) => return Err(PayError::Protocol(err.to_string())),
        };
        Ok(WalletInfo {
            methods,
            encryption: if self.cipher == Nip47Ciphers::NIP44V2 {
                Encryption::Nip44
            } else {
                Encryption::Nip04
            },
            wallet_pubkey: self.uri.public_key.to_hex(),
            relays: self.uri.relays.iter().map(ToString::to_string).collect(),
        })
    }

    async fn pay_invoice(
        &self,
        bolt11: &str,
        payment_hash: &str,
        timeout: Duration,
    ) -> Result<PaymentProof, PayError> {
        let response = self
            .request(
                Request::pay_invoice(PayInvoiceRequest::new(bolt11)),
                timeout,
            )
            .await?;
        if let Some(error) = response.error {
            return Err(wallet_error(error));
        }
        let Some(ResponseResult::PayInvoice(paid)) = response.result else {
            return Err(PayError::Protocol(
                "pay_invoice answered without a result".into(),
            ));
        };
        check_preimage(&paid.preimage, payment_hash)?;
        Ok(PaymentProof {
            payment_hash: payment_hash.to_ascii_lowercase(),
            preimage: paid.preimage.trim().to_ascii_lowercase(),
        })
    }

    async fn lookup_invoice(&self, payment_hash: &str) -> Result<Lookup, PayError> {
        let request = Request::lookup_invoice(LookupInvoiceRequest {
            payment_hash: Some(payment_hash.to_owned()),
            invoice: None,
        });
        let response = self.request(request, QUERY_TIMEOUT).await?;
        match (response.error, response.result) {
            // Not (yet) known to the wallet: as far as we can tell, still in flight.
            (Some(error), _) if error.code == ErrorCode::NotFound => Ok(Lookup::Pending),
            (Some(error), _) => Err(wallet_error(error)),
            (None, Some(ResponseResult::LookupInvoice(found))) => {
                match (found.state, found.preimage) {
                    (_, Some(preimage)) if check_preimage(&preimage, payment_hash).is_ok() => {
                        Ok(Lookup::Settled {
                            preimage: preimage.trim().to_ascii_lowercase(),
                        })
                    }
                    (Some(TransactionState::Failed | TransactionState::Expired), _) => {
                        Ok(Lookup::Failed)
                    }
                    _ => Ok(Lookup::Pending),
                }
            }
            (None, _) => Err(PayError::Protocol(
                "lookup_invoice answered without a result".into(),
            )),
        }
    }
}

/// Opens real NIP-47 connections.
#[derive(Debug, Default, Clone, Copy)]
pub struct NwcConnector;

#[async_trait]
impl WalletConnector for NwcConnector {
    async fn connect(&self, nwc_uri: &str) -> Result<Box<dyn SenderWallet>, PayError> {
        Ok(Box::new(NwcWallet::connect(nwc_uri).await?))
    }
}

#[cfg(test)]
#[path = "nwc_tests.rs"]
mod relay_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every NIP-47 code maps to ours, keeps its wire name, and is classified the way RFC 0002
    /// says: terminal codes need the sender to act; the rest may succeed on a retry.
    #[test]
    fn every_wallet_error_code_maps_and_classifies() {
        let table = [
            (ErrorCode::RateLimited, "RATE_LIMITED", false),
            (ErrorCode::NotImplemented, "NOT_IMPLEMENTED", true),
            (ErrorCode::InsufficientBalance, "INSUFFICIENT_BALANCE", true),
            (ErrorCode::PaymentFailed, "PAYMENT_FAILED", false),
            (ErrorCode::NotFound, "NOT_FOUND", false),
            (ErrorCode::QuotaExceeded, "QUOTA_EXCEEDED", true),
            (ErrorCode::Restricted, "RESTRICTED", true),
            (ErrorCode::Unauthorized, "UNAUTHORIZED", true),
            (ErrorCode::Internal, "INTERNAL", false),
            (ErrorCode::Other, "OTHER", false),
        ];
        for (nip47, name, terminal) in table {
            let err = wallet_error(NIP47Error {
                code: nip47,
                message: "x".repeat(500),
            });
            let PayError::Wallet { code, message } = err else {
                panic!("{name}: not a wallet error");
            };
            assert_eq!(code.as_str(), name);
            assert_eq!(code.to_string(), name);
            assert_eq!(code.is_terminal(), terminal, "{name}");
            assert_eq!(message.len(), 200, "wallet messages are bounded");
        }
    }

    #[test]
    fn encryption_names_match_the_database() {
        assert_eq!(Encryption::Nip44.as_str(), "nip44");
        assert_eq!(Encryption::Nip04.as_str(), "nip04");
    }
}
