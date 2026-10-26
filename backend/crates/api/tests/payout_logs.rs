//! Runs alone in its own test binary (see `log_redaction.rs`): adding a payout method never logs
//! the beneficiary's account number or name.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::{fake_fiat, fiat_app, send, session_for};
use serde_json::json;
use sqlx::PgPool;

#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn beneficiary_details_never_reach_the_logs(pool: PgPool) -> sqlx::Result<()> {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = fiat_app(pool.clone(), fake_fiat()).await;
    let session = session_for(&pool, 11, "reviewer").await;
    for account in ["0123456789", "1111111111"] {
        send(
            &app,
            "POST",
            "/v1/me/payout-methods",
            Some(&session),
            Some(json!({
                "country": "NG", "destination_type": "bank", "is_default": true,
                "fields": { "account_number": account, "bank_code": "058" }
            })),
        )
        .await;
    }
    let (status, _) = send(&app, "GET", "/v1/me/payout-methods", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK);

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("payout method added"),
        "the capture works: {logs}"
    );
    for secret in ["0123456789", "1111111111", "Ada Obi"] {
        assert!(!logs.contains(secret), "{secret} leaked into the logs");
    }
    Ok(())
}
