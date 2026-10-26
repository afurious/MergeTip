//! `POST /v1/webhooks/github`: authenticate, store, enqueue, acknowledge.
//!
//! Nothing is processed inline. GitHub expects an answer within 10 seconds and redelivers on
//! failure, so the handler only does the minimum needed to never lose or double-process an
//! event: verify the signature over the raw bytes, store the delivery, and enqueue
//! `process_event` in the same transaction.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use mergetip_engine::jobs::{self, ProcessEvent};
use mergetip_store::github_events;

use crate::{ApiError, AppState};

const SIGNATURE: &str = "x-hub-signature-256";
const EVENT: &str = "x-github-event";
const DELIVERY: &str = "x-github-delivery";

/// Receive a GitHub App webhook delivery.
#[utoipa::path(
    post,
    path = "/v1/webhooks/github",
    operation_id = "receive_github_webhook",
    tag = "webhooks",
    params(
        ("X-Hub-Signature-256" = String, Header, description = "`sha256=` HMAC of the raw body"),
        ("X-GitHub-Event" = String, Header, description = "Event name, e.g. `issue_comment`"),
        ("X-GitHub-Delivery" = String, Header, description = "Unique delivery ID"),
    ),
    request_body(content = String, content_type = "application/json", description = "Raw event payload"),
    responses(
        (status = 202, description = "Stored for processing (or already stored)"),
        (status = 400, description = "A required header is missing or the body is not JSON", body = crate::error::ErrorBody),
        (status = 401, description = "Signature missing or wrong", body = crate::error::ErrorBody),
    )
)]
#[tracing::instrument(skip_all, fields(delivery_id, event))]
pub async fn github(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let signature = header(&headers, SIGNATURE).ok_or_else(invalid_signature)?;
    mergetip_crypto::verify_github_signature(
        state.config().github_webhook_secret.expose().as_bytes(),
        &body,
        signature,
    )
    .map_err(|_| invalid_signature())?;

    let event = header(&headers, EVENT).ok_or_else(|| missing_header(EVENT))?;
    let delivery_id = header(&headers, DELIVERY).ok_or_else(|| missing_header(DELIVERY))?;
    let span = tracing::Span::current();
    span.record("delivery_id", delivery_id);
    span.record("event", event);

    let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "The webhook body is not JSON.",
        )
    })?;
    let action = payload.get("action").and_then(serde_json::Value::as_str);

    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    let is_new =
        github_events::insert_if_new(&mut *tx, delivery_id, event, action, &payload).await?;
    if is_new {
        jobs::enqueue(
            &mut tx,
            &ProcessEvent {
                delivery_id: delivery_id.to_owned(),
            },
        )
        .await?;
    }
    tx.commit().await.map_err(ApiError::internal)?;

    tracing::info!(action, duplicate = !is_new, "webhook accepted");
    Ok(StatusCode::ACCEPTED)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

fn invalid_signature() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_signature",
        "The webhook signature is missing or does not match.",
    )
}

fn missing_header(name: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "missing_header",
        format!("The {name} header is required."),
    )
}

/// Resend delivery events this API records. Anything else is acknowledged and ignored.
fn email_status(event_type: &str) -> Option<&'static str> {
    match event_type {
        "email.sent" => Some("sent"),
        "email.delivered" => Some("delivered"),
        "email.opened" => Some("opened"),
        "email.bounced" => Some("bounced"),
        _ => None,
    }
}

