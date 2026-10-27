//! LNURL-pay: from a Lightning address to an invoice (LUD-06, LUD-12 comments, LUD-16 addresses).
//!
//! Requests go over HTTPS only. Plain HTTP is allowed solely for local hosts (loopback and
//! single-label Docker service names) and only when the client was built with
//! `allow_local_http`, for the mock LNURL server in development.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use mergetip_core::{Clock, Msats, Sats};
use serde::Deserialize;

use crate::{InvoiceError, LightningAddress, Network, ReceiverEndpoint, ValidatedInvoice, invoice};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Server errors are retried this many times (connection errors too); anything else is final.
const RETRIES: usize = 2;
const RETRY_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("{0:?} is not a Lightning address")]
    InvalidAddress(String),
    /// The server couldn't be reached or kept failing; worth trying again later.
    #[error("Lightning address server unreachable: {0}")]
    Unreachable(String),
    /// The server answered, but not with a usable LNURL-pay response.
    #[error("invalid LNURL-pay response: {0}")]
    Invalid(String),
    /// The server refused (`{"status":"ERROR"}`).
    #[error("the Lightning address server refused: {0}")]
    Refused(String),
    #[error("amount outside what this address accepts ({min} to {max} msat)")]
    AmountOutOfRange { min: u64, max: u64 },
    #[error(transparent)]
    Invoice(#[from] InvoiceError),
}

impl ResolveError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }
}

/// The LNURL-pay parameters behind a Lightning address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayParams {
    pub callback: reqwest::Url,
    pub min: Msats,
    pub max: Msats,
    /// The raw metadata string; the invoice's description hash must be its SHA-256.
    pub metadata: String,
    /// Longest comment accepted (LUD-12); 0 means none.
    pub comment_allowed: usize,
}

#[derive(Debug, Deserialize)]
struct PayRequest {
    tag: Option<String>,
    callback: Option<String>,
    #[serde(rename = "minSendable")]
    min_sendable: Option<u64>,
    #[serde(rename = "maxSendable")]
    max_sendable: Option<u64>,
    metadata: Option<String>,
    #[serde(rename = "commentAllowed", default)]
    comment_allowed: Option<usize>,
    status: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallbackResponse {
    pr: Option<String>,
    status: Option<String>,
    reason: Option<String>,
}

pub struct LnurlClient {
    http: reqwest::Client,
    network: Network,
    allow_local_http: bool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for LnurlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LnurlClient")
            .field("network", &self.network)
            .field("allow_local_http", &self.allow_local_http)
            .finish_non_exhaustive()
    }
}

impl LnurlClient {
    pub fn new(
        network: Network,
        allow_local_http: bool,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ResolveError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("mergetip/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // A redirect could leave HTTPS or the address's own domain; LNURL never needs one.
            .redirect(reqwest::redirect::Policy::none());
        // Outside local development, names may only resolve to public addresses (SSRF).
        if !allow_local_http {
            builder = builder.dns_resolver(Arc::new(crate::egress::PublicOnlyResolver));
        }
        let http = builder
            .build()
            .map_err(|err| ResolveError::Unreachable(err.to_string()))?;
        Ok(Self {
            http,
            network,
            allow_local_http,
            clock,
        })
    }

    /// Fetches and checks the address's LNURL-pay parameters.
    pub async fn pay_params(&self, address: &LightningAddress) -> Result<PayParams, ResolveError> {
        let scheme = self.scheme_for(address)?;
        let url = format!(
            "{scheme}://{}/.well-known/lnurlp/{}",
            address.domain(),
            address.user()
        );
        let url =
            reqwest::Url::parse(&url).map_err(|err| ResolveError::Invalid(err.to_string()))?;
        let body: PayRequest = self.get_json(url).await?;
        if body.status.as_deref() == Some("ERROR") {
            return Err(ResolveError::Refused(body.reason.unwrap_or_default()));
        }
        if body.tag.as_deref() != Some("payRequest") {
            return Err(ResolveError::Invalid("not a payRequest".into()));
        }
        let (Some(callback), Some(min), Some(max), Some(metadata)) = (
            body.callback,
            body.min_sendable,
            body.max_sendable,
            body.metadata,
        ) else {
            return Err(ResolveError::Invalid("missing payRequest fields".into()));
        };
        if min == 0 || min > max {
            return Err(ResolveError::Invalid(format!(
                "bad amount range {min}..{max}"
            )));
        }
        let callback =
            reqwest::Url::parse(&callback).map_err(|err| ResolveError::Invalid(err.to_string()))?;
        self.check_callback(&callback, address)?;
        Ok(PayParams {
            callback,
            min: Msats::new(min),
            max: Msats::new(max),
            metadata,
            comment_allowed: body.comment_allowed.unwrap_or(0),
        })
    }

    /// Requests and validates an invoice for exactly `amount`.
    pub async fn invoice(
        &self,
        address: &LightningAddress,
        amount: Sats,
        comment: Option<&str>,
    ) -> Result<ValidatedInvoice, ResolveError> {
        let params = self.pay_params(address).await?;
        let msats = amount.to_msats().ok_or(ResolveError::AmountOutOfRange {
            min: params.min.get(),
            max: params.max.get(),
        })?;
        if msats < params.min || msats > params.max {
            return Err(ResolveError::AmountOutOfRange {
                min: params.min.get(),
                max: params.max.get(),
            });
        }

        let mut callback = params.callback.clone();
        {
            let mut query = callback.query_pairs_mut();
            query.append_pair("amount", &msats.get().to_string());
            if let Some(comment) = comment.and_then(|c| trim_comment(c, params.comment_allowed)) {
                query.append_pair("comment", &comment);
            }
        }
        let body: CallbackResponse = self.get_json(callback).await?;
        if body.status.as_deref() == Some("ERROR") {
            return Err(ResolveError::Refused(body.reason.unwrap_or_default()));
        }
        let bolt11 = body
            .pr
            .ok_or_else(|| ResolveError::Invalid("callback returned no invoice".into()))?;
        Ok(invoice::validate(
            &bolt11,
            msats,
            &params.metadata,
            self.network,
            self.clock.now(),
        )?)
    }

    fn scheme_for(&self, address: &LightningAddress) -> Result<&'static str, ResolveError> {
        if self.allow_local_http && address.is_local() {
            Ok("http")
        } else if address.is_local() || !crate::egress::host_allowed(address.host()) {
            Err(ResolveError::InvalidAddress(address.to_string()))
        } else {
            Ok("https")
        }
    }

