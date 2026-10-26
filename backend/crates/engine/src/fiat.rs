//! Paying a tip out in local currency (FR-10, RFC 0004 "Pay sequence").
//!
//! The sender's wallet pays an invoice Bitnob generates for the payout, so MergeTip never holds
//! funds. Everything before the wallet is asked (quote, initialize, invoice checks) happens here;
//! reserving and paying reuse the Lightning path unchanged.

use std::sync::Arc;

use mergetip_crypto::Redacted;
use mergetip_fiat::{
    Beneficiary, FiatError, FiatProvider, InitializeRequest, LIGHTNING_CHAIN, LIGHTNING_SOURCE,
    QuoteRequest,
};
use mergetip_payer::{Network, ValidatedInvoice, invoice};
use mergetip_store::{
    feature_flags, payment_attempts,
    payout_methods::{self, PayoutMethod},
    tips::{self, FiatDetails, Tip},
    users::User,
};
use rust_decimal::Decimal;

use crate::{Engine, EngineError};

/// Fiat payouts, when configured (`Engine::with_fiat`).
pub(crate) struct FiatPayments {
    pub provider: Arc<dyn FiatProvider>,
    /// The Lightning network Bitnob's invoices must be for.
    pub network: Network,
}

/// Quotes per payment attempt: the first, and one re-quote if it expires (RFC 0004).
const QUOTE_ROUNDS: u32 = 2;
const PAYMENT_REASON: &str = "MergeTip tip";
const SATS_PER_BTC: u64 = 100_000_000;

/// What the fiat path decided before any money moves.
pub(crate) enum FiatInvoice {
    /// Pay this; the tip's fiat details are recorded.
    Ready(ValidatedInvoice),
    /// The tip is outside the corridor's limits: pay the Lightning address, or pledge.
    Fallback,
    /// Give up on this tip with a code for the sender.
    Fail(&'static str, Option<String>),
}

impl Engine {
    /// The receiver's default fiat method, if fiat payouts are on (configured and flagged) and
    /// the receiver chose fiat.
    pub(crate) async fn fiat_method(
        &self,
        receiver: &User,
    ) -> Result<Option<PayoutMethod>, EngineError> {
        if self.fiat.is_none()
            || !feature_flags::is_enabled(&self.pool, feature_flags::FIAT_PAYOUTS).await?
        {
            return Ok(None);
        }
        Ok(payout_methods::default_for_user(&self.pool, receiver.id).await?)
    }

