//! Gauges read from the database: queue health, tips per state and the double-pay guard. One
//! worker sampling is enough, but every worker does it; the values are the same.

use std::time::Duration;

use mergetip_core::TipState;
use mergetip_store::{PgPool, StoreResult, observability, tips};
use tokio_util::sync::CancellationToken;

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);

/// Reads the database once and sets every sampled gauge.
pub async fn sample(pool: &PgPool) -> StoreResult<()> {
    let queue = observability::queue_health(pool).await?;
    metrics::gauge!("jobs_ready").set(queue.ready as f64);
    metrics::gauge!("jobs_oldest_age_seconds").set(queue.oldest_age_seconds);
    metrics::gauge!("jobs_dead").set(queue.dead as f64);

    // Every state gets a value, so a state that empties reads 0 instead of keeping its last count.
    let counts = tips::count_by_state(pool).await?;
    for state in TipState::ALL {
        let count = counts
            .iter()
            .find(|(s, _)| s == state.as_str())
            .map_or(0, |(_, n)| *n);
        metrics::gauge!("tips_in_state", "state" => state.as_str()).set(count as f64);
    }

    let doubled = observability::multiply_settled_tips(pool).await?;
    metrics::gauge!("tips_settled_more_than_once").set(doubled as f64);
    if doubled > 0 {
        tracing::error!(tips = doubled, "a tip has more than one settled payment");
    }
    Ok(())
}

/// Samples every [`SAMPLE_INTERVAL`] until `shutdown`.
pub async fn run(pool: PgPool, shutdown: CancellationToken) {
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = tick.tick() => {
                if let Err(err) = sample(&pool).await {
                    tracing::warn!(error = %err, "metrics sampling failed");
                }
            }
        }
    }
}
