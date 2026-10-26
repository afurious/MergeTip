//! Route table and middleware stack.

pub(crate) mod auth;
pub(crate) mod claims;
pub(crate) mod compose;
pub(crate) mod confirmations;
pub(crate) mod email;
pub(crate) mod health;
pub(crate) mod me;
pub(crate) mod settings;
pub(crate) mod tips;
pub(crate) mod users;
pub(crate) mod webhooks;

use std::time::Duration;

use axum::{
    Router,
    body::to_bytes,
    http::{HeaderName, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
};
use tower_http::{
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    sensitive_headers::SetSensitiveHeadersLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{ApiError, AppState};

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
/// GitHub webhook payloads can be several megabytes; nothing else comes close.
const BODY_LIMIT_BYTES: usize = 5 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(state.config().web_origin.clone())
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .max_age(Duration::from_secs(3600));

    let v1 = Router::new()
        .route("/health", get(health::health))
        .route("/auth/github/login", get(auth::login))
        .route("/auth/github/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/extension-token", post(auth::extension_token))
        .route("/me", get(me::get_me).delete(me::delete_me))
        .route("/me/tips", get(tips::list_my_tips))
        .route("/users/{login}/receive-status", get(users::receive_status))
        .route("/tips/comment", post(compose::post_tip_comment))
        .route(
            "/me/wallet",
            put(settings::connect_wallet).delete(settings::disconnect_wallet),
        )
        .route(
            "/me/lightning-address",
            put(settings::set_lightning_address),
        )
        .route("/me/settings", patch(settings::update_settings))
        .route(
            "/claims/{token}",
            get(claims::get_claim).post(claims::claim_tip),
        )
        .route(
            "/confirmations/{tip_id}",
            get(confirmations::get_confirmation)
                .post(confirmations::confirm_tip)
                .delete(confirmations::cancel_tip),
        )
        .route(
            "/confirmations/{tip_id}/otp",
            post(confirmations::send_code),
        )
        .route(
            "/email/unsubscribe",
            get(email::unsubscribe_link).post(email::unsubscribe_one_click),
        )
        .route("/webhooks/github", post(webhooks::github))
        .route("/webhooks/resend", post(webhooks::resend));

    // Layers run outermost-last. CORS wraps every layer that can answer on its own (body limit,
    // timeout) and the error normaliser, so even those responses reach the browser readable and
    // in the JSON error shape. The request ID is set before tracing sees the request, and
    // credentials are marked sensitive so they never reach the trace output.
    Router::new()
        .route("/healthz", get(health::healthz))
        .nest("/v1", v1)
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "This endpoint does not support that method.",
            )
        })
        .layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            REQUEST_TIMEOUT,
        ))
        .layer(middleware::map_response(json_error_body))
        .layer(cors)
        .layer(PropagateRequestIdLayer::new(REQUEST_ID))
        .layer(TraceLayer::new_for_http())
        .layer(SetSensitiveHeadersLayer::new([
            header::AUTHORIZATION,
            header::COOKIE,
            header::SET_COOKIE,
        ]))
        .layer(SetRequestIdLayer::new(REQUEST_ID, MakeRequestUuid))
        .with_state(state)
}

/// Largest plain-text error body worth carrying over into `message`.
const MAX_ERROR_TEXT: usize = 1024;

/// Rewrites error responses that did not come from [`ApiError`] (the body-limit 413, the
/// timeout 503, axum's plain-text extractor rejections) into the JSON error contract. Headers
/// such as `Retry-After` and `Allow` are kept. A 4xx body becomes the message; a 5xx body is
/// never shown, because it may describe internals.
async fn json_error_body(response: Response) -> Response {
    let status = response.status();
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|ct| ct.as_bytes().starts_with(b"application/json"));
    if is_json || !(status.is_client_error() || status.is_server_error()) {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let reason = status.canonical_reason().unwrap_or("Error");
    let message = if status.is_client_error() {
        to_bytes(body, MAX_ERROR_TEXT)
            .await
            .ok()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| reason.to_owned())
    } else {
        reason.to_owned()
    };
    let code = reason.to_lowercase().replace([' ', '-'], "_");

    let mut normalized = ApiError::new(status, code, message).into_response();
    parts.headers.remove(header::CONTENT_TYPE);
    parts.headers.remove(header::CONTENT_LENGTH);
    for (name, value) in parts.headers.drain().filter_map(|(n, v)| n.map(|n| (n, v))) {
        normalized.headers_mut().insert(name, value);
    }
    normalized
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use serde_json::{Value, json};

    use super::*;

    async fn mapped(status: StatusCode, body: &'static str) -> (StatusCode, Value, Response) {
        let original = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::RETRY_AFTER, "5")
            .body(Body::from(body))
            .unwrap();
        let response = json_error_body(original).await;
        let status = response.status();
        let (parts, body) = response.into_parts();
        let json = serde_json::from_slice(&to_bytes(body, 4096).await.unwrap()).unwrap();
        (status, json, Response::from_parts(parts, Body::empty()))
    }

    #[tokio::test]
    async fn timeout_becomes_a_json_error_without_internal_text() {
        let (status, json, response) =
            mapped(StatusCode::SERVICE_UNAVAILABLE, "inner service detail").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json,
            json!({ "error": { "code": "service_unavailable", "message": "Service Unavailable" } })
        );
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }

    #[tokio::test]
    async fn client_error_text_becomes_the_message() {
        let (status, json, _) = mapped(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Failed to deserialize the JSON body: missing field `address`",
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json["error"]["code"], "unprocessable_entity");
        assert_eq!(
            json["error"]["message"],
            "Failed to deserialize the JSON body: missing field `address`"
        );
    }

    #[tokio::test]
    async fn json_and_success_responses_pass_through() {
        let ok = json_error_body(StatusCode::NO_CONTENT.into_response()).await;
        assert_eq!(ok.status(), StatusCode::NO_CONTENT);
        let api = json_error_body(ApiError::not_found().into_response()).await;
        let body = to_bytes(api.into_body(), 4096).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "not_found");
    }
}
