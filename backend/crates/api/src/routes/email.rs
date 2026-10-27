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

fn invalid_token() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_token",
        "This unsubscribe link isn't valid.",
    )
}

async fn unsubscribe(state: &AppState, token: &str) -> Result<UserId, ApiError> {
    let user_id = mergetip_notify::unsubscribe::verify(state.keyring(), token)
        .map(UserId::from_uuid)
        .ok_or_else(invalid_token)?;
    if !users::opt_out_of_email(state.pool(), user_id).await? {
        return Err(ApiError::not_found());
    }
    tracing::info!(%user_id, "unsubscribed from email");
    Ok(user_id)
}

fn page(title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title></head>\
         <body style=\"font-family: sans-serif; max-width: 480px; margin: 48px auto; padding: 0 16px;\">\
         {body}</body></html>"
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// The page behind the link in an email: it asks before unsubscribing. Opening a link must not
/// change anything (RFC 8058): mail scanners and link previews fetch every link in a message.
#[utoipa::path(
    get,
    path = "/v1/email/unsubscribe",
    operation_id = "unsubscribe_link",
    tag = "email",
    params(UnsubscribeQuery),
    responses(
        (status = 200, description = "A page with an unsubscribe button (HTML)", content_type = "text/html"),
        (status = 400, description = "`invalid_token`", body = crate::error::ErrorBody),
    )
)]
pub async fn unsubscribe_link(
    State(state): State<AppState>,
    Query(query): Query<UnsubscribeQuery>,
) -> Result<Response, ApiError> {
    // Check the token now, so a bad link says so before the button is pressed.
    mergetip_notify::unsubscribe::verify(state.keyring(), &query.token)
        .ok_or_else(invalid_token)?;
    // Tokens are `hex.base64url`: nothing in them needs escaping inside an attribute.
    let token: String = query
        .token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    Ok(page(
        "Unsubscribe from MergeTip emails",
        &format!(
            "<h1>Stop MergeTip emails?</h1><p>You won't get emails about tips any more. You'll \
             still be mentioned on GitHub, and you can turn emails back on in your settings.</p>\
             <form method=\"post\" action=\"?token={token}\">\
             <button type=\"submit\" style=\"padding: 10px 16px; font-size: 16px;\">\
             Unsubscribe</button></form>"
        ),
    ))
}

/// Unsubscribe: RFC 8058 one-click from a mail client (`List-Unsubscribe=One-Click`, answered
/// 204), or the button on the confirm page (answered with a page).
#[utoipa::path(
    post,
    path = "/v1/email/unsubscribe",
    operation_id = "unsubscribe_one_click",
    tag = "email",
    params(UnsubscribeQuery),
    request_body(content = String, content_type = "application/x-www-form-urlencoded", description = "`List-Unsubscribe=One-Click` from a mail client, or empty from the confirm page"),
    responses(
        (status = 204, description = "Unsubscribed (one-click)"),
        (status = 200, description = "Unsubscribed (from the confirm page; HTML)", content_type = "text/html"),
        (status = 400, description = "`invalid_token`", body = crate::error::ErrorBody),
    )
)]
pub async fn unsubscribe_one_click(
    State(state): State<AppState>,
    Query(query): Query<UnsubscribeQuery>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    unsubscribe(&state, &query.token).await?;
    if body.as_ref() == b"List-Unsubscribe=One-Click" {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(page(
        "Unsubscribed",
        "<h1>You're unsubscribed</h1><p>MergeTip won't email you about tips any more. You'll \
         still be mentioned on GitHub, and you can turn emails back on in your settings.</p>",
    ))
}
