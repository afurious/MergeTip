use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use mergetip_api::{ApiConfig, AppState, router};
use mergetip_core::SystemClock;
use mergetip_store::PgPool;
use serde_json::{Value, json};
use tower::ServiceExt;

const WEB: &str = "http://localhost:5173";

fn app() -> Router {
    let env = HashMap::from([
        (
            "MASTER_KEY",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        ),
        ("DATABASE_URL", "postgres://unused/db"),
        ("PUBLIC_BASE_URL", WEB),
        ("GITHUB_WEBHOOK_SECRET", "test-secret"),
    ]);
    let config = Arc::new(ApiConfig::from_env(&env).unwrap());
    // These routes never touch the database; a lazy pool never connects.
    let pool = PgPool::connect_lazy("postgres://unused/db").unwrap();
    router(AppState::new(pool, config, Arc::new(SystemClock)))
}

async fn send(request: Request<Body>) -> Response {
    app().oneshot(request).await.unwrap()
}

async fn json_body(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn health_reports_status_and_version() {
    let response = send(get("/v1/health")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") })
    );
}

#[tokio::test]
async fn healthz_stays_plain_text_for_fly() {
    let response = send(get("/healthz")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64).await.unwrap();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn unknown_route_uses_the_error_shape() {
    let response = send(get("/v1/nope")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn wrong_method_uses_the_error_shape() {
    let response = send(Request::delete("/v1/health").body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "method_not_allowed"
    );
}

#[tokio::test]
async fn cors_preflight_allows_the_web_app_with_credentials() {
    let response = send(
        Request::options("/v1/health")
            .header(header::ORIGIN, WEB)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PATCH")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let headers = response.headers();
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], WEB);
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_CREDENTIALS], "true");
    assert!(
        headers[header::ACCESS_CONTROL_ALLOW_METHODS]
            .to_str()
            .unwrap()
            .contains("PATCH")
    );
}

/// With a single allowed origin the header is constant; a browser on any other origin sees a
/// mismatch and blocks the response.
#[tokio::test]
async fn cors_never_allows_other_origins() {
    let response = send(
        Request::get("/v1/health")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let allowed = response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN);
    assert!(allowed.is_none_or(|origin| origin == WEB), "{allowed:?}");
}

#[tokio::test]
async fn request_id_is_generated_and_propagated() {
    let generated = send(get("/v1/health")).await;
    let id = generated.headers()["x-request-id"].to_str().unwrap();
    assert_eq!(id.len(), 36, "uuid request id, got {id}");

    let echoed = send(
        Request::get("/v1/health")
            .header("x-request-id", "trace-me")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(echoed.headers()["x-request-id"], "trace-me");
}

/// Rejected before routing, yet still readable by the web app: CORS headers and the JSON
/// error shape are present.
#[tokio::test]
async fn oversized_bodies_are_rejected_in_the_error_shape() {
    let response = send(
        Request::post("/v1/health")
            .header(header::ORIGIN, WEB)
            .header(header::CONTENT_LENGTH, 6 * 1024 * 1024)
            .body(Body::from(vec![b'x'; 6 * 1024 * 1024]))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], WEB);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "payload_too_large"
    );
}
