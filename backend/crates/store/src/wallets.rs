//! `wallet_connections`: a sender's NWC connection. Writes arrive in M3; until then the engine
//! only needs to know whether a sender has one.

use mergetip_core::UserId;
use sqlx::PgExecutor;

use crate::error::StoreResult;

/// Whether the user has a connected, unrevoked wallet.
pub async fn has_active(exec: impl PgExecutor<'_>, user: UserId) -> StoreResult<bool> {
    let active = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM wallet_connections WHERE user_id = $1 AND revoked_at IS NULL
        ) AS "active!"
        "#,
        user.as_uuid(),
    )
    .fetch_one(exec)
    .await?;
    Ok(active)
}
