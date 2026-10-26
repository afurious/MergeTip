//! [`BitnobClient`]: signed HTTPS calls to the Bitnob Payouts API.

use std::time::Duration;

use async_trait::async_trait;
use mergetip_crypto::{Redacted, random_bytes};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::{
    AccountQuery, Country, CountryRequirements, FiatError, FiatProvider, InitializeRequest,
    InitializedPayout, Limit, Payout, Quote, QuoteRequest, ResolvedAccount, signing,
};

/// Retries after a 5xx or network failure (never after a 4xx: Bitnob answered).
const MAX_RETRIES: u32 = 2;

#[derive(Debug, Clone)]
pub struct BitnobConfig {
    /// `https://api.bitnob.com` in production.
    pub base_url: String,
    pub client_id: String,
    pub client_secret: Redacted<String>,
    /// Wait before each retry (multiplied by the attempt number).
    pub retry_backoff: Duration,
}

pub struct BitnobClient {
    http: reqwest::Client,
    config: BitnobConfig,
}

impl std::fmt::Debug for BitnobClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitnobClient")
            .field("base_url", &self.config.base_url)
            .finish_non_exhaustive()
    }
}

impl BitnobClient {
    pub fn new(config: BitnobConfig) -> Result<Self, FiatError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mergetip/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|err| FiatError::Unavailable(err.to_string()))?;
        Ok(Self { http, config })
    }

    /// One signed call with bounded retries. Returns the response's `data`.
    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, FiatError> {
        let body = body.map(Value::to_string).unwrap_or_default();
        let mut attempt = 0;
        loop {
            match self.call_once(method.clone(), path, query, &body).await {
                Err(err) if err.is_retryable() && attempt < MAX_RETRIES => {
                    attempt += 1;
                    tracing::info!(path, attempt, error = %err, "retrying Bitnob call");
                    tokio::time::sleep(self.config.retry_backoff * attempt).await;
                }
                other => return other,
            }
        }
    }

    async fn call_once(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: &str,
    ) -> Result<Value, FiatError> {
        let nonce = hex::encode(
            random_bytes::<16>().map_err(|err| FiatError::Unavailable(err.to_string()))?,
        );
        // A fresh timestamp and nonce per attempt: Bitnob rejects replayed nonces.
        let auth = signing::sign_request(
            &self.config.client_id,
            self.config.client_secret.expose(),
            OffsetDateTime::now_utc().unix_timestamp(),
            &nonce,
            body,
        );
        let mut url = reqwest::Url::parse(&format!(
            "{}{path}",
            self.config.base_url.trim_end_matches('/')
        ))
        .map_err(|err| FiatError::Decode(format!("bad Bitnob URL: {err}")))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut request = self
            .http
            .request(method, url)
            .header("accept", "application/json")
            .header("X-Auth-Client", auth.client)
            .header("X-Auth-Timestamp", auth.timestamp)
            .header("X-Auth-Nonce", auth.nonce)
            .header("X-Auth-Signature", auth.signature);
        if !body.is_empty() {
            request = request
                .header("content-type", "application/json")
                .body(body.to_owned());
        }
        let response = request
            .send()
            .await
            .map_err(|err| FiatError::Unavailable(err.without_url().to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| FiatError::Unavailable(err.to_string()))?;
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(FiatError::Unavailable(format!("HTTP {status}")));
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|_| FiatError::Decode(format!("HTTP {status}, body is not JSON")))?;
        if !status.is_success() {
            return Err(rejection(status, &parsed));
        }
        // ASSUMPTION(bitnob): responses wrap their payload as `{ "success", "message", "data" }`,
        // as the Initialize reference shows; a bare payload is accepted too.
        Ok(parsed.get("data").cloned().unwrap_or(parsed))
    }
}

