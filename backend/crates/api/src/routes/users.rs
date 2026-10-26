//! `GET /v1/users/{login}/receive-status`: what happens if you tip this person now (FR-8.3).

use axum::{
    Json,
    extract::{Path, State},
};
use mergetip_store::{StoreError, users};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{ApiError, AppState, extract::AuthUser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReceiveMethod {
    /// Paid straight away.
    LightningAddress,
    /// Becomes a pledge they can claim.
    None,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiveStatus {
    pub login: String,
    pub method: ReceiveMethod,
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
    let method = match users::get_by_login(state.pool(), &login).await {
        Ok(user) if user.lightning_address.is_some() => ReceiveMethod::LightningAddress,
        Ok(_) | Err(StoreError::NotFound) => ReceiveMethod::None,
        Err(other) => return Err(other.into()),
    };
    Ok(Json(ReceiveStatus { login, method }))
}