/// Receive a Resend delivery event (Svix-signed) and update the email status of every tip the
/// message covered (FR-11.7).
#[utoipa::path(
    post,
    path = "/v1/webhooks/resend",
    operation_id = "receive_resend_webhook",
    tag = "webhooks",
    params(
        ("svix-id" = String, Header, description = "Unique message ID"),
        ("svix-timestamp" = String, Header, description = "Unix seconds when it was sent"),
        ("svix-signature" = String, Header, description = "Space-separated `v1,<base64>` signatures"),
    ),
    request_body(content = String, content_type = "application/json", description = "Raw event payload"),
    responses(
        (status = 200, description = "Recorded, or ignored as irrelevant"),
        (status = 401, description = "Signature missing, wrong or too old", body = crate::error::ErrorBody),
        (status = 404, description = "Resend webhooks aren't configured", body = crate::error::ErrorBody),
    )
)]
pub async fn resend(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let Some(secret) = state.config().resend_webhook_secret.as_ref() else {
        return Err(ApiError::not_found());
    };
    let (Some(id), Some(timestamp), Some(signatures)) = (
        header(&headers, "svix-id"),
        header(&headers, "svix-timestamp"),
        header(&headers, "svix-signature"),
    ) else {
        return Err(invalid_signature());
    };
    mergetip_crypto::verify_svix_signature(
        secret.expose(),
        id,
        timestamp,
        &body,
        signatures,
        state.clock().now().unix_timestamp(),
    )
    .map_err(|_| invalid_signature())?;

    #[derive(serde::Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: String,
        data: EventData,
    }
    #[derive(serde::Deserialize)]
    struct EventData {
        email_id: Option<String>,
    }
    let Ok(event) = serde_json::from_slice::<Event>(&body) else {
        return Ok(StatusCode::OK);
    };
    let (Some(status), Some(email_id)) = (email_status(&event.kind), event.data.email_id) else {
        return Ok(StatusCode::OK);
    };
    let tips = mergetip_store::email_messages::tips_for(state.pool(), &email_id).await?;
    let changed = mergetip_store::tips::advance_email_status(state.pool(), &tips, status).await?;
    tracing::info!(event = %event.kind, changed, "email delivery event");
    Ok(StatusCode::OK)
}

/// Receive a Bitnob payout event (RFC 0004): check the signature over the raw body, store it once
/// per `event_id`, and queue it. Processing happens in the worker.
#[utoipa::path(
    post,
    path = "/v1/webhooks/bitnob",
    operation_id = "receive_bitnob_webhook",
    tag = "webhooks",
    params(("x-bitnob-signature" = String, Header, description = "HMAC-SHA512 of the raw body")),
    request_body(content = String, content_type = "application/json", description = "Raw event payload"),
    responses(
        (status = 200, description = "Stored for processing (or already stored)"),
        (status = 400, description = "No event ID or not JSON", body = crate::error::ErrorBody),
        (status = 401, description = "Signature missing or wrong", body = crate::error::ErrorBody),
        (status = 404, description = "Bitnob webhooks aren't configured", body = crate::error::ErrorBody),
    )
)]
pub async fn bitnob(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let Some(secret) = state.config().bitnob_webhook_secret.as_ref() else {
        return Err(ApiError::not_found());
    };
    let signature = header(&headers, "x-bitnob-signature").ok_or_else(invalid_signature)?;
    if !mergetip_fiat::signing::verify_webhook(secret.expose(), &body, signature) {
        return Err(invalid_signature());
    }
    let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "The webhook body is not JSON.",
        )
    })?;
    let event_id = payload
        .get("event_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| header(&headers, "x-bitnob-event-id"))
        .filter(|id| !id.is_empty() && id.len() <= 200)
        .ok_or_else(|| missing_header("x-bitnob-event-id"))?
        .to_owned();

    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    let is_new =
        mergetip_store::bitnob_events::insert_if_new(&mut *tx, &event_id, &payload).await?;
    if is_new {
        jobs::enqueue(
            &mut tx,
            &mergetip_engine::jobs::ProcessBitnobEvent {
                event_id: event_id.clone(),
            },
        )
        .await?;
    }
    tx.commit().await.map_err(ApiError::internal)?;
    tracing::info!(%event_id, duplicate = !is_new, "Bitnob webhook accepted");
    Ok(StatusCode::OK)
}