/// A 4xx: Bitnob's own code and message when it sent them.
fn rejection(status: StatusCode, body: &Value) -> FiatError {
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("request refused")
        .to_owned();
    let code = body
        .get("code")
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    // ASSUMPTION(bitnob): an expired quote is a 4xx whose code or message says "expired".
    let says_expired = |s: &str| s.to_ascii_lowercase().contains("expired");
    if says_expired(&message) || code.as_deref().is_some_and(says_expired) {
        return FiatError::QuoteExpired;
    }
    FiatError::Rejected {
        status: status.as_u16(),
        code,
        message,
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, FiatError> {
    serde_json::from_value(value).map_err(|err| FiatError::Decode(err.to_string()))
}

/// IDs from Bitnob go into URL paths; refuse anything that isn't a plain identifier.
fn path_segment(id: &str) -> Result<&str, FiatError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok {
        Ok(id)
    } else {
        Err(FiatError::Decode(format!("unusable identifier {id:?}")))
    }
}

/// Picks `data.{key}` when present, else `data` itself.
fn unwrap_key(data: Value, key: &str) -> Value {
    match data {
        Value::Object(mut map) if map.contains_key(key) => map.remove(key).unwrap_or(Value::Null),
        other => other,
    }
}

#[async_trait]
impl FiatProvider for BitnobClient {
    async fn supported_countries(&self) -> Result<Vec<Country>, FiatError> {
        let data = self
            .call(Method::GET, "/api/payouts/supported-countries", &[], None)
            .await?;
        decode(unwrap_key(data, "countries"))
    }

    async fn country_requirements(&self, code: &str) -> Result<CountryRequirements, FiatError> {
        let code = path_segment(code)?;
        let data = self
            .call(
                Method::GET,
                &format!("/api/payouts/supported-countries/{code}"),
                &[],
                None,
            )
            .await?;
        decode(data)
    }

    async fn limits(&self) -> Result<Vec<Limit>, FiatError> {
        let data = self
            .call(Method::GET, "/api/payouts/limits", &[], None)
            .await?;
        decode(unwrap_key(data, "limits"))
    }

    async fn account_lookup(&self, query: &AccountQuery) -> Result<ResolvedAccount, FiatError> {
        let mut params = vec![
            ("country", query.country.as_str()),
            ("bank_code", query.bank_code.as_str()),
            ("account_number", query.account_number.as_str()),
        ];
        if let Some(kind) = &query.kind {
            params.push(("type", kind.as_str()));
        }
        let data = self
            .call(Method::GET, "/api/payouts/account-lookup", &params, None)
            .await?;
        decode(data)
    }

    async fn create_quote(&self, request: &QuoteRequest) -> Result<Quote, FiatError> {
        let body =
            serde_json::to_value(request).map_err(|err| FiatError::Decode(err.to_string()))?;
        let data = self
            .call(Method::POST, "/api/payouts/quotes", &[], Some(&body))
            .await?;
        decode(data)
    }

    async fn initialize_payout(
        &self,
        quote_id: &str,
        request: &InitializeRequest,
    ) -> Result<InitializedPayout, FiatError> {
        let quote_id = path_segment(quote_id)?;
        let mut body = json!({
            "quote_id": quote_id,
            "reference": request.reference,
            "payment_reason": request.payment_reason,
            "callback_url": request.callback_url,
            "beneficiary": request.beneficiary.expose(),
        });
        if let Some(customer) = &request.customer_id {
            body["customer_id"] = json!(customer);
        }
        let data = self
            .call(
                Method::POST,
                &format!("/api/payouts/{quote_id}/initialize"),
                &[],
                Some(&body),
            )
            .await?;
        let payout = unwrap_key(data, "payout");
        // ASSUMPTION(bitnob): the docs name `expires_at` and say a Lightning invoice is returned
        // for on-the-fly settlement, but not the invoice's field; these are the candidates seen
        // across Bitnob's docs, first match wins.
        let payment_request = [
            "payment_request",
            "lightning_invoice",
            "paymentAddress",
            "payment_address",
        ]
        .iter()
        .find_map(|key| payout.get(*key).and_then(Value::as_str))
        .ok_or_else(|| FiatError::Decode("initialize returned no invoice".into()))?
        .to_owned();
        let field = |key: &str| {
            payout
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| FiatError::Decode(format!("initialize returned no {key}")))
        };
        let expires_at = OffsetDateTime::parse(
            &field("expires_at")?,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|err| FiatError::Decode(format!("bad expires_at: {err}")))?;
        Ok(InitializedPayout {
            id: field("id")?,
            status: field("status")?,
            payment_request,
            expires_at,
        })
    }

    async fn get_payout(&self, id: &str) -> Result<Payout, FiatError> {
        let id = path_segment(id)?;
        let data = self
            .call(Method::GET, &format!("/api/payouts/{id}"), &[], None)
            .await?;
        decode(unwrap_key(data, "payout"))
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
