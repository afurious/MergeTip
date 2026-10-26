//! `bitnob_events`: Bitnob webhook deliveries, deduplicated by `event_id`.

use serde_json::Value;
use sqlx::PgExecutor;
use time::OffsetDateTime;

use crate::error::StoreResult;

/// Stores a delivery. Returns `false` when the event was already received (a retry).
pub async fn insert_if_new(
    exec: impl PgExecutor<'_>,
    event_id: &str,
    payload: &Value,
) -> StoreResult<bool> {
    let inserted = sqlx::query_scalar!(
        r#"
        INSERT INTO bitnob_events (event_id, payload) VALUES ($1, $2)
        ON CONFLICT (event_id) DO NOTHING
        RETURNING event_id
        "#,
        event_id,
        payload,
    )
    .fetch_optional(exec)
    .await?;
    Ok(inserted.is_some())
}

/// The stored payload, and whether it was processed already.
pub async fn get(
    exec: impl PgExecutor<'_>,
    event_id: &str,
) -> StoreResult<(Value, Option<OffsetDateTime>)> {
    let row = sqlx::query!(
        "SELECT payload, processed_at FROM bitnob_events WHERE event_id = $1",
        event_id
    )
    .fetch_one(exec)
    .await?;
    Ok((row.payload, row.processed_at))
}

pub async fn mark_processed(
    exec: impl PgExecutor<'_>,
    event_id: &str,
    at: OffsetDateTime,
) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE bitnob_events SET processed_at = $2 WHERE event_id = $1",
        event_id,
        at
    )
    .execute(exec)
    .await?;
    Ok(())
}
