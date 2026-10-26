//! Connecting a wallet, linking a Lightning address and changing limits (FR-6.2–6.4).

use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode};
use mergetip_core::Sats;
use mergetip_crypto::Redacted;
use mergetip_engine::jobs::{self, ResumeSenderTips};
use mergetip_payer::{LightningAddress, ResolveError, nwc::PayError};
use mergetip_store::{
    users::{self, SettingsUpdate},
    wallets::{self, NewConnection},
};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::{
    ApiError, AppState,
    extract::AuthUser,
    routes::me::{self, Me},
};

/// Connecting and checking a wallet must finish well inside the request timeout.
const WALLET_CHECK_TIMEOUT: Duration = Duration::from_secs(8);
/// The lowest confirmation threshold a sender may set (FR-12.1).
const MIN_CONFIRM_THRESHOLD: u64 = 1_000;

#[derive(Deserialize, ToSchema)]
pub struct WalletRequest {
    /// `nostr+walletconnect://…`. Treated as a password: never stored in plain text, logged or
    /// returned.
    #[schema(value_type = String)]
    nwc_uri: Redacted<String>,
}

/// Connect (or replace) the sender's wallet.
#[utoipa::path(
    put,
    path = "/v1/me/wallet",
    operation_id = "connect_wallet",
    tag = "me",
    request_body = WalletRequest,
    responses(
        (status = 200, body = Me),
        (status = 422, description = "`nwc_invalid_uri`, `nwc_unreachable` or `nwc_missing_pay_invoice`", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn connect_wallet(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<WalletRequest>,
) -> Result<Json<Me>, ApiError> {
    let unreachable = || {
        ApiError::unprocessable(
            "nwc_unreachable",
            "Couldn't reach your wallet. Check it's online and try again.",
        )
    };
    let check = async {
        let wallet = state.wallets().connect(body.nwc_uri.expose()).await?;
        let info = wallet.get_info().await;
        wallet.close().await;
        info
    };
    let info = match tokio::time::timeout(WALLET_CHECK_TIMEOUT, check).await {
        Err(_elapsed) => return Err(unreachable()),
        Ok(Err(PayError::InvalidUri)) => {
            return Err(ApiError::unprocessable(
                "nwc_invalid_uri",
                "That isn't a valid wallet connection string.",
            ));
        }
        Ok(Err(err)) => {
            tracing::info!(user_id = %auth.user.id, error = %err, "wallet check failed");
            return Err(unreachable());
        }
        Ok(Ok(info)) => info,
    };
    if !info.supports("pay_invoice") {
        return Err(ApiError::unprocessable(
            "nwc_missing_pay_invoice",
            "This connection can't pay invoices. Create one with the pay_invoice permission.",
        ));
    }

    let id = uuid::Uuid::new_v4();
    let ciphertext = state
        .keyring()
        .seal(
            body.nwc_uri.expose().as_bytes(),
            format!("wallet_connections.nwc_uri:{id}").as_bytes(),
        )
        .map_err(ApiError::internal)?;
    // The wallet and the job that resumes the sender's waiting tips commit together, so a
    // crash can't leave tips waiting on a wallet that is already connected.
    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    wallets::replace(
        &mut tx,
        &NewConnection {
            id,
            user_id: auth.user.id,
            nwc_uri_ciphertext: &ciphertext,
            wallet_pubkey: &info.wallet_pubkey,
            relays: &info.relays,
            methods: &info.methods,
            encryption: info.encryption.as_str(),
        },
        state.clock().now(),
    )
    .await?;
    jobs::enqueue(
        &mut tx,
        &ResumeSenderTips {
            user_id: auth.user.id,
        },
    )
    .await?;
    tx.commit().await.map_err(ApiError::internal)?;
    tracing::info!(user_id = %auth.user.id, wallet = %info.wallet_pubkey, "wallet connected");
    me::build(&state, auth.user.id).await.map(Json)
}

/// Disconnect the sender's wallet.
#[utoipa::path(
    delete,
    path = "/v1/me/wallet",
    operation_id = "disconnect_wallet",
    tag = "me",
    responses((status = 204, description = "Disconnected (or nothing was connected)")),
    security(("session" = []), ("bearer" = []))
)]
pub async fn disconnect_wallet(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<StatusCode, ApiError> {
    if wallets::revoke(state.pool(), auth.user.id, state.clock().now()).await? {
        tracing::info!(user_id = %auth.user.id, "wallet disconnected");
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
pub struct AddressRequest {
    /// A Lightning address such as `alice@getalby.com`.
    address: String,
}

/// Link the receiver's Lightning address, after checking it resolves.
#[utoipa::path(
    put,
    path = "/v1/me/lightning-address",
    operation_id = "set_lightning_address",
    tag = "me",
    request_body = AddressRequest,
    responses(
        (status = 200, body = Me),
        (status = 422, description = "`lnurl_invalid` or `lnurl_unreachable`", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn set_lightning_address(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<AddressRequest>,
) -> Result<Json<Me>, ApiError> {
    let invalid = || {
        ApiError::unprocessable(
            "lnurl_invalid",
            "That doesn't look like a working Lightning address.",
        )
    };
    let address: LightningAddress = body.address.parse().map_err(|_| invalid())?;
    match state.receivers().probe(&address).await {
        Ok(()) => {}
        Err(ResolveError::Unreachable(_)) => {
            return Err(ApiError::unprocessable(
                "lnurl_unreachable",
                "Couldn't reach that Lightning address. Check it and try again.",
            ));
        }
        Err(err) => {
            tracing::info!(user_id = %auth.user.id, error = %err, "Lightning address rejected");
            return Err(invalid());
        }
    }
    users::set_lightning_address(state.pool(), auth.user.id, Some(&address.to_string())).await?;
    me::build(&state, auth.user.id).await.map(Json)
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingsRequest {
    per_tip_max_sats: Option<u64>,
    daily_max_sats: Option<u64>,
    confirm_threshold_sats: Option<u64>,
    confirm_otp_enabled: Option<bool>,
    email_opt_out: Option<bool>,
}

/// Change limits and preferences. Omitted fields are unchanged.
#[utoipa::path(
    patch,
    path = "/v1/me/settings",
    operation_id = "update_settings",
    tag = "me",
    request_body = SettingsRequest,
    responses(
        (status = 200, body = Me),
        (status = 422, description = "`invalid_setting`", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn update_settings(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<SettingsRequest>,
) -> Result<Json<Me>, ApiError> {
    let invalid = |message: &'static str| ApiError::unprocessable("invalid_setting", message);
    // Amounts are stored as BIGINT, and nobody needs a cap above 1 BTC.
    let amount = |value: Option<u64>| value.filter(|v| *v <= 100_000_000).map(Sats::new);
    for value in [
        body.per_tip_max_sats,
        body.daily_max_sats,
        body.confirm_threshold_sats,
    ]
    .into_iter()
    .flatten()
    {
        if value == 0 || value > 100_000_000 {
            return Err(invalid("Limits must be between 1 and 100,000,000 sats."));
        }
    }
    if body
        .confirm_threshold_sats
        .is_some_and(|t| t < MIN_CONFIRM_THRESHOLD)
    {
        return Err(invalid(
            "The confirmation threshold can't be below 1,000 sats.",
        ));
    }
    let user = &auth.user;
    let per_tip = body.per_tip_max_sats.unwrap_or(user.per_tip_max.get());
    let daily = body.daily_max_sats.unwrap_or(user.daily_max.get());
    if per_tip > daily {
        return Err(invalid(
            "The per-tip limit can't be higher than the daily limit.",
        ));
    }

    users::update_settings(
        state.pool(),
        user.id,
        &SettingsUpdate {
            per_tip_max: amount(body.per_tip_max_sats),
            daily_max: amount(body.daily_max_sats),
            confirm_threshold: amount(body.confirm_threshold_sats),
            confirm_otp_enabled: body.confirm_otp_enabled,
            email_opt_out: body.email_opt_out,
        },
    )
    .await?;
    me::build(&state, user.id).await.map(Json)
}
