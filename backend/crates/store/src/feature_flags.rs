//! `feature_flags`: runtime switches (e.g. `payments_enabled`, the SEV1 kill switch).

use sqlx::PgExecutor;

use crate::error::StoreResult;

/// Global kill switch for paying tips. Off: payments wait (they are retried later).
pub const PAYMENTS_ENABLED: &str = "payments_enabled";

/// Commit-author emails may be used to notify pledge receivers (RFC 0003 privacy note). With no
/// row, it is on outside production and off in production.
pub const EMAIL_FROM_COMMITS: &str = "email_from_commits";

/// The flag's explicit setting, or `None` when no row exists (callers pick the default).
pub async fn lookup(exec: impl PgExecutor<'_>, key: &str) -> StoreResult<Option<bool>> {
    let enabled = sqlx::query_scalar!("SELECT enabled FROM feature_flags WHERE key = $1", key)
        .fetch_optional(exec)
        .await?;
    Ok(enabled)
}

/// Whether the flag is on. An unknown flag is off, so a typo can never enable something.
pub async fn is_enabled(exec: impl PgExecutor<'_>, key: &str) -> StoreResult<bool> {
    let enabled = sqlx::query_scalar!("SELECT enabled FROM feature_flags WHERE key = $1", key)
        .fetch_optional(exec)
        .await?;
    Ok(enabled.unwrap_or(false))
}

pub async fn set(exec: impl PgExecutor<'_>, key: &str, enabled: bool) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO feature_flags (key, enabled) VALUES ($1, $2)
        ON CONFLICT (key) DO UPDATE SET enabled = EXCLUDED.enabled
        "#,
        key,
        enabled,
    )
    .execute(exec)
    .await?;
    Ok(())
}
