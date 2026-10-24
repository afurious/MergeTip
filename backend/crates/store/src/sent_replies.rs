//! `sent_replies`: what the bot has already said, so retries never repeat it.

use sqlx::PgConnection;

use crate::error::StoreResult;

/// Records `key` as sent. Returns `false` if it already was.
///
/// Call it inside the transaction that performs the write and commit after the write succeeded:
/// a failed write rolls the claim back, and a concurrent claim of the same key waits for this
/// transaction and then sees it as sent.
pub async fn claim(conn: &mut PgConnection, key: &str) -> StoreResult<bool> {
    let result = sqlx::query!(
        "INSERT INTO sent_replies (key) VALUES ($1) ON CONFLICT (key) DO NOTHING",
        key
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}
