//! Deadlines: tips that waited too long are closed (FR-4.4, RFC 0003 "Time windows").

use mergetip_core::TipState;
use mergetip_store::{
    StoreError, installations, otp_codes,
    tips::{self, Tip, TipPatch},
};
use serde_json::json;
use sqlx::PgConnection;

use crate::{
    Engine, EngineError,
    comments::{cancel_tip, enqueue_tip_reply},
    jobs::TipMessage,
    notices::enqueue_digest,
};

/// Tips handled per state per run; the rest wait for the next run a minute later.
pub const EXPIRY_BATCH: i64 = 500;

/// What one run of [`Engine::expire_tips`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpiryReport {
    /// Senders who never connected a wallet, and high-value tips nobody confirmed.
    pub cancelled: usize,
    /// Pledges nobody claimed.
    pub expired: usize,
    /// Receivers whose 7-day reminder was queued.
    pub reminders: usize,
}

impl Engine {
    /// Closes every tip past its deadline and queues due reminders. Safe with several workers
    /// at once: rows are claimed with `SKIP LOCKED` and every move is a compare-and-set, so no
    /// tip moves twice.
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

        let mut tx = self.pool.begin().await?;
        for tip in
            tips::lock_expired(&mut tx, TipState::AwaitingConfirmation, now, EXPIRY_BATCH).await?
        {
            if cancel_tip(
                &mut tx,
                &tip,
                TipState::AwaitingConfirmation,
                "confirmation_timeout",
            )
            .await?
            {
                otp_codes::delete(&mut *tx, tip.id).await?;
                report.cancelled += 1;
            }
        }
        tx.commit().await?;

        let mut tx = self.pool.begin().await?;
        for tip in tips::lock_expired(&mut tx, TipState::Pledged, now, EXPIRY_BATCH).await? {
            if expire_pledge(&mut tx, &tip, now).await? {
                report.expired += 1;
            }
        }
        tx.commit().await?;

        report.reminders = self.schedule_reminders().await?;
        if report != ExpiryReport::default() {
            tracing::info!(
                cancelled = report.cancelled,
                expired = report.expired,
                reminders = report.reminders,
                "expiry run"
            );
        }
        Ok(report)
    }
}

/// `pledged → expired`: tells the sender in the thread that nothing was charged, and queues the
/// receiver's final notice. The claim link then answers 410.
async fn expire_pledge(
    tx: &mut PgConnection,
    tip: &Tip,
    now: time::OffsetDateTime,
) -> Result<bool, EngineError> {
    let expired = match tips::transition(
        tx,
        tip.id,
        TipState::Pledged,
        TipState::Expired,
        &TipPatch::default(),
        json!({ "reason": "unclaimed" }),
    )
    .await
    {
        Ok(tip) => tip,
        Err(StoreError::Conflict) => return Ok(false),
        Err(other) => return Err(other.into()),
    };
    let installation = installations::get(&mut *tx, expired.installation_id).await?;
    enqueue_tip_reply(
        tx,
        installation.github_installation_id,
        &expired,
        TipMessage::Expired,
    )
    .await?;
    if let Some(receiver) = expired.receiver_id {
        enqueue_digest(tx, receiver, now).await?;
    }
    Ok(true)
}