    /// The callback must use HTTPS too. Plain HTTP is accepted only for the local mock: a local
    /// address, local HTTP allowed, and a callback that is itself on a local host.
    fn check_callback(
        &self,
        callback: &reqwest::Url,
        address: &LightningAddress,
    ) -> Result<(), ResolveError> {
        let local_host = callback
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || !host.contains('.'));
        let local_mock = self.allow_local_http
            && address.is_local()
            && callback.scheme() == "http"
            && local_host;
        if local_mock {
            return Ok(());
        }
        if callback.scheme() != "https" {
            return Err(ResolveError::Invalid("callback is not HTTPS".into()));
        }
        // The server chooses the callback: it must not aim us at an internal address.
        if !callback.host_str().is_some_and(crate::egress::host_allowed) {
            return Err(ResolveError::Invalid(
                "callback points at a non-public host".into(),
            ));
        }
        Ok(())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: reqwest::Url,
    ) -> Result<T, ResolveError> {
        let mut last_error = String::new();
        for attempt in 0..=RETRIES {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            let response = match self.http.get(url.clone()).send().await {
                Ok(response) => response,
                Err(err) => {
                    last_error = err.without_url().to_string();
                    continue;
                }
            };
            let status = response.status();
            if status.is_server_error() {
                last_error = format!("HTTP {status}");
                continue;
            }
            // LNURL servers report refusals in a JSON body, sometimes with a 4xx status.
            let bytes = response
                .bytes()
                .await
                .map_err(|err| ResolveError::Unreachable(err.without_url().to_string()))?;
            return serde_json::from_slice(&bytes)
                .map_err(|_| ResolveError::Invalid(format!("HTTP {status} with a non-JSON body")));
        }
        Err(ResolveError::Unreachable(last_error))
    }
}

/// Fits a note into the comment length the server allows, on a character boundary.
fn trim_comment(comment: &str, allowed: usize) -> Option<String> {
    let comment = comment.trim();
    if allowed == 0 || comment.is_empty() {
        return None;
    }
    Some(comment.chars().take(allowed).collect())
}

#[async_trait]
impl ReceiverEndpoint for LnurlClient {
    async fn invoice_for(
        &self,
        address: &LightningAddress,
        amount: Sats,
        comment: Option<&str>,
    ) -> Result<ValidatedInvoice, ResolveError> {
        self.invoice(address, amount, comment).await
    }

    async fn probe(&self, address: &LightningAddress) -> Result<(), ResolveError> {
        self.pay_params(address).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_trimmed_to_the_allowed_length() {
        assert_eq!(trim_comment("for the review", 0), None);
        assert_eq!(trim_comment("  ", 10), None);
        assert_eq!(
            trim_comment("for the review", 7).as_deref(),
            Some("for the")
        );
        assert_eq!(trim_comment("ünïcödé", 3).as_deref(), Some("ünï"));
    }
}

#[cfg(test)]
#[path = "lnurl_tests.rs"]
mod http_tests;
