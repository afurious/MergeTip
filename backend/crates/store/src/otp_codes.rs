//! `otp_codes`: the live confirmation code for a high-value tip, as an HMAC.

use mergetip_core::TipId;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;

use crate::error::StoreResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpCode {
    pub code_hmac: String,
    pub expires_at: OffsetDateTime,
    /// Wrong guesses so far.
    pub attempts: i32,
    pub created_at: OffsetDateTime,
}

/// Issues a new code for the tip, replacing any earlier one (and its attempt count).
pub async fn replace(
    exec: impl PgExecutor<'_>,
    tip_id: TipId,
    code_hmac: &str,
    created_at: OffsetDateTime,
    expires_at: OffsetDateTime,
) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO otp_codes (tip_id, code_hmac, expires_at, attempts, created_at)
        VALUES ($1, $2, $3, 0, $4)
        ON CONFLICT (tip_id) DO UPDATE SET
            code_hmac = EXCLUDED.code_hmac, expires_at = EXCLUDED.expires_at,
            attempts = 0, created_at = EXCLUDED.created_at
        "#,
        tip_id.as_uuid(),
        code_hmac,
        expires_at,
        created_at,
    )
    .execute(exec)
    .await?;
    Ok(())
}

pub async fn get(exec: impl PgExecutor<'_>, tip_id: TipId) -> StoreResult<Option<OtpCode>> {
    let code = sqlx::query_as!(
        OtpCode,
        "SELECT code_hmac, expires_at, attempts, created_at FROM otp_codes WHERE tip_id = $1",
        tip_id.as_uuid()
    )
    .fetch_optional(exec)
    .await?;
    Ok(code)
}

/// The code, locked until the caller's transaction ends, so two guesses can't both be counted
/// against a stale attempt count.
pub async fn lock(conn: &mut PgConnection, tip_id: TipId) -> StoreResult<Option<OtpCode>> {
    let code = sqlx::query_as!(
        OtpCode,
        r#"
        SELECT code_hmac, expires_at, attempts, created_at FROM otp_codes
        WHERE tip_id = $1 FOR UPDATE
        "#,
        tip_id.as_uuid()
    )
    .fetch_optional(conn)
    .await?;
    Ok(code)
}

/// Counts a wrong guess; returns the new total.
pub async fn record_attempt(exec: impl PgExecutor<'_>, tip_id: TipId) -> StoreResult<i32> {
    let attempts = sqlx::query_scalar!(
        "UPDATE otp_codes SET attempts = attempts + 1 WHERE tip_id = $1 RETURNING attempts",
        tip_id.as_uuid()
    )
    .fetch_one(exec)
    .await?;
    Ok(attempts)
}

pub async fn delete(exec: impl PgExecutor<'_>, tip_id: TipId) -> StoreResult<()> {
    sqlx::query!("DELETE FROM otp_codes WHERE tip_id = $1", tip_id.as_uuid())
        .execute(exec)
        .await?;
    Ok(())
}
