//! Fiat payout outcomes (RFC 0004 "Webhooks"): Bitnob's webhooks (and operator re-polls) move a
//! tip from `fiat_processing` to `paid` or `fiat_stuck`.
//!
//! Webhooks can arrive twice, late or out of order. Each status has a rank, and a tip's recorded
//! provider status only ever moves up, so a stale `processing` after a `success` changes nothing.

use mergetip_core::{TipId, TipState};
use mergetip_fiat::PayoutStatus;
use mergetip_github::Reaction;
use mergetip_store::{
    StoreError, bitnob_events, installations,
    tips::{self, FiatDetails, Tip, TipPatch},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;

use crate::{
    Engine, EngineError,
    comments::{enqueue_reaction, enqueue_tip_reply},
    jobs::TipMessage,
};

#[derive(Debug, Deserialize)]
struct Event {
    event: String,
    data: EventData,
}

#[derive(Debug, Deserialize)]
struct EventData {
    id: String,
    #[serde(default)]
    reference: Option<String>,
}

/// The payout status an event announces. Route on `event`, not `data.status` (Bitnob docs).
fn status_of(event: &str) -> Option<PayoutStatus> {
    match event {
        "payouts.initialized" => Some(PayoutStatus::Initiated),
        "payouts.processing" => Some(PayoutStatus::Processing),
        "payouts.withdrawal.success" => Some(PayoutStatus::Success),
        "payouts.withdrawal.expired" | "payouts.withdrawal.failed" | "payouts.failed" => {
            Some(PayoutStatus::Failed)
        }
        _ => None,
    }
}

impl Engine {
    /// Applies a stored webhook delivery. Safe to run more than once.
    pub async fn process_bitnob_event(&self, event_id: &str) -> Result<(), EngineError> {
        let (payload, processed) = bitnob_events::get(&self.pool, event_id).await?;
        if processed.is_some() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        match serde_json::from_value::<Event>(payload) {
            Ok(event) => match status_of(&event.event) {
                Some(status) => match self.lock_payout_tip(&mut tx, &event.data).await? {
                    Some(tip) => apply_status(&mut tx, &tip, status).await?,
                    None => tracing::warn!(event_id, payout = %event.data.id,
                            "Bitnob event for an unknown payout"),
                },
                None => tracing::debug!(event_id, event = %event.event, "Bitnob event ignored"),
            },
            Err(err) => tracing::warn!(event_id, error = %err, "unreadable Bitnob event"),
        }
        bitnob_events::mark_processed(&mut *tx, event_id, self.clock.now()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The tip a payout belongs to: by payout ID, or by our `"{tip}:{attempt}:{round}"`
    /// reference when the payout ID wasn't recorded yet.
    async fn lock_payout_tip(
        &self,
        tx: &mut PgConnection,
        data: &EventData,
    ) -> Result<Option<Tip>, EngineError> {
        if let Some(tip) = tips::lock_by_payout_id(tx, &data.id).await? {
            return Ok(Some(tip));
        }
        let Some(tip_id) = data
            .reference
            .as_deref()
            .and_then(|r| r.split(':').next())
            .and_then(|id| id.parse::<uuid::Uuid>().ok())
        else {
            return Ok(None);
        };
        match tips::lock_for_update(tx, TipId::from_uuid(tip_id)).await {
            // Only a payout this tip is waiting on; an earlier, abandoned payout is ignored.
            Ok(tip) if tip.fiat.payout_id.as_deref() == Some(data.id.as_str()) => Ok(Some(tip)),
            Ok(_) | Err(StoreError::NotFound) => Ok(None),
            Err(other) => Err(other.into()),
        }
    }

    /// Re-applies the recorded provider status after the wallet paid (a success webhook can beat
    /// the wallet's own answer).
    pub(crate) async fn apply_recorded_payout_status(
        &self,
        tip_id: TipId,
    ) -> Result<(), EngineError> {
        let mut tx = self.pool.begin().await?;
        let tip = tips::lock_for_update(&mut tx, tip_id).await?;
        if let Some(status) = tip
            .fiat
            .provider_status
            .as_deref()
            .and_then(PayoutStatus::parse)
            .filter(|s| s.is_final())
        {
            move_by_status(&mut tx, &tip, status).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Records `status` if it ranks above the tip's current one, then moves the tip if it can.
pub(crate) async fn apply_status(
    tx: &mut PgConnection,
    tip: &Tip,
    status: PayoutStatus,
) -> Result<(), EngineError> {
    let current = tip
        .fiat
        .provider_status
        .as_deref()
        .and_then(PayoutStatus::parse);
    if current.is_some_and(|c| c.is_final() || c >= status) {
        tracing::debug!(tip_id = %tip.id, ?status, ?current, "stale payout status ignored");
        return Ok(());
    }
    tips::set_fiat_details(
        &mut *tx,
        tip.id,
        &FiatDetails {
            provider_status: Some(status.as_str().to_owned()),
            ..FiatDetails::default()
        },
    )
    .await?;
    move_by_status(tx, tip, status).await
}

/// `fiat_processing → paid` on success (with the receipt), `→ fiat_stuck` on failure. A tip
/// still in `paying` waits: the wallet's answer applies the recorded status when it lands.
async fn move_by_status(
    tx: &mut PgConnection,
    tip: &Tip,
    status: PayoutStatus,
) -> Result<(), EngineError> {
    if tip.state != TipState::FiatProcessing {
        return Ok(());
    }
    match status {
        PayoutStatus::Success => {
            let paid = tips::transition(
                tx,
                tip.id,
                TipState::FiatProcessing,
                TipState::Paid,
                &TipPatch::default(),
                json!({ "reason": "payout_delivered" }),
            )
            .await?;
            let installation = installations::get(&mut *tx, paid.installation_id).await?;
            enqueue_tip_reply(
                tx,
                installation.github_installation_id,
                &paid,
                TipMessage::PaidOut,
            )
            .await?;
            enqueue_reaction(
                tx,
                installation.github_installation_id,
                &paid.thread,
                paid.comment_id,
                Reaction::Rocket,
            )
            .await?;
            tracing::info!(tip_id = %tip.id, "fiat payout delivered");
        }
        PayoutStatus::Failed => {
            tips::transition(
                tx,
                tip.id,
                TipState::FiatProcessing,
                TipState::FiatStuck,
                &TipPatch::error("payout_failed", None),
                json!({ "reason": "payout_failed" }),
            )
            .await?;
            // Bitnob holds the sats now: an operator must retry or refund (runbook fiat-stuck).
            tracing::error!(tip_id = %tip.id, "fiat payout stuck: sats with the provider, fiat not delivered");
        }
        PayoutStatus::Initiated | PayoutStatus::Processing => {}
    }
    Ok(())
}
