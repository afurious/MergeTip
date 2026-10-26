//! Fiat payouts (FR-10, RFC 0004): the [`FiatProvider`] seam and its Bitnob implementation.
//!
//! Field names come from Bitnob's public API reference. Where the docs are silent the code says
//! so with `ASSUMPTION(bitnob)`, so one sandbox run shows exactly which lines to adjust.

mod client;
pub mod signing;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::collections::BTreeMap;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use zeroize::Zeroize;

pub use client::{BitnobClient, BitnobConfig};

// ASSUMPTION(bitnob): Bitnob's settlement-models page says on-the-fly payouts can be funded "by
// paying a generated … Lightning … invoice", but the quote reference documents `source` only as
// `onchain`/`offchain` and `chain` only with on-chain networks. These are our best reading; a
// sandbox run confirms or corrects them (BLOCKERS.md).
/// `source` for a quote funded by paying a Lightning invoice.
pub const LIGHTNING_SOURCE: &str = "onchain";
/// `chain` for a quote funded by paying a Lightning invoice.
pub const LIGHTNING_CHAIN: &str = "LIGHTNING";

/// Bitnob sends amounts sometimes as JSON strings, sometimes as numbers; both become exact
/// decimals (never floats: a number is re-read from its literal text).
mod amount {
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, de::Error};

    pub fn de<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let text = match &value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            other => return Err(D::Error::custom(format!("not an amount: {other}"))),
        };
        Decimal::from_str(text.trim())
            .or_else(|_| Decimal::from_scientific(text.trim()))
            .map_err(D::Error::custom)
    }

    pub fn de_opt<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Decimal>, D::Error> {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(v) => de(v).map(Some).map_err(D::Error::custom),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FiatError {
    /// Bitnob couldn't be reached, timed out or failed (5xx); worth retrying later.
    #[error("payout provider unavailable: {0}")]
    Unavailable(String),
    /// Bitnob refused the request (4xx). `code` is its error code when it sent one.
    #[error("payout provider refused ({status}): {message}")]
    Rejected {
        status: u16,
        code: Option<String>,
        message: String,
    },
    /// The quote or its invoice expired before it could be used.
    #[error("payout quote expired")]
    QuoteExpired,
    #[error("unexpected payout provider response: {0}")]
    Decode(String),
}

impl FiatError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// A destination country and its corridors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Country {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub corridors: Vec<Corridor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Corridor {
    pub currency: String,
    #[serde(default)]
    pub destination_types: Vec<String>,
}

/// What a receiver must enter for each payout rail in a country. The web form is generated from
/// this; nothing about it is hard-coded (FR-10.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CountryRequirements {
    pub code: String,
    pub name: String,
    /// Keyed by rail, e.g. `bank`, `mobile_money`.
    #[serde(default)]
    pub destination_types: BTreeMap<String, Rail>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rail {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub fields: Vec<FormField>,
    #[serde(default)]
    pub banks: Option<Vec<Bank>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub min_length: Option<u32>,
    #[serde(default)]
    pub max_length: Option<u32>,
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Choices for select fields; passed through as Bitnob sends them.
    #[serde(default)]
    pub options: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bank {
    pub bank_code: String,
    pub bank_name: String,
}

/// Payout limits for one corridor, in the corridor's currency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    pub country: String,
    pub currency: String,
    #[serde(deserialize_with = "amount::de")]
    pub min_amount: Decimal,
    #[serde(deserialize_with = "amount::de")]
    pub max_amount: Decimal,
}

