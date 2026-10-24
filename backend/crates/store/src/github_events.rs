//! `github_events`: raw webhook deliveries, stored before any processing.

use serde_json::Value;
use sqlx::PgExecutor;
use time::OffsetDateTime;

use crate::error::StoreResult;

#[derive(Debug, Clone, PartialEq)]
pub struct GithubEvent {
    pub delivery_id: String,
    pub event: String,
    pub action: Option<String>,
    pub payload: Value,
    pub received_at: OffsetDateTime,
    pub processed_at: Option<OffsetDateTime>,
}

/// Stores a delivery. Returns `false` when this delivery ID was already stored, so GitHub's
/// redeliveries are acknowledged without being processed twice.
pub async fn insert_if_new(
    exec: impl PgExecutor<'_>,
    delivery_id: &str,
    event: &str,
    action: Option<&str>,
    payload: &Value,
) -> StoreResult<bool> {
    let result = sqlx::query!(
        r#"
        INSERT INTO github_events (delivery_id, event, action, payload)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (delivery_id) DO NOTHING
        "#,
        delivery_id,
        event,
        action,
        payload,
    )
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn get(exec: impl PgExecutor<'_>, delivery_id: &str) -> StoreResult<GithubEvent> {
    let event = sqlx::query_as!(
        GithubEvent,
        r#"
        SELECT delivery_id, event, action, payload, received_at, processed_at
        FROM github_events WHERE delivery_id = $1
        "#,
        delivery_id,
    )
    .fetch_one(exec)
    .await?;
    Ok(event)
}

pub async fn mark_processed(
    exec: impl PgExecutor<'_>,
    delivery_id: &str,
    at: OffsetDateTime,
) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE github_events SET processed_at = $2 WHERE delivery_id = $1",
        delivery_id,
        at,
    )
    .execute(exec)
    .await?;
    Ok(())
}
