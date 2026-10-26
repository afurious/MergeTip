//! Acting on GitHub as a signed-in user (posting their `/tip` comment from the extension).

use mergetip_core::UserId;
use mergetip_crypto::Redacted;
use mergetip_store::users;
use time::Duration;

use crate::{ApiError, AppState};

/// Refresh a user token this long before it expires, so it can't lapse mid-request.
const REFRESH_MARGIN: Duration = Duration::minutes(5);

/// A usable GitHub token for the user, refreshed first if it is about to expire.
///
/// Fails with `401 github_reauth_required` when there is no token or GitHub refuses the refresh
/// token: the user has to sign in again.
pub async fn for_user(state: &AppState, user_id: UserId) -> Result<Redacted<String>, ApiError> {
    let secrets = users::secrets(state.pool(), user_id).await?;
    let open = |blob: &[u8], column: &str| -> Result<Redacted<String>, ApiError> {
        let plain = state
            .keyring()
            .open(blob, format!("users.{column}:{user_id}").as_bytes())
            .map_err(ApiError::internal)?;
        String::from_utf8(plain.expose().clone())
            .map(Redacted::new)
            .map_err(ApiError::internal)
    };

    let access = secrets
        .github_token_ciphertext
        .as_deref()
        .ok_or_else(reauth_required)?;
    let now = state.clock().now();
    let fresh = secrets
        .github_token_expires_at
        .is_none_or(|expires| expires - now > REFRESH_MARGIN);
    if fresh {
        return open(access, "github_token");
    }

    let refresh = secrets
        .github_refresh_token_ciphertext
        .as_deref()
        .ok_or_else(reauth_required)?;
    let refresh = open(refresh, "github_refresh_token")?;
    let tokens = state
        .oauth()
        .refresh(refresh.expose())
        .await
        .map_err(|err| {
            tracing::info!(%user_id, error = %err, "GitHub token refresh refused");
            reauth_required()
        })?;
    let seal = |value: &str, column: &str| {
        state
            .keyring()
            .seal(
                value.as_bytes(),
                format!("users.{column}:{user_id}").as_bytes(),
            )
            .map_err(ApiError::internal)
    };
    let access_blob = seal(tokens.access_token.expose(), "github_token")?;
    let refresh_blob = tokens
        .refresh_token
        .as_ref()
        .map(|t| seal(t.expose(), "github_refresh_token"))
        .transpose()?;
    users::set_github_tokens(
        state.pool(),
        user_id,
        &access_blob,
        refresh_blob.as_deref(),
        tokens.expires_at,
    )
    .await?;
    Ok(tokens.access_token)
}

fn reauth_required() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::UNAUTHORIZED,
        "github_reauth_required",
        "Sign in with GitHub again to continue.",
    )
}
