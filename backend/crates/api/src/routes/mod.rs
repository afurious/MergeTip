//! Route table and middleware stack.

pub(crate) mod health;

use std::time::Duration;

use axum::{
    Router,
    http::{HeaderName, Method, StatusCode, header},
    routing::get,
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

    let v1 = Router::new().route("/health", get(health::health));

    // Layers run outermost-last: the request ID is set before tracing sees the request, and
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
        .layer(cors)
        .layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            REQUEST_TIMEOUT,
        ))
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