/// Name enquiry before saving a payout method (FR-10.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountQuery {
    pub country: String,
    /// `mobile_money` for mobile-money countries, `None` for banks.
    pub kind: Option<String>,
    /// The bank code, or the mobile-money network (e.g. `MTN`).
    pub bank_code: String,
    pub account_number: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResolvedAccount {
    pub account_name: String,
    #[serde(default)]
    pub bank_name: Option<String>,
    #[serde(default)]
    pub bank_code: Option<String>,
    #[serde(default)]
    pub is_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteRequest {
    pub from_asset: String,
    pub to_currency: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    /// The amount of `from_asset` to convert (BTC for us).
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub payment_reason: String,
    pub reference: String,
    pub country: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExchangeRate {
    #[serde(deserialize_with = "amount::de")]
    pub rate: Decimal,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Quote {
    pub id: String,
    /// The ID Initialize takes in its path.
    pub quote_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(deserialize_with = "amount::de")]
    pub amount: Decimal,
    #[serde(deserialize_with = "amount::de")]
    pub settlement_amount: Decimal,
    pub exchange_rate: ExchangeRate,
    #[serde(default, deserialize_with = "amount::de_opt")]
    pub fees: Option<Decimal>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Where the money goes. Sensitive: zeroised on drop, and never logged (`Debug` hides it).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Zeroize)]
pub struct Beneficiary {
    pub destination_type: String,
    pub country: String,
    pub account_name: String,
    pub account_number: String,
    pub bank_code: String,
    /// Mobile-money network, e.g. `MTN`, `MPESA`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

impl std::fmt::Debug for Beneficiary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Beneficiary")
            .field("destination_type", &self.destination_type)
            .field("country", &self.country)
            .finish_non_exhaustive()
    }
}

impl Beneficiary {
    /// The account as shown back to its owner: last four characters only.
    pub fn masked(&self) -> String {
        let tail: String = self
            .account_number
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("****{tail}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InitializeRequest {
    pub customer_id: Option<String>,
    pub beneficiary: mergetip_crypto::Redacted<Beneficiary>,
    pub reference: String,
    pub payment_reason: String,
    pub callback_url: String,
}

/// An initialized payout: pay `payment_request` before `expires_at` and Bitnob pays out.
#[derive(Debug, Clone, PartialEq)]
pub struct InitializedPayout {
    pub id: String,
    pub status: String,
    pub payment_request: String,
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Payout {
    pub id: String,
    #[serde(default)]
    pub reference: Option<String>,
    pub status: String,
    #[serde(default, deserialize_with = "amount::de_opt")]
    pub settlement_amount: Option<Decimal>,
}

/// A payout rail that pays local currency. Behind a trait so the engine is tested without
/// Bitnob, and so a second provider (e.g. Kotani Pay) can be added.
#[async_trait]
pub trait FiatProvider: Send + Sync {
    async fn supported_countries(&self) -> Result<Vec<Country>, FiatError>;
    async fn country_requirements(&self, code: &str) -> Result<CountryRequirements, FiatError>;
    async fn limits(&self) -> Result<Vec<Limit>, FiatError>;
    async fn account_lookup(&self, query: &AccountQuery) -> Result<ResolvedAccount, FiatError>;
    async fn create_quote(&self, request: &QuoteRequest) -> Result<Quote, FiatError>;
    async fn initialize_payout(
        &self,
        quote_id: &str,
        request: &InitializeRequest,
    ) -> Result<InitializedPayout, FiatError>;
    async fn get_payout(&self, id: &str) -> Result<Payout, FiatError>;
}

/// Payout status, ranked so a late webhook or poll never moves a tip backwards. Bitnob uses
/// upper-case statuses on payouts and lower-case ones in webhooks; both map here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PayoutStatus {
    Initiated,
    Processing,
    /// Final.
    Success,
    /// Final: expired or failed; the sats are with Bitnob, the fiat wasn't delivered.
    Failed,
}

impl PayoutStatus {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "quote" | "initiated" | "pending" => Some(Self::Initiated),
            "processing" => Some(Self::Processing),
            "success" | "completed" => Some(Self::Success),
            "expired" | "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initiated => "initiated",
            Self::Processing => "processing",
            Self::Success => "success",
            Self::Failed => "failed",
        }
    }

    pub const fn is_final(self) -> bool {
        matches!(self, Self::Success | Self::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_rank_forward_only() {
        let parse = |s| PayoutStatus::parse(s).unwrap();
        assert!(parse("INITIATED") < parse("processing"));
        assert!(parse("processing") < parse("COMPLETED"));
        assert!(parse("pending") < parse("expired"));
        assert_eq!(PayoutStatus::parse("weird"), None);
        assert!(parse("success").is_final() && !parse("processing").is_final());
    }

    #[test]
    fn beneficiaries_never_print_their_account() {
        let b = Beneficiary {
            destination_type: "bank".into(),
            country: "NG".into(),
            account_name: "Ada Obi".into(),
            account_number: "0123456789".into(),
            bank_code: "058".into(),
            network: None,
        };
        let shown = format!("{b:?}");
        assert!(!shown.contains("0123456789") && !shown.contains("Ada"));
        assert_eq!(b.masked(), "****6789");
    }
}
