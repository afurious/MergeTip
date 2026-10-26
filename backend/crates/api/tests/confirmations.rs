//! Confirming high-value tips in the web app (task 4.4).

mod common;

use std::sync::Arc;

use axum::{Router, http::StatusCode};
use common::{FakeReceivers, MASTER_KEY, send, session_for, state_with_mailer};
use mergetip_api::router;
use mergetip_core::{
    Clock, FixedClock, GithubInstallationId, GithubUserId, Sats, ThreadRef, TipId, TipKind,
    TipState,
};
use mergetip_crypto::Keyring;
use mergetip_notify::testing::RecordingMailer;
use mergetip_payer::nwc::NwcConnector;
use mergetip_store::{
    installations,
    tips::{self, NewTip, TipPatch},
    users::{self, EmailSource, GithubProfile},
};
use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

struct World {
    app: Router,
    pool: PgPool,
    clock: Arc<FixedClock>,
    mailer: Arc<RecordingMailer>,
    sender: String,
}

impl World {
    async fn new(pool: PgPool) -> Self {
        let clock = Arc::new(FixedClock::new(OffsetDateTime::now_utc()));
        let mailer = Arc::new(RecordingMailer::new());
        let app = router(state_with_mailer(
            pool.clone(),
            "http://127.0.0.1:9",
            clock.clone(),
            Arc::new(NwcConnector),
            Arc::new(FakeReceivers),
            mailer.clone(),
        ));
        let sender = session_for(&pool, 10, "maintainer").await;
        Self {
            app,
            pool,
            clock,
            mailer,
            sender,
        }
    }

