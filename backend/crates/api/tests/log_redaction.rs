//! Runs alone in its own test binary: tracing caches which callsites are enabled, so a capture
//! installed while other tests log on other threads can miss events.

mod common;

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use common::WEB;
use mergetip_api::router;
use mergetip_core::{GithubUserId, SystemClock};
use mergetip_crypto::random_token;
use mergetip_payer::{LnurlClient, Network, nwc::NwcConnector, testing::MockWallet};
use mergetip_store::{
    sessions::{self, SessionKind},
    users::{self, GithubProfile},
};
use nostr_sdk::prelude::MockRelay;
use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

/// Captures every log line written while it is the default subscriber.
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

/// The NWC URI is a spending credential: it must never reach the logs, at any level.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_nwc_uri_never_reaches_the_logs(pool: PgPool) -> sqlx::Result<()> {
    let relay = MockRelay::run().await.unwrap();
    let wallet = MockWallet::start(&relay.url().await.to_string(), [3; 32])
        .await
        .unwrap();
    let secret = wallet
        .uri()
        .split("secret=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();

    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let clock = Arc::new(SystemClock);
    let receivers = LnurlClient::new(Network::Regtest, true, clock.clone()).unwrap();
    let state = common::state_with(
        pool.clone(),
        "http://127.0.0.1:9",
        clock,
        Arc::new(NwcConnector),
        Arc::new(receivers),
    );
    let app = router(state);
    let mut conn = pool.acquire().await?;
    let user = users::upsert_by_github_id(
        &mut conn,
        &GithubProfile {
            github_id: GithubUserId::new(42),
            login: "alice".into(),
            avatar_url: None,
            created_at: None,
        },
    )
    .await
    .unwrap();
    let token = random_token().unwrap();
    sessions::create(
        &pool,
        user.id,
        SessionKind::Web,
        &token.hash,
        OffsetDateTime::now_utc() + Duration::days(1),
        None,
    )
    .await
    .unwrap();
    let put = |uri: String| {
        Request::put("/v1/me/wallet")
            .header(header::COOKIE, format!("mt_session={}", token.raw.expose()))
            .header(header::ORIGIN, WEB)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "nwc_uri": uri }).to_string()))
            .unwrap()
    };

    let ok = app
        .clone()
        .oneshot(put(wallet.uri().to_owned()))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    // A failing connect is where a careless error message would echo the input.
    let bad_uri = wallet.uri().replace("relay=", "rel=");
    let bad = app.clone().oneshot(put(bad_uri)).await.unwrap();
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!output.is_empty(), "the capture works");
    assert!(!output.contains(&secret), "NWC secret found in logs");
    assert!(
        !output.contains("nostr+walletconnect://"),
        "NWC URI found in logs"
    );
    Ok(())
}
