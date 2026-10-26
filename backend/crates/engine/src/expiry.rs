//! Deadlines: tips that waited too long are closed (FR-4.4, RFC 0003 "Time windows").

use mergetip_core::TipState;
use mergetip_store::tips;

use crate::{Engine, EngineError, comments::cancel_tip};

/// Tips handled per state per run; the rest wait for the next run a minute later.
pub const EXPIRY_BATCH: i64 = 500;

/// What one run of [`Engine::expire_tips`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpiryReport {
    pub cancelled: usize,
}

impl Engine {
    /// Closes every tip past its deadline. Safe with several workers at once: rows are claimed
    /// with `SKIP LOCKED` and every move is a compare-and-set.
    pub async fn expire_tips(&self) -> Result<ExpiryReport, EngineError> {
        let now = self.clock.now();
        let mut report = ExpiryReport::default();

        let mut tx = self.pool.begin().await?;
        for tip in tips::lock_expired(&mut tx, TipState::AwaitingSender, now, EXPIRY_BATCH).await? {
            if cancel_tip(&mut tx, &tip, TipState::AwaitingSender, "sender_timeout").await? {
                report.cancelled += 1;
            }
        }
        tx.commit().await?;

        if report != ExpiryReport::default() {
            tracing::info!(cancelled = report.cancelled, "expired stale tips");
        }
        Ok(report)
    }
}
