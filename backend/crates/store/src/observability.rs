//! Read-only queries behind the sampled gauges (`jobs_ready`, `jobs_oldest_age_seconds`,
//! `tips_in_state`, the double-pay guard). Cheap enough to run every few seconds.

use sqlx::PgExecutor;

use crate::error::StoreResult;

/// The job queue as an alert sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueHealth {
    /// Jobs due now that no worker holds.
    pub ready: i64,
    /// How long the oldest of them has been due; 0 when none are.
    pub oldest_age_seconds: f64,
    /// Jobs that exhausted their attempts (kept for inspection).
    pub dead: i64,
}

pub async fn queue_health(exec: impl PgExecutor<'_>) -> StoreResult<QueueHealth> {
    let row = sqlx::query!(
        r#"
        SELECT
            count(*) FILTER (WHERE dead_at IS NULL AND run_at <= now() AND locked_at IS NULL)
                AS "ready!",
            COALESCE(EXTRACT(EPOCH FROM now() - min(run_at) FILTER (
                WHERE dead_at IS NULL AND run_at <= now() AND locked_at IS NULL)), 0)::float8
                AS "oldest!",
            count(*) FILTER (WHERE dead_at IS NOT NULL) AS "dead!"
        FROM jobs
        "#
    )
    .fetch_one(exec)
    .await?;
    Ok(QueueHealth {
        ready: row.ready,
        oldest_age_seconds: row.oldest,
        dead: row.dead,
    })
}

/// Tips with more than one succeeded payment attempt. The unique index makes this impossible;
/// the gauge proves it stays that way (SEV1 if it ever isn't zero).
pub async fn multiply_settled_tips(exec: impl PgExecutor<'_>) -> StoreResult<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!" FROM (
            SELECT tip_id FROM payment_attempts WHERE status = 'succeeded'
            GROUP BY tip_id HAVING count(*) > 1
        ) settled
        "#
    )
    .fetch_one(exec)
    .await?;
    Ok(count)
}
