//! `GET /v1/users/{login}/receive-status`: what happens if you tip this person now (FR-8.3).

use axum::{
    Json,
    extract::{Path, State},
};
use mergetip_store::{
    StoreError,
    payout_methods::{self, PayoutMethod},
    tips, users,
};
use rust_decimal::Decimal;
use serde::Serialize;
use utoipa::ToSchema;

use crate::{ApiError, AppState, extract::AuthUser, routes::payouts::provider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReceiveMethod {
    /// Paid straight away.
    LightningAddress,
    /// Paid out in local currency (tips below `fiat_min_sats` go to their Lightning address, or
    /// become a pledge).
    Fiat,
    /// Becomes a pledge they can claim.
    None,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiveStatus {
    pub login: String,
    pub method: ReceiveMethod,
    /// For `fiat`: the smallest tip the receiver's corridor pays out, estimated from the last
    /// quoted rate. Absent when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fiat_min_sats: Option<u64>,
}

/// Whether a GitHub user can receive a tip right now. Unknown users can't, yet.
#[utoipa::path(
    get,
    path = "/v1/users/{login}/receive-status",
    operation_id = "receive_status",
    tag = "users",
    params(("login" = String, Path, description = "GitHub login")),
    responses(
        (status = 200, body = ReceiveStatus),
        (status = 401, description = "Not signed in", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn receive_status(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(login): Path<String>,
) -> Result<Json<ReceiveStatus>, ApiError> {
    let valid = !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !valid {
        return Err(ApiError::not_found());
    }
    let user = match users::get_by_login(state.pool(), &login).await {
        Ok(user) => Some(user),
        Err(StoreError::NotFound) => None,
        Err(other) => return Err(other.into()),
    };
    if let Some(user) = &user
        && let Some(method) = payout_methods::default_for_user(state.pool(), user.id).await?
        && let Ok(fiat) = provider(&state).await
    {
        let fiat_min_sats = fiat_min_sats(&state, fiat, &method).await;
        return Ok(Json(ReceiveStatus {
            login,
            method: ReceiveMethod::Fiat,
            fiat_min_sats,
        }));
    }
    let method = match user {
        Some(user) if user.lightning_address.is_some() => ReceiveMethod::LightningAddress,
        _ => ReceiveMethod::None,
    };
    Ok(Json(ReceiveStatus {
        login,
        method,
        fiat_min_sats: None,
    }))
}

/// The corridor minimum in sats: `min_amount ÷ rate × 10⁸`, rounded up. `None` without a limit
/// or a recent rate; this is an estimate for the extension's warning, never used to pay.
async fn fiat_min_sats(
    state: &AppState,
    fiat: &dyn mergetip_fiat::FiatProvider,
    method: &PayoutMethod,
) -> Option<u64> {
    let limits = fiat.limits().await.ok()?;
    let limit = limits.iter().find(|l| {
        l.country.eq_ignore_ascii_case(&method.country)
            && l.currency.eq_ignore_ascii_case(&method.currency)
    })?;
    let rate = tips::latest_fx_rate(state.pool(), &method.currency)
        .await
        .ok()??;
    if rate.is_zero() {
        return None;
    }
    let sats = (limit.min_amount / rate * Decimal::from(100_000_000_u64)).ceil();
    u64::try_from(sats).ok()
}
