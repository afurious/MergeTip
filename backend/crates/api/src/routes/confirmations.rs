//! Confirming high-value tips in the web app (FR-12, RFC 0003).
//!
//! Every endpoint needs the web session cookie (never the extension's bearer token) of the tip's
//! sender. Confirming is the only way out of `awaiting_confirmation` besides cancelling or the
//! 1-hour expiry: no GitHub comment can do it, so a hijacked GitHub account alone can't move a
//! large amount.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use mergetip_core::{TipId, TipState};
use mergetip_engine::confirm::{
    self, OTP_MAX_ATTEMPTS, OTP_RESEND_INTERVAL, OTP_TTL, new_otp, otp_mac, otp_matches,
};
use mergetip_notify::{Email, MailError};
use mergetip_store::{
    otp_codes,
    tips::{self, Tip as StoredTip},
};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::{
    ApiError, AppState,
    extract::{AuthUser, SessionUser},
    routes::{
        me::user_email,
        tips::{Tip, TipEnvelope},
    },
};

fn parse_id(raw: &str) -> Result<TipId, ApiError> {
    raw.parse::<uuid::Uuid>()
        .map(TipId::from_uuid)
        .map_err(|_| ApiError::not_found())
}

fn check_sender(auth: &AuthUser, tip: &StoredTip) -> Result<(), ApiError> {
    if tip.sender_id == auth.user.id {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "not_sender",
            "Only the person who sent this tip can confirm or cancel it.",
        ))
    }
}

/// The tip must still be waiting, and its hour must not be up. The expiry job may lag the
/// deadline; the deadline is what counts.
fn check_open(state: &AppState, tip: &StoredTip) -> Result<(), ApiError> {
    if tip.state != TipState::AwaitingConfirmation {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_awaiting_confirmation",
            "This tip isn't waiting for confirmation any more.",
        ));
    }
    if tip
        .confirm_expires_at
        .is_none_or(|at| at <= state.clock().now())
    {
        return Err(ApiError::new(
            StatusCode::GONE,
            "confirmation_expired",
            "The hour to confirm this tip has passed, so it was cancelled. Nothing was charged.",
        ));
    }
    Ok(())
}

fn otp_invalid(message: &'static str) -> ApiError {
    ApiError::unprocessable("otp_invalid", message)
}

/// A high-value tip waiting for its sender's confirmation.
#[utoipa::path(
    get,
    path = "/v1/confirmations/{tip_id}",
    operation_id = "get_confirmation",
    tag = "confirmations",
    params(("tip_id" = String, Path)),
    responses(
        (status = 200, body = TipEnvelope),
        (status = 403, description = "`not_sender`", body = crate::error::ErrorBody),
        (status = 404, description = "No such tip", body = crate::error::ErrorBody),
    ),
    security(("session" = []))
)]
pub async fn get_confirmation(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
    Path(tip_id): Path<String>,
) -> Result<Json<TipEnvelope>, ApiError> {
    let tip = tips::get(state.pool(), parse_id(&tip_id)?).await?;
    check_sender(&auth, &tip)?;
    Ok(Json(TipEnvelope {
        tip: Tip::load(&state, &tip).await?,
    }))
}

/// Email a 6-digit code for confirming this tip (only when the sender turned codes on).
#[utoipa::path(
    post,
    path = "/v1/confirmations/{tip_id}/otp",
    operation_id = "send_confirmation_code",
    tag = "confirmations",
    params(("tip_id" = String, Path)),
    responses(
        (status = 204, description = "Code sent"),
        (status = 403, description = "`not_sender`", body = crate::error::ErrorBody),
        (status = 409, description = "`not_awaiting_confirmation`", body = crate::error::ErrorBody),
        (status = 410, description = "`confirmation_expired`", body = crate::error::ErrorBody),
        (status = 422, description = "`otp_not_enabled` or `otp_unavailable` (no email on file)", body = crate::error::ErrorBody),
        (status = 429, description = "`otp_rate_limited`", body = crate::error::ErrorBody),
        (status = 502, description = "`email_failed`", body = crate::error::ErrorBody),
    ),
    security(("session" = []))
)]
pub async fn send_code(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
    Path(tip_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let tip = tips::get(state.pool(), parse_id(&tip_id)?).await?;
    check_sender(&auth, &tip)?;
    check_open(&state, &tip)?;
    if !auth.user.confirm_otp_enabled {
        return Err(ApiError::unprocessable(
            "otp_not_enabled",
            "Confirmation codes are off in your settings; confirm without one.",
        ));
    }
    let Some(email) = user_email(&state, auth.user.id).await? else {
        return Err(ApiError::unprocessable(
            "otp_unavailable",
            "There's no email address on your account to send a code to.",
        ));
    };
    let now = state.clock().now();
    if let Some(existing) = otp_codes::get(state.pool(), tip.id).await?
        && now - existing.created_at < OTP_RESEND_INTERVAL
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "otp_rate_limited",
            "A code was sent less than a minute ago. Check your email, or try again shortly.",
        ));
    }

    let code = new_otp()?;
    otp_codes::replace(
        state.pool(),
        tip.id,
        &otp_mac(state.keyring(), tip.id, &code),
        now,
        now + OTP_TTL,
    )
    .await?;
    let message = Email {
        to: email,
        subject: "Your MergeTip confirmation code".into(),
        html: format!(
            "<p>Your code to confirm the {} tip to @{}: <strong>{code}</strong></p>\
             <p>It expires in 10 minutes. If you didn't ask for it, ignore this email.</p>",
            tip.amount, tip.receiver_login
        ),
        text: format!(
            "Your code to confirm the {} tip to @{}: {code}\n\nIt expires in 10 minutes. If you \
             didn't ask for it, ignore this email.\n",
            tip.amount, tip.receiver_login
        ),
        headers: Vec::new(),
        tag: "otp".into(),
        idempotency_key: None,
    };
    if let Err(err) = state.mailer().send(&message).await {
        // No code was delivered, so don't let the rate limit block asking again.
        otp_codes::delete(state.pool(), tip.id).await?;
        tracing::warn!(tip_id = %tip.id, error = %err, "confirmation code not sent");
        return Err(match err {
            MailError::NotConfigured => ApiError::unprocessable(
                "otp_unavailable",
                "Email isn't available right now, so a code can't be sent.",
            ),
            _ => ApiError::new(
                StatusCode::BAD_GATEWAY,
                "email_failed",
                "The code couldn't be emailed. Try again in a moment.",
            ),
        });
    }
    tracing::info!(tip_id = %tip.id, "confirmation code sent");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    /// The emailed code; required when confirmation codes are on.
    otp: Option<String>,
}

