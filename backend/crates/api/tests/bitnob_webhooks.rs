//! `POST /v1/webhooks/bitnob` (task 5.5): signed, deduplicated, queued.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{BITNOB_SECRET, fake_app};
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

fn delivery(body: &str, signature: &str) -> Request<Body> {
    Request::post("/v1/webhooks/bitnob")
        .header("content-type", "application/json")
        .header("x-bitnob-signature", signature)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn sign(body: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha512>::new_from_slice(BITNOB_SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

async fn count(pool: &PgPool, sql_table: &str) -> i64 {
    match sql_table {
        "events" => sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM bitnob_events"#)
            .fetch_one(pool)
            .await
            .unwrap(),
        _ => sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM jobs WHERE kind = 'process_bitnob_event'"#
        )
        .fetch_one(pool)
        .await
        .unwrap(),
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn signed_events_are_stored_once_and_queued(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let body = json!({
        "event": "payouts.withdrawal.success", "event_id": "ev_1",
        "data": { "id": "po_1", "reference": "r", "status": "success" }
    })
    .to_string();
    for _ in 0..3 {
        let response = app
            .clone()
            .oneshot(delivery(&body, &sign(&body)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(count(&pool, "events").await, 1);
    assert_eq!(count(&pool, "jobs").await, 1, "retries queue nothing new");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn forged_or_unidentified_events_are_refused(pool: PgPool) -> sqlx::Result<()> {
    let app = fake_app(pool.clone());
    let body = json!({ "event": "payouts.withdrawal.success", "event_id": "ev_1",
                       "data": { "id": "po_1" } })
    .to_string();
    let forged = app
        .clone()
        .oneshot(delivery(&body, &"ab".repeat(64)))
        .await
        .unwrap();
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    let tampered = app
        .clone()
        .oneshot(delivery(&body.replace("po_1", "po_2"), &sign(&body)))
        .await
        .unwrap();
    assert_eq!(tampered.status(), StatusCode::UNAUTHORIZED);
    let no_id = json!({ "event": "payouts.processing", "data": { "id": "po_1" } }).to_string();
    let response = app
        .clone()
        .oneshot(delivery(&no_id, &sign(&no_id)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(count(&pool, "events").await, 0);
    Ok(())
}
