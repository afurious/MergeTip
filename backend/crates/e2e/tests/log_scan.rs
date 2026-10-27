//! Critical test 10: the logs never carry a secret. Runs the journeys that touch every kind of
//! secret (wallet connection, sessions, extension tokens, GitHub tokens, emails, confirmation
//! codes) with every log line captured at `debug`, then scans the capture.
//!
//! Alone in its own test binary: it installs the process-wide subscriber.

mod common;

use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{System, WorkerOptions, claim_link};
use serde_json::json;
use sqlx::PgPool;
use tracing_subscriber::{EnvFilter, fmt::MakeWriter};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn no_secret_ever_reaches_the_logs(pool: PgPool) -> sqlx::Result<()> {
    let captured = Captured::default();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("debug"))
        .with_writer(captured.clone())
        .init();

    let system = System::start(pool).await;
    let worker = system.worker(WorkerOptions::default());
    let mut secrets: Vec<String> = Vec::new();
    let cookie_value = |cookie: &str| cookie.trim_start_matches("mt_session=").to_owned();

    // Journey A: sign in, connect a wallet (the NWC URI travels through the api), get paid.
    system.link_address("reviewer").await;
    let maintainer = system.connect_wallet().await;
    secrets.push(system.wallet.uri().to_owned());
    secrets.push(
        system
            .wallet
            .uri()
            .split("secret=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_owned(),
    );
    secrets.push(cookie_value(&maintainer));
    system
        .deliver_comment("d1", 2001, "/tip @reviewer 2100 for the review")
        .await;
    system.wait_for_idle_queue().await;

    // An extension token, used as a bearer token.
    let (status, minted) = system
        .write("POST", "/v1/auth/extension-token", &maintainer, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let bearer = minted["token"].as_str().unwrap().to_owned();
    let (status, _) = system
        .call(
            Request::get("/v1/me")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    secrets.push(bearer);

    // A high-value tip confirmed with an emailed code.
    let (status, body) = system
        .write(
            "PATCH",
            "/v1/me/settings",
            &maintainer,
            json!({ "per_tip_max_sats": 100_000, "daily_max_sats": 200_000,
                    "confirm_otp_enabled": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    system
        .deliver_comment("d2", 2002, "/tip @reviewer 60000")
        .await;
    system.wait_for_idle_queue().await;
    let tip_id = sqlx::query_scalar!("SELECT id FROM tips WHERE state = 'awaiting_confirmation'")
        .fetch_one(&system.pool)
        .await?;
    let confirm = format!("/v1/confirmations/{tip_id}");
    let (status, _) = system
        .write("POST", &format!("{confirm}/otp"), &maintainer, json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let email = system.api_mail.sent().last().cloned().unwrap();
    let code = email
        .text
        .split(|c: char| !c.is_ascii_digit())
        .find(|w| w.len() == 6)
        .unwrap()
        .to_owned();
    let (status, body) = system
        .write("POST", &confirm, &maintainer, json!({ "otp": code }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    system.wait_for_idle_queue().await;
    secrets.push(email.to.clone());

    // A pledge claimed by a newcomer who signs in.
    system
        .deliver_comment("d3", 2003, "/tip @author 1000")
        .await;
    system.wait_for_idle_queue().await;
    let token = claim_link(&system.comments().await);
    let author = system.sign_in("author-code").await;
    secrets.push(cookie_value(&author));
    let (status, body) = system
        .write(
            "POST",
            &format!("/v1/claims/{token}"),
            &author,
            json!({ "lightning_address": format!("author@{}", system.lnurl.address()) }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    system.wait_for_idle_queue().await;

    // GitHub user tokens exchanged at sign-in.
    secrets.extend(["ghu_maintainer", "ghu_reviewer", "ghu_author"].map(String::from));
    secrets.extend(["ghr_maintainer", "ghr_author"].map(String::from));

    let paid = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM tips WHERE state = 'paid'"#)
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(paid, 3, "every journey finished");
    worker.stop().await;

    let logs = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    assert!(logs.contains("tip paid"), "the capture works");
    for pattern in ["nostr+walletconnect://", "secret="] {
        assert!(!logs.contains(pattern), "{pattern:?} appears in the logs");
    }
    for secret in &secrets {
        assert!(
            !logs.contains(secret.as_str()),
            "a secret leaked: {secret:.6}…"
        );
    }
    // Six digits also turn up inside timestamps and durations: look for the code as a number
    // of its own.
    let otp_logged = logs
        .split(|c: char| !c.is_ascii_digit())
        .any(|number| number == code);
    assert!(!otp_logged, "the confirmation code leaked");
    Ok(())
}
