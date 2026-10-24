use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use mergetip_api::{ApiConfig, AppState, router};
use mergetip_core::SystemClock;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const SECRET: &str = "test-secret";

fn app(pool: PgPool) -> Router {
    let env = HashMap::from([
        (
            "MASTER_KEY",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        ),
        ("DATABASE_URL", "postgres://unused/db"),
        ("PUBLIC_BASE_URL", "http://localhost:5173"),
        ("GITHUB_WEBHOOK_SECRET", SECRET),
    ]);
    let config = Arc::new(ApiConfig::from_env(&env).unwrap());
    router(AppState::new(pool, config, Arc::new(SystemClock)))
}

fn sign(body: &[u8]) -> String {
    format!(
        "sha256={}",
        hex::encode(mergetip_crypto::hmac_sha256(SECRET.as_bytes(), body))
    )
}

fn delivery(id: &str, body: &[u8], signature: Option<String>) -> Request<Body> {
    let mut request = Request::post("/v1/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "issue_comment")
        .header("x-github-delivery", id);
    if let Some(signature) = signature {
        request = request.header("x-hub-signature-256", signature);
    }
    request.body(Body::from(body.to_vec())).unwrap()
}

async fn send(pool: &PgPool, request: Request<Body>) -> Response {
    app(pool.clone()).oneshot(request).await.unwrap()
}

async fn error_code(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1 << 16).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["error"]["code"].clone()
}

async fn counts(pool: &PgPool) -> (i64, i64) {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM github_events) AS "events!",
                  (SELECT count(*) FROM jobs WHERE kind = 'process_event') AS "jobs!""#
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (row.events, row.jobs)
}

fn payload() -> Vec<u8> {
    serde_json::to_vec(&json!({ "action": "created", "comment": { "id": 1, "body": "/tip @a 1" } }))
        .unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn valid_delivery_is_stored_and_enqueued(pool: PgPool) -> sqlx::Result<()> {
    let body = payload();
    let response = send(&pool, delivery("d-1", &body, Some(sign(&body)))).await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(counts(&pool).await, (1, 1));
    let stored = sqlx::query!("SELECT event, action, payload FROM github_events")
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored.event, "issue_comment");
    assert_eq!(stored.action.as_deref(), Some("created"));
    assert_eq!(stored.payload["comment"]["id"], 1);
    let job = sqlx::query_scalar!("SELECT payload FROM jobs")
        .fetch_one(&pool)
        .await?;
    assert_eq!(job, json!({ "delivery_id": "d-1" }));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn redelivery_is_acknowledged_once(pool: PgPool) -> sqlx::Result<()> {
    let body = payload();
    for _ in 0..3 {
        let response = send(&pool, delivery("d-1", &body, Some(sign(&body)))).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
    assert_eq!(counts(&pool).await, (1, 1));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn bad_or_missing_signatures_are_rejected(pool: PgPool) -> sqlx::Result<()> {
    let body = payload();
    let tampered = br#"{"action":"created","comment":{"id":2}}"#;
    for request in [
        delivery("d-1", &body, None),
        delivery("d-2", &body, Some("sha256=00".into())),
        delivery("d-3", tampered, Some(sign(&body))),
        delivery("d-4", &body, Some(sign(&body).replace("sha256=", "sha1="))),
    ] {
        let response = send(&pool, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(error_code(response).await, "invalid_signature");
    }
    assert_eq!(counts(&pool).await, (0, 0));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn missing_headers_and_non_json_are_bad_requests(pool: PgPool) -> sqlx::Result<()> {
    let body = payload();
    let no_delivery = Request::post("/v1/webhooks/github")
        .header("x-github-event", "issue_comment")
        .header("x-hub-signature-256", sign(&body))
        .body(Body::from(body.clone()))
        .unwrap();
    let response = send(&pool, no_delivery).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_code(response).await, "missing_header");

    let not_json = b"hello";
    let response = send(&pool, delivery("d-9", not_json, Some(sign(not_json)))).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_code(response).await, "invalid_payload");
    assert_eq!(counts(&pool).await, (0, 0));
    Ok(())
}