    /// Quote → initialize → check the invoice, with one re-quote if the quote expires.
    pub(crate) async fn fiat_invoice(
        &self,
        tip: &Tip,
        method: &PayoutMethod,
    ) -> Result<FiatInvoice, EngineError> {
        let Some(fiat) = &self.fiat else {
            return Ok(FiatInvoice::Fallback);
        };
        let provider = fiat.provider.as_ref();
        let btc = Decimal::from(tip.amount.get()) / Decimal::from(SATS_PER_BTC);
        let msats = tip
            .amount
            .to_msats()
            .ok_or_else(|| EngineError::Malformed("tip amount overflows msat".into()))?;
        let limits = match provider.limits().await {
            Ok(limits) => limits,
            Err(err) => return fail_or(err),
        };
        let limit = limits.into_iter().find(|l| {
            l.country.eq_ignore_ascii_case(&method.country)
                && l.currency.eq_ignore_ascii_case(&method.currency)
        });
        let beneficiary = self.beneficiary(method)?;
        let attempt = payment_attempts::count_for_tip(&self.pool, tip.id).await? + 1;

        for round in 1..=QUOTE_ROUNDS {
            let quote = match provider
                .create_quote(&QuoteRequest {
                    from_asset: "btc".into(),
                    to_currency: method.currency.clone(),
                    source: LIGHTNING_SOURCE.into(),
                    chain: Some(LIGHTNING_CHAIN.into()),
                    amount: btc,
                    payment_reason: PAYMENT_REASON.into(),
                    // The first quote is the tip itself; re-quotes need their own reference.
                    reference: if attempt == 1 && round == 1 {
                        tip.id.to_string()
                    } else {
                        format!("{}-{attempt}-{round}", tip.id)
                    },
                    country: method.country.clone(),
                })
                .await
            {
                Ok(quote) => quote,
                Err(err) => return fail_or(err),
            };
            if limit.as_ref().is_some_and(|l| {
                quote.settlement_amount < l.min_amount || quote.settlement_amount > l.max_amount
            }) {
                tracing::info!(tip_id = %tip.id, currency = %method.currency,
                    "tip outside the fiat corridor's limits");
                return Ok(FiatInvoice::Fallback);
            }
            // Fees come out of the tip (PRD default): the sender pays exactly the tip.
            if quote.amount != btc {
                return Ok(FiatInvoice::Fail(
                    "fiat_quote_mismatch",
                    Some(format!("quoted {} BTC for a {btc} BTC tip", quote.amount)),
                ));
            }

            let payout = match provider
                .initialize_payout(
                    &quote.quote_id,
                    &InitializeRequest {
                        customer_id: method.provider_customer_id.clone(),
                        beneficiary: Redacted::new(beneficiary.expose().clone()),
                        reference: format!("{}:{attempt}:{round}", tip.id),
                        payment_reason: PAYMENT_REASON.into(),
                        callback_url: format!(
                            "{}/v1/webhooks/bitnob",
                            self.settings.api_base_url.trim_end_matches('/')
                        ),
                    },
                )
                .await
            {
                Ok(payout) => payout,
                Err(FiatError::QuoteExpired) => {
                    tracing::info!(tip_id = %tip.id, round, "fiat quote expired; re-quoting");
                    continue;
                }
                Err(err) => return fail_or(err),
            };
            let validated = match invoice::validate_provider_invoice(
                &payout.payment_request,
                msats,
                fiat.network,
                self.clock.now(),
            ) {
                Ok(validated) => validated,
                Err(invoice::InvoiceError::ExpiresTooSoon) => {
                    tracing::info!(tip_id = %tip.id, round, "fiat invoice expires too soon; re-quoting");
                    continue;
                }
                Err(err) => {
                    return Ok(FiatInvoice::Fail(
                        "fiat_invoice_invalid",
                        Some(err.to_string()),
                    ));
                }
            };

            tips::set_fiat_details(
                &self.pool,
                tip.id,
                &FiatDetails {
                    currency: Some(method.currency.clone()),
                    amount: Some(quote.settlement_amount),
                    fx_rate: Some(quote.exchange_rate.rate),
                    fees: quote.fees,
                    quote_id: Some(quote.quote_id.clone()),
                    payout_id: Some(payout.id.clone()),
                    provider_status: Some("initiated".into()),
                },
            )
            .await?;
            return Ok(FiatInvoice::Ready(validated));
        }
        Ok(FiatInvoice::Fail("fiat_quote_expired", None))
    }

    fn beneficiary(&self, method: &PayoutMethod) -> Result<Redacted<Beneficiary>, EngineError> {
        let plain = self
            .payments
            .keyring
            .open(
                &method.beneficiary_ciphertext,
                format!("payout_methods.beneficiary:{}", method.id).as_bytes(),
            )
            .map_err(|err| EngineError::Malformed(format!("cannot open a beneficiary: {err}")))?;
        serde_json::from_slice(plain.expose())
            .map(Redacted::new)
            .map_err(|_| EngineError::Malformed("stored beneficiary is not valid JSON".into()))
    }
}

/// Outages are retried later; refusals end the tip with a code for the sender.
fn fail_or(err: FiatError) -> Result<FiatInvoice, EngineError> {
    match err {
        err if err.is_retryable() => Err(EngineError::Retry {
            reason: err.to_string(),
            after: std::time::Duration::from_secs(60),
        }),
        FiatError::QuoteExpired => Ok(FiatInvoice::Fail("fiat_quote_expired", None)),
        other => Ok(FiatInvoice::Fail("fiat_rejected", Some(other.to_string()))),
    }
}
