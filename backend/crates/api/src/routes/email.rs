//! One-click unsubscribe (FR-11.5, RFC 8058). The token in the link is signed, so no session is
//! needed: mail clients POST to the `List-Unsubscribe` URL on the user's behalf.

use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use mergetip_core::UserId;
use mergetip_store::users;
use serde::Deserialize;

use crate::{ApiError, AppState};

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct UnsubscribeQuery {
    /// The signed token from the email's link.
    token: String,
}

async fn unsubscribe(state: &AppState, token: &str) -> Result<UserId, ApiError> {
    let user_id = mergetip_notify::unsubscribe::verify(state.keyring(), token)
        .map(UserId::from_uuid)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_token",
                "This unsubscribe link isn't valid.",
            )
        })?;
    if !users::opt_out_of_email(state.pool(), user_id).await? {
        return Err(ApiError::not_found());
    }
    tracing::info!(%user_id, "unsubscribed from email");
    Ok(user_id)
}

/// Unsubscribe from the link in an email; answers with a short page.
#[utoipa::path(
    get,
    path = "/v1/email/unsubscribe",
    operation_id = "unsubscribe_link",
    tag = "email",
    params(UnsubscribeQuery),
    responses(
        (status = 200, description = "Unsubscribed (HTML page)", content_type = "text/html"),
        (status = 400, description = "`invalid_token`", body = crate::error::ErrorBody),
    )
)]
pub async fn unsubscribe_link(
    State(state): State<AppState>,
    Query(query): Query<UnsubscribeQuery>,
) -> Result<Response, ApiError> {
    unsubscribe(&state, &query.token).await?;
    let page = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width\"><title>Unsubscribed</title></head>\
        <body style=\"font-family: sans-serif; max-width: 480px; margin: 48px auto; padding: 0 16px;\">\
        <h1>You're unsubscribed</h1><p>MergeTip won't email you about tips any more. You'll still \
        be mentioned on GitHub, and you can turn emails back on in your settings.</p></body></html>";
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], page).into_response())
}

/// One-click unsubscribe (RFC 8058), sent by mail clients from the `List-Unsubscribe` header.
#[utoipa::path(
    post,
    path = "/v1/email/unsubscribe",
    operation_id = "unsubscribe_one_click",
    tag = "email",
    params(UnsubscribeQuery),
    responses(
        (status = 204, description = "Unsubscribed"),
        (status = 400, description = "`invalid_token`", body = crate::error::ErrorBody),
    )
)]
pub async fn unsubscribe_one_click(
    State(state): State<AppState>,
    Query(query): Query<UnsubscribeQuery>,
) -> Result<StatusCode, ApiError> {
    unsubscribe(&state, &query.token).await?;
    Ok(StatusCode::NO_CONTENT)
}
