//! Unsubscribe links and Resend delivery tracking (task 4.6).

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use common::{MASTER_KEY, RESEND_SECRET, fake_app, send, session_for};
use mergetip_crypto::{Keyring, hmac_sha256};
use mergetip_store::users;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn unsubscribe_links_opt_the_user_out(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    session_for(&pool, 11, "reviewer").await;
    let user = users::get_by_login(&pool, "reviewer").await.unwrap();
    let keyring = Keyring::from_hex(MASTER_KEY, None).unwrap();
    let token = mergetip_notify::unsubscribe::token(&keyring, user.id.as_uuid());

    // A link clicked in a mail client.
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/v1/email/unsubscribe?token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(users::get(&pool, user.id).await.unwrap().email_opt_out);

    // RFC 8058 one-click: a POST from the mail client, no cookies, no Origin.
    sqlx::query!("UPDATE users SET email_opt_out = false")
        .execute(&pool)
        .await?;
    let response = app
        .clone()
        .oneshot(
            Request::post(format!("/v1/email/unsubscribe?token={token}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("List-Unsubscribe=One-Click"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(users::get(&pool, user.id).await.unwrap().email_opt_out);

    let forged = format!("{}.AAAA", user.id.as_uuid().simple());
    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/email/unsubscribe?token={forged}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_token");
    Ok(())
}

fn signed(body: &str, id: &str, at: i64) -> Request<Body> {
    let key = STANDARD
        .decode(RESEND_SECRET.trim_start_matches("whsec_"))
        .unwrap();
    let signature = STANDARD.encode(hmac_sha256(&key, format!("{id}.{at}.{body}").as_bytes()));
    Request::post("/v1/webhooks/resend")
        .header("content-type", "application/json")
        .header("svix-id", id)
        .header("svix-timestamp", at.to_string())
        .header("svix-signature", format!("v1,{signature}"))
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn status_of(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar!("SELECT email_status FROM tips")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn resend_events_move_email_status_forward_only(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let installation = mergetip_store::installations::upsert(
        &pool,
        mergetip_core::GithubInstallationId::new(77),
        "acme",
    )
    .await
    .unwrap();
    session_for(&pool, 11, "reviewer").await;
    let reviewer = users::get_by_login(&pool, "reviewer").await.unwrap();
    let mut conn = pool.acquire().await?;
    let tip = mergetip_store::tips::insert_idempotent(
        &mut conn,
        &mergetip_store::tips::NewTip {
            idempotency_key: "k".into(),
            installation_id: installation.id,
            sender_id: reviewer.id,
            receiver_login: "reviewer".into(),
            thread: mergetip_core::ThreadRef {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 42,
                is_pr: true,
            },
            comment_id: 1,
            command_index: 0,
            kind: mergetip_core::TipKind::Now,
            amount: mergetip_core::Sats::new(5_000),
            note: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let sent_at = time::OffsetDateTime::now_utc();
    mergetip_store::tips::mark_notified(&pool, &[tip.id], sent_at, "sent")
        .await
        .unwrap();
    mergetip_store::email_messages::record(&pool, "em_1", reviewer.id, &[tip.id], sent_at)
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let event = |kind: &str| json!({ "type": kind, "data": { "email_id": "em_1" } }).to_string();
    let deliver = |kind: &str, id: &str| app.clone().oneshot(signed(&event(kind), id, now));

    assert_eq!(
        deliver("email.delivered", "msg_1").await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(status_of(&pool).await.as_deref(), Some("delivered"));
    deliver("email.opened", "msg_2").await.unwrap();
    assert_eq!(status_of(&pool).await.as_deref(), Some("opened"));
    // A late "delivered" can't move it back.
    deliver("email.delivered", "msg_3").await.unwrap();
    assert_eq!(status_of(&pool).await.as_deref(), Some("opened"));
    deliver("email.bounced", "msg_4").await.unwrap();
    assert_eq!(status_of(&pool).await.as_deref(), Some("bounced"));
    deliver("email.opened", "msg_5").await.unwrap();
    assert_eq!(
        status_of(&pool).await.as_deref(),
        Some("bounced"),
        "bounces are final"
    );
    // Unknown messages and event types are acknowledged and ignored.
    let other = json!({ "type": "email.clicked", "data": { "email_id": "em_9" } }).to_string();
    let response = app
        .clone()
        .oneshot(signed(&other, "msg_6", now))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn unsigned_or_stale_resend_events_are_rejected(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let body = json!({ "type": "email.bounced", "data": { "email_id": "em_1" } }).to_string();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    let mut forged = signed(&body, "msg_1", now);
    forged
        .headers_mut()
        .insert("svix-signature", "v1,bm90IGEgc2lnbmF0dXJl".parse().unwrap());
    let response = app.clone().oneshot(forged).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let stale = signed(&body, "msg_1", now - 3600);
    let response = app.clone().oneshot(stale).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let mut unsigned = signed(&body, "msg_1", now);
    unsigned.headers_mut().remove("svix-signature");
    let response = app.clone().oneshot(unsigned).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}
