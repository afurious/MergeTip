//! `GET /v1/me` and the `Me` shape returned by every settings endpoint.

use axum::{Json, extract::State};
use mergetip_core::UserId;
use mergetip_store::{payment_attempts, sessions, users, wallets};
use serde::Serialize;
use time::{OffsetDateTime, Time};
use utoipa::ToSchema;

use crate::{
    ApiError, AppState,
    extract::{AuthUser, SessionUser},
};

#[derive(Debug, Serialize, ToSchema)]
pub struct Me {
    pub id: String,
    pub github_login: String,
    pub avatar_url: String,
    pub email: Option<String>,
    pub lightning_address: Option<String>,
    pub wallet: WalletStatus,
    pub caps: Caps,
    pub confirm_threshold_sats: u64,
    pub confirm_otp_enabled: bool,
    pub email_opt_out: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WalletStatus {
    pub connected: bool,
    pub relay: Option<String>,
    /// NIP-47 methods the wallet granted, e.g. `pay_invoice`.
    pub methods: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Caps {
    pub per_tip_max_sats: u64,
    pub daily_max_sats: u64,
    /// Sats paid or in flight since midnight UTC.
    pub spent_today_sats: u64,
}

/// The signed-in user.
#[utoipa::path(
    get,
    path = "/v1/me",
    operation_id = "get_me",
    tag = "me",
    responses(
        (status = 200, body = Me),
        (status = 401, description = "Not signed in", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn get_me(State(state): State<AppState>, auth: AuthUser) -> Result<Json<Me>, ApiError> {
    build(&state, auth.user.id).await.map(Json)
}

/// Delete the account: secrets and personal data are erased and waiting tips are closed. Tip
/// records stay, anonymised, so replayed webhooks still can't create duplicates.
#[utoipa::path(
    delete,
    path = "/v1/me",
    operation_id = "delete_me",
    tag = "me",
    responses(
        (status = 204, description = "Deleted and signed out"),
        (status = 401, description = "Not signed in", body = crate::error::ErrorBody),
    ),
    security(("session" = []))
)]
pub async fn delete_me(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
) -> Result<axum::response::Response, ApiError> {
    use axum::{
        http::{StatusCode, header},
        response::IntoResponse,
    };

    let user_id = auth.user.id;
    let now = state.clock().now();
    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    users::lock(&mut tx, user_id).await?;
    wallets::revoke(&mut *tx, user_id, now).await?;
    let closed = mergetip_engine::account::close_sender_tips(&mut tx, user_id).await?;
    sessions::delete_for_user(&mut *tx, user_id).await?;
    mergetip_store::payout_methods::delete_for_user(&mut *tx, user_id).await?;
    users::anonymise(&mut *tx, user_id, now).await?;
    tx.commit().await.map_err(ApiError::internal)?;
    tracing::info!(%user_id, closed_tips = closed, "account deleted");

    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        crate::cookies::clear(
            crate::cookies::SESSION,
            "/",
            state.config().secure_cookies(),
        ),
    );
    Ok(response)
}

/// Assembles `Me` from current data. Settings endpoints return it after writing.
pub(crate) async fn build(state: &AppState, user_id: UserId) -> Result<Me, ApiError> {
    let user = users::get(state.pool(), user_id).await?;
    let email = user_email(state, user_id).await?;
    let wallet = wallets::active(state.pool(), user_id).await?;
    let midnight = utc_midnight(state.clock().now());
    let spent = payment_attempts::reserved_since(state.pool(), user_id, midnight).await?;

    Ok(Me {
        id: user.id.to_string(),
        github_login: user.github_login,
        avatar_url: user.avatar_url.unwrap_or_default(),
        email,
        lightning_address: user.lightning_address,
        wallet: WalletStatus {
            connected: wallet.is_some(),
            relay: wallet.as_ref().and_then(|w| w.relays.first().cloned()),
            methods: wallet.map(|w| w.methods).unwrap_or_default(),
        },
        caps: Caps {
            per_tip_max_sats: user.per_tip_max.get(),
            daily_max_sats: user.daily_max.get(),
            spent_today_sats: spent.get() / 1_000,
        },
        confirm_threshold_sats: user.confirm_threshold.get(),
        confirm_otp_enabled: user.confirm_otp_enabled,
        email_opt_out: user.email_opt_out,
    })
}

/// The user's stored email, decrypted (the API may read emails; only the worker reads NWC URIs).
/// An undecryptable value is logged and treated as absent.
pub(crate) async fn user_email(
    state: &AppState,
    user_id: UserId,
) -> Result<Option<String>, ApiError> {
    let secrets = users::secrets(state.pool(), user_id).await?;
    Ok(secrets.email_ciphertext.and_then(|blob| {
        state
            .keyring()
            .open(&blob, format!("users.email:{user_id}").as_bytes())
            .map_err(|err| tracing::error!(%user_id, error = %err, "cannot decrypt stored email"))
            .ok()
            .and_then(|plain| String::from_utf8(plain.expose().clone()).ok())
    }))
}

/// The daily cap resets at midnight UTC.
pub(crate) fn utc_midnight(now: OffsetDateTime) -> OffsetDateTime {
    now.to_offset(time::UtcOffset::UTC)
        .replace_time(Time::MIDNIGHT)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn day_starts_at_utc_midnight() {
        assert_eq!(
            utc_midnight(datetime!(2026-11-02 23:59 +01:00)),
            datetime!(2026-11-02 0:00 UTC)
        );
        assert_eq!(
            utc_midnight(datetime!(2026-11-03 0:30 +01:00)),
            datetime!(2026-11-02 0:00 UTC)
        );
    }
}