    /// An 80,000-sat tip from the maintainer to the reviewer, awaiting confirmation for an hour.
    async fn tip(&self, kind: TipKind) -> TipId {
        let installation = installations::upsert(&self.pool, GithubInstallationId::new(77), "acme")
            .await
            .unwrap();
        let mut conn = self.pool.acquire().await.unwrap();
        let sender = users::get_by_login(&self.pool, "maintainer").await.unwrap();
        let receiver = users::upsert_by_github_id(
            &mut conn,
            &GithubProfile {
                github_id: GithubUserId::new(11),
                login: "reviewer".into(),
                avatar_url: None,
                created_at: None,
            },
        )
        .await
        .unwrap();
        let tip = tips::insert_idempotent(
            &mut conn,
            &NewTip {
                idempotency_key: format!("k-{}", uuid::Uuid::new_v4()),
                installation_id: installation.id,
                sender_id: sender.id,
                receiver_login: "reviewer".into(),
                thread: ThreadRef {
                    owner: "acme".into(),
                    repo: "widgets".into(),
                    number: 42,
                    is_pr: true,
                },
                comment_id: 1001,
                command_index: 0,
                kind,
                amount: Sats::new(80_000),
                note: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        tips::transition(
            &mut conn,
            tip.id,
            TipState::Received,
            TipState::Verifying,
            &TipPatch::default(),
            json!({}),
        )
        .await
        .unwrap();
        tips::transition(
            &mut conn,
            tip.id,
            TipState::Verifying,
            TipState::AwaitingConfirmation,
            &TipPatch {
                receiver_id: Some(receiver.id),
                confirm_expires_at: Some(self.clock.now() + Duration::hours(1)),
                ..TipPatch::default()
            },
            json!({}),
        )
        .await
        .unwrap();
        tip.id
    }

    /// Turns codes on and gives the sender an email to receive them.
    async fn enable_otp(&self) {
        let sender = users::get_by_login(&self.pool, "maintainer").await.unwrap();
        let sealed = Keyring::from_hex(MASTER_KEY, None)
            .unwrap()
            .seal(
                b"maintainer@example.com",
                format!("users.email:{}", sender.id).as_bytes(),
            )
            .unwrap();
        users::set_email(&self.pool, sender.id, &sealed, EmailSource::Verified)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE users SET confirm_otp_enabled = true WHERE id = $1",
            sender.id.as_uuid()
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn post(
        &self,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        send(&self.app, "POST", uri, Some(&self.sender), body).await
    }

    async fn state(&self, tip: TipId) -> String {
        sqlx::query_scalar!("SELECT state FROM tips WHERE id = $1", tip.as_uuid())
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn queued(&self, kind: &str) -> i64 {
        sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM jobs WHERE kind = $1"#, kind)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// The code from the last email sent.
    fn last_code(&self) -> String {
        let sent = self.mailer.sent();
        let text = &sent.last().expect("an email").text;
        text.split(|c: char| !c.is_ascii_digit())
            .find(|word| word.len() == 6)
            .expect("a 6-digit code")
            .to_owned()
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_sender_confirms_and_the_tip_is_paid(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = w.tip(TipKind::Now).await;

    let (status, body) = send(
        &w.app,
        "GET",
        &format!("/v1/confirmations/{tip}"),
        Some(&w.sender),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tip"]["state"], "awaiting_confirmation");
    assert_eq!(body["tip"]["amount_sats"], 80_000);
    assert!(body["tip"]["expires_at"].is_string());

    let (status, body) = w.post(&format!("/v1/confirmations/{tip}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tip"]["state"], "paying");
    assert_eq!(w.queued("pay_tip").await, 1);
    let confirmed =
        sqlx::query_scalar!("SELECT confirmed_at FROM tips WHERE id = $1", tip.as_uuid())
            .fetch_one(&w.pool)
            .await?;
    assert!(confirmed.is_some());

    // Confirming twice does nothing more.
    let (status, body) = w.post(&format!("/v1/confirmations/{tip}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "not_awaiting_confirmation");
    assert_eq!(w.queued("pay_tip").await, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_confirmed_on_merge_tip_waits_for_the_merge(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = w.tip(TipKind::OnMerge).await;
    let (status, body) = w.post(&format!("/v1/confirmations/{tip}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tip"]["state"], "held");
    assert_eq!(w.queued("pay_tip").await, 0);
    // Checked against GitHub at once, in case the PR already merged.
    assert_eq!(w.queued("resolve_held").await, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn only_the_senders_web_session_can_confirm(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = w.tip(TipKind::Now).await;
    let uri = format!("/v1/confirmations/{tip}");

    let receiver = session_for(&w.pool, 11, "reviewer").await;
    let (status, body) = send(&w.app, "POST", &uri, Some(&receiver), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "not_sender");

    // The extension's bearer token can't confirm: the web session is required.
    let token = mergetip_crypto::random_token().unwrap();
    let sender = users::get_by_login(&w.pool, "maintainer").await.unwrap();
    mergetip_store::sessions::create(
        &w.pool,
        sender.id,
        mergetip_store::sessions::SessionKind::Extension,
        &token.hash,
        OffsetDateTime::now_utc() + Duration::days(1),
        None,
    )
    .await
    .unwrap();
    let request = axum::http::Request::post(&uri)
        .header("authorization", format!("Bearer {}", token.raw.expose()))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(w.app.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(w.state(tip).await, "awaiting_confirmation");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_hour_later_it_is_too_late(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = w.tip(TipKind::Now).await;
    w.clock.advance(Duration::hours(1));
    let (status, body) = w.post(&format!("/v1/confirmations/{tip}"), None).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body["error"]["code"], "confirmation_expired");
    assert_eq!(w.queued("pay_tip").await, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn with_codes_on_the_emailed_code_is_required(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.enable_otp().await;
    let tip = w.tip(TipKind::Now).await;
    let confirm = format!("/v1/confirmations/{tip}");
    let otp = format!("/v1/confirmations/{tip}/otp");

    let (status, body) = w.post(&confirm, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "otp_invalid");

    let (status, _) = w.post(&otp, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    {
        let sent = w.mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "maintainer@example.com");
        assert_eq!(sent[0].tag, "otp");
    }
    let code = w.last_code();
    let stored = sqlx::query_scalar!("SELECT code_hmac FROM otp_codes")
        .fetch_one(&w.pool)
        .await?;
    assert!(!stored.contains(&code), "only a MAC of the code is stored");

    // One code a minute.
    let (status, body) = w.post(&otp, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "otp_rate_limited");

    let wrong = if code == "000000" { "111111" } else { "000000" };
    let (status, _) = w.post(&confirm, Some(json!({ "otp": wrong }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = w.post(&confirm, Some(json!({ "otp": code }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tip"]["state"], "paying");
    let left = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM otp_codes"#)
        .fetch_one(&w.pool)
        .await?;
    assert_eq!(left, 0, "a used code is gone");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn five_wrong_codes_lock_the_code(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    w.enable_otp().await;
    let tip = w.tip(TipKind::Now).await;
    let confirm = format!("/v1/confirmations/{tip}");
    w.post(&format!("/v1/confirmations/{tip}/otp"), None).await;
    let code = w.last_code();
    let wrong = if code == "000000" { "111111" } else { "000000" };

    for _ in 0..5 {
        let (status, body) = w.post(&confirm, Some(json!({ "otp": wrong }))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "otp_invalid");
    }
    // The right code no longer works: it was thrown away.
    let (status, _) = w.post(&confirm, Some(json!({ "otp": code }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(w.state(tip).await, "awaiting_confirmation");

    // A fresh code (after the resend interval) works.
    w.clock.advance(Duration::seconds(61));
    let (status, _) = w.post(&format!("/v1/confirmations/{tip}/otp"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = w
        .post(&confirm, Some(json!({ "otp": w.last_code() })))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.state(tip).await, "paying");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_sender_can_cancel_instead(pool: PgPool) -> sqlx::Result<()> {
    let w = World::new(pool).await;
    let tip = w.tip(TipKind::Now).await;
    let uri = format!("/v1/confirmations/{tip}");

    let (status, _) = send(&w.app, "DELETE", &uri, Some(&w.sender), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(w.state(tip).await, "cancelled");
    let code = sqlx::query_scalar!("SELECT error_code FROM tips WHERE id = $1", tip.as_uuid())
        .fetch_one(&w.pool)
        .await?;
    assert_eq!(code.as_deref(), Some("cancelled_by_sender"));
    assert_eq!(
        w.queued("post_reply").await,
        1,
        "a status line in the thread"
    );

    let (status, _) = send(&w.app, "DELETE", &uri, Some(&w.sender), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = w.post(&uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    Ok(())
}
