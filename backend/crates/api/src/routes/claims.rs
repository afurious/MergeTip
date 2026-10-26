//! Claiming a pledge (FR-6.5, FR-11.6, journey B, RFC 0003 "Pledges and claim tokens").
//!
//! The link is public (it is in the GitHub thread). What protects a pledge is that only the
//! receiver, signed in with GitHub, can claim it, once, before it expires.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use mergetip_core::TipState as Core;
use mergetip_crypto::hash_token;
use mergetip_engine::jobs::{self, PayTip};
use mergetip_store::{
    tips::{self, Tip as StoredTip, TipPatch},
    users,
};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use crate::{
    ApiError, AppState,
    extract::SessionUser,
    routes::{
        settings::checked_address,
        tips::{Tip, TipEnvelope},
    },
};

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "claim_not_found",
        "This claim link isn't valid, or it was already used.",
    )
}

fn expired() -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "claim_expired",
        "This tip expired before it was claimed. Nothing was charged.",
    )
}

fn already_claimed() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "already_claimed",
        "This tip was already claimed.",
    )
}

/// A pledge is claimable while it is `pledged` and before its deadline. The expiry job may lag
/// the deadline by a minute; the deadline itself is what counts.
fn check_claimable(state: &AppState, tip: &StoredTip) -> Result<(), ApiError> {
    match tip.state {
        Core::Pledged if tip.expires_at.is_some_and(|at| at <= state.clock().now()) => {
            Err(expired())
        }
        Core::Pledged => Ok(()),
        Core::Expired => Err(expired()),
        _ => Err(already_claimed()),
    }
}

/// The pledge behind a claim link, for the claim page.
#[utoipa::path(
    get,
    path = "/v1/claims/{token}",
    operation_id = "get_claim",
    tag = "claims",
    params(("token" = String, Path, description = "The token from the claim link")),
    responses(
        (status = 200, body = TipEnvelope),
        (status = 404, description = "`claim_not_found`", body = crate::error::ErrorBody),
        (status = 410, description = "`claim_expired`", body = crate::error::ErrorBody),
    )
)]
pub async fn get_claim(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Json<TipEnvelope>, ApiError> {
    let tip = tips::get_by_claim_hash(state.pool(), &hash_token(&token))
        .await?
        .ok_or_else(not_found)?;
    if let Err(err) = check_claimable(&state, &tip)
        && err.status == StatusCode::GONE
    {
        return Err(err);
    }
    Ok(Json(TipEnvelope {
        tip: Tip::load(&state, &tip).await?,
    }))
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    /// Where to receive the sats. Omit to use the address already saved in settings.
    lightning_address: Option<String>,
}

/// Claim a pledge as its receiver: the sender's wallet pays it to the receiver's address.
#[utoipa::path(
    post,
    path = "/v1/claims/{token}",
    operation_id = "claim_tip",
    tag = "claims",
    params(("token" = String, Path, description = "The token from the claim link")),
    request_body(content = ClaimRequest, description = "Optional; an empty body uses the saved address"),
    responses(
        (status = 200, body = TipEnvelope),
        (status = 403, description = "`not_receiver`", body = crate::error::ErrorBody),
        (status = 404, description = "`claim_not_found`", body = crate::error::ErrorBody),
        (status = 409, description = "`already_claimed`", body = crate::error::ErrorBody),
        (status = 410, description = "`claim_expired`", body = crate::error::ErrorBody),
        (status = 422, description = "`no_address`, `lnurl_invalid` or `lnurl_unreachable`", body = crate::error::ErrorBody),
    ),
    security(("session" = []))
)]
pub async fn claim_tip(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
    Path(token): Path<String>,
    body: Option<Json<ClaimRequest>>,
) -> Result<Json<TipEnvelope>, ApiError> {
    let token_hash = hash_token(&token);
    let tip = tips::get_by_claim_hash(state.pool(), &token_hash)
        .await?
        .ok_or_else(not_found)?;
    // The receiver is linked by GitHub account ID, set by verification and by sign-in alike.
    if tip.receiver_id != Some(auth.user.id) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "not_receiver",
            "Only the person this tip is for can claim it. Sign in with their GitHub account.",
        ));
    }
    check_claimable(&state, &tip)?;

    // Checking an address calls out to its LNURL server, so it happens before any lock.
    let requested = body.and_then(|Json(b)| b.lightning_address);
    let address = match requested {
        Some(raw) => Some(
            checked_address(&state, auth.user.id, &raw)
                .await?
                .to_string(),
        ),
        None => None,
    };
    if address.is_none() && auth.user.lightning_address.is_none() {
        return Err(ApiError::unprocessable(
            "no_address",
            "Add a Lightning address to receive this tip.",
        ));
    }

    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    // Re-read under the lock: a concurrent claim may have used the token in the meantime.
    let tip = tips::lock_by_claim_hash(&mut tx, &token_hash)
        .await?
        .ok_or_else(already_claimed)?;
    check_claimable(&state, &tip)?;
    if let Some(address) = &address {
        users::set_lightning_address(&mut *tx, auth.user.id, Some(address)).await?;
    }
    tips::clear_claim_token(&mut *tx, tip.id).await?;
    let claimed = tips::transition(
        &mut tx,
        tip.id,
        Core::Pledged,
        Core::Paying,
        &TipPatch::default(),
        json!({ "reason": "claimed", "user_id": auth.user.id }),
    )
    .await?;
    jobs::enqueue(&mut tx, &PayTip { tip_id: tip.id }).await?;
    tx.commit().await.map_err(ApiError::internal)?;
    tracing::info!(tip_id = %tip.id, user_id = %auth.user.id, "pledge claimed");

    Ok(Json(TipEnvelope {
        tip: Tip::load(&state, &claimed).await?,
    }))
}
