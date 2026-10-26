//! Mock LNURL-pay server: every `user@<this host>` is a Lightning address. Invoices are real,
//! signed regtest invoices that `mock-nwc` (sharing `MOCK_SEED`) can pay.
//!
//! Magic users, for exercising MergeTip's invoice checks:
//! - `wrongamount`: the invoice is for 1 sat more than asked.
//! - `expired`: the invoice expires in 30 seconds.
//! - `badhash`: the invoice commits to different metadata.
//! - `down`: every request fails with 500.
//!
//! Environment: `MOCK_LNURL_ADDR` (listen address), `MOCK_SEED` (32-byte hex).

use std::{sync::Arc, time::Duration};

use anyhow::{Context, bail};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use mergetip_core::Msats;
use mergetip_payer::{
    Network,
    testing::{MintParams, mint_invoice},
};
use serde::Deserialize;
use serde_json::json;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct Mock {
    seed: [u8; 32],
}

fn router(mock: Mock) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/.well-known/lnurlp/{user}", get(pay_request))
        .route("/lnurlp/{user}/callback", get(callback))
        .with_state(Arc::new(mock))
}

fn metadata(user: &str, host: &str) -> String {
    json!([
        ["text/plain", format!("Sats for {user}")],
        ["text/identifier", format!("{user}@{host}")]
    ])
    .to_string()
}

fn host(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8090")
        .to_owned()
}

async fn pay_request(Path(user): Path<String>, headers: HeaderMap) -> Response {
    if user == "down" {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let host = host(&headers);
    Json(json!({
        "tag": "payRequest",
        "callback": format!("http://{host}/lnurlp/{user}/callback"),
        "minSendable": 1_000,
        "maxSendable": 100_000_000_000u64,
        "metadata": metadata(&user, &host),
        "commentAllowed": 140
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    amount: u64,
}

async fn callback(
    State(mock): State<Arc<Mock>>,
    Path(user): Path<String>,
    Query(query): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Response {
    let host = host(&headers);
    let mut params = MintParams {
        network: Network::Regtest,
        amount: Msats::new(query.amount),
        metadata: metadata(&user, &host),
        seed: mock.seed,
        ..MintParams::default()
    };
    match user.as_str() {
        "down" => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        "wrongamount" => params.amount = Msats::new(query.amount + 1_000),
        "expired" => params.expiry = Duration::from_secs(30),
        "badhash" => params.metadata = "[]".into(),
        _ => {}
    }
    let minted = mint_invoice(&params);
    tracing::info!(
        user,
        msat = query.amount,
        hash = minted.payment_hash,
        "invoice minted"
    );
    Json(json!({ "pr": minted.bolt11, "routes": [] })).into_response()
}

fn seed() -> anyhow::Result<[u8; 32]> {
    let raw = std::env::var("MOCK_SEED").context("MOCK_SEED is required")?;
    let bytes = hex::decode(raw.trim()).context("MOCK_SEED is not hex")?;
    match <[u8; 32]>::try_from(bytes.as_slice()) {
        Ok(seed) => Ok(seed),
        Err(_) => bail!("MOCK_SEED must be 32 bytes of hex"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let mock = Mock { seed: seed()? };
    let addr = std::env::var("MOCK_LNURL_ADDR").unwrap_or_else(|_| "0.0.0.0:8090".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "mock-lnurl listening; any user@<host> is a Lightning address");
    axum::serve(listener, router(mock)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use mergetip_payer::{InvoiceError, invoice};
    use time::OffsetDateTime;
    use tower::ServiceExt;

    use super::*;

    const SEED: [u8; 32] = [1; 32];

    async fn get(path: &str) -> (StatusCode, serde_json::Value) {
        let request = axum::http::Request::get(path)
            .header(header::HOST, "localhost:8090")
            .body(Body::empty())
            .unwrap();
        let response = router(Mock { seed: SEED }).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn invoice_for(user: &str) -> Result<mergetip_payer::ValidatedInvoice, InvoiceError> {
        let (_, params) = get(&format!("/.well-known/lnurlp/{user}")).await;
        let (_, body) = get(&format!("/lnurlp/{user}/callback?amount=21000")).await;
        invoice::validate(
            body["pr"].as_str().unwrap(),
            Msats::new(21_000),
            params["metadata"].as_str().unwrap(),
            Network::Regtest,
            OffsetDateTime::now_utc(),
        )
    }

    #[tokio::test]
    async fn serves_lud16_with_a_working_callback() {
        let (status, params) = get("/.well-known/lnurlp/alice").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(params["tag"], "payRequest");
        assert_eq!(
            params["callback"],
            "http://localhost:8090/lnurlp/alice/callback"
        );
        assert!(
            invoice_for("alice").await.is_ok(),
            "MergeTip accepts its invoices"
        );
    }

    #[tokio::test]
    async fn magic_users_trip_each_invoice_check() {
        assert!(matches!(
            invoice_for("wrongamount").await,
            Err(InvoiceError::WrongAmount { .. })
        ));
        assert_eq!(
            invoice_for("expired").await,
            Err(InvoiceError::ExpiresTooSoon)
        );
        assert_eq!(
            invoice_for("badhash").await,
            Err(InvoiceError::DescriptionHashMismatch)
        );
        assert_eq!(
            get("/.well-known/lnurlp/down").await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