/// Confirm a high-value tip: it is paid now, or when its PR merges for an on-merge tip.
#[utoipa::path(
    post,
    path = "/v1/confirmations/{tip_id}",
    operation_id = "confirm_tip",
    tag = "confirmations",
    params(("tip_id" = String, Path)),
    request_body(content = ConfirmRequest, description = "Optional unless codes are on"),
    responses(
        (status = 200, body = TipEnvelope),
        (status = 403, description = "`not_sender`", body = crate::error::ErrorBody),
        (status = 409, description = "`not_awaiting_confirmation`", body = crate::error::ErrorBody),
        (status = 410, description = "`confirmation_expired`", body = crate::error::ErrorBody),
        (status = 422, description = "`otp_invalid`", body = crate::error::ErrorBody),
    ),
    security(("session" = []))
)]
pub async fn confirm_tip(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
    Path(tip_id): Path<String>,
    body: Option<Json<ConfirmRequest>>,
) -> Result<Json<TipEnvelope>, ApiError> {
    let tip_id = parse_id(&tip_id)?;
    let otp = body.and_then(|Json(b)| b.otp);
    let now = state.clock().now();

    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    let tip = tips::lock_for_update(&mut tx, tip_id).await?;
    check_sender(&auth, &tip)?;
    check_open(&state, &tip)?;

    if auth.user.confirm_otp_enabled {
        let code = otp.ok_or_else(|| otp_invalid("Enter the code we emailed you."))?;
        let stored = otp_codes::lock(&mut tx, tip_id)
            .await?
            .filter(|stored| stored.expires_at > now)
            .ok_or_else(|| otp_invalid("That code has expired. Request a new one."))?;
        if !otp_matches(state.keyring(), tip_id, &code, &stored.code_hmac) {
            // The failed guess is committed before answering, so it always counts.
            let attempts = otp_codes::record_attempt(&mut *tx, tip_id).await?;
            if attempts >= OTP_MAX_ATTEMPTS {
                otp_codes::delete(&mut *tx, tip_id).await?;
            }
            tx.commit().await.map_err(ApiError::internal)?;
            tracing::info!(%tip_id, attempts, "wrong confirmation code");
            return Err(if attempts >= OTP_MAX_ATTEMPTS {
                otp_invalid("Too many wrong codes. Request a new one.")
            } else {
                otp_invalid("That code isn't right.")
            });
        }
        otp_codes::delete(&mut *tx, tip_id).await?;
    }

    let released = confirm::confirm(&mut tx, &tip, now).await?;
    tx.commit().await.map_err(ApiError::internal)?;
    tracing::info!(%tip_id, user_id = %auth.user.id, "high-value tip confirmed");
    Ok(Json(TipEnvelope {
        tip: Tip::load(&state, &released).await?,
    }))
}

/// Cancel a high-value tip instead of confirming it. Nothing is charged.
#[utoipa::path(
    delete,
    path = "/v1/confirmations/{tip_id}",
    operation_id = "cancel_confirmation",
    tag = "confirmations",
    params(("tip_id" = String, Path)),
    responses(
        (status = 204, description = "Cancelled"),
        (status = 403, description = "`not_sender`", body = crate::error::ErrorBody),
        (status = 409, description = "`not_awaiting_confirmation`", body = crate::error::ErrorBody),
    ),
    security(("session" = []))
)]
pub async fn cancel_tip(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
    Path(tip_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let tip_id = parse_id(&tip_id)?;
    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    let tip = tips::lock_for_update(&mut tx, tip_id).await?;
    check_sender(&auth, &tip)?;
    if tip.state != TipState::AwaitingConfirmation {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_awaiting_confirmation",
            "This tip isn't waiting for confirmation any more.",
        ));
    }
    confirm::cancel_by_sender(&mut tx, &tip).await?;
    otp_codes::delete(&mut *tx, tip_id).await?;
    tx.commit().await.map_err(ApiError::internal)?;
    tracing::info!(%tip_id, "high-value tip cancelled by its sender");
    Ok(StatusCode::NO_CONTENT)
}
