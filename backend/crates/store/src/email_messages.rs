//! `email_messages`: which tips each sent email covered, for provider delivery webhooks.

use mergetip_core::{TipId, UserId};
use sqlx::PgExecutor;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreResult;

/// Remembers a sent message. A provider ID seen twice (a retried send the provider deduplicated)
/// is kept as first recorded.
pub async fn record(
    exec: impl PgExecutor<'_>,
    provider_id: &str,
    user_id: UserId,
    tips: &[TipId],
    sent_at: OffsetDateTime,
) -> StoreResult<()> {
    let tips: Vec<Uuid> = tips.iter().map(TipId::as_uuid).collect();
    sqlx::query!(
        r#"
        INSERT INTO email_messages (provider_id, user_id, tip_ids, sent_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (provider_id) DO NOTHING
        "#,
        provider_id,
        user_id.as_uuid(),
        &tips,
        sent_at,
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// The tips a message covered; empty for a message we don't know.
pub async fn tips_for(exec: impl PgExecutor<'_>, provider_id: &str) -> StoreResult<Vec<Uuid>> {
    let tips = sqlx::query_scalar!(
        "SELECT tip_ids FROM email_messages WHERE provider_id = $1",
        provider_id
    )
    .fetch_optional(exec)
    .await?;
    Ok(tips.unwrap_or_default())
}
