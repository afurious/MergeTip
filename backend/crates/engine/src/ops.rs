//! Operator actions for stuck fiat payouts (RFC 0004 "Stuck payouts", runbook `fiat-stuck.md`).
//!
//! A `fiat_stuck` tip is the one case where value sits outside a user wallet: Bitnob holds the
//! sats and the local currency wasn't delivered. An operator must retry or refund within 24 h.
//! These run from the worker binary (`mergetip-worker ops …`); each records who acted in the
//! tip's audit trail (`tip_events.detail.operator`).

use mergetip_core::{TipId, TipState};
use mergetip_fiat::{FiatProvider, PayoutStatus};
use mergetip_payer::{Network, invoice};
use mergetip_store::{
    PgPool,
    tips::{self, FiatDetails, TipPatch},
};
use serde_json::json;
use time::OffsetDateTime;

use crate::{EngineError, payouts::apply_status};

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error("tip {0} not found")]
    NotFound(TipId),
    #[error("tip is {0}, not fiat_stuck")]
    NotStuck(TipState),
    #[error("tip has no provider payout to retry")]
    NoPayout,
    #[error("refund invoice rejected: {0}")]
    BadRefund(String),
    #[error("payout provider: {0}")]
    Provider(#[from] mergetip_fiat::FiatError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

impl From<mergetip_store::StoreError> for OpsError {
    fn from(err: mergetip_store::StoreError) -> Self {
        Self::Engine(err.into())
    }
}

fn require_stuck(tip: &tips::Tip) -> Result<(), OpsError> {
    if tip.state == TipState::FiatStuck {
        Ok(())
    } else {
        Err(OpsError::NotStuck(tip.state))
    }
}

/// `fiat_stuck → fiat_processing`, then asks Bitnob for the payout's status and applies it: a
/// delivered payout becomes `paid`, a failed one is stuck again. Returns the tip's state after.
pub async fn fiat_retry(
    pool: &PgPool,
    provider: &dyn FiatProvider,
    tip_id: TipId,
    operator: &str,
) -> Result<TipState, OpsError> {
    let tip = match tips::get(pool, tip_id).await {
        Ok(tip) => tip,
        Err(mergetip_store::StoreError::NotFound) => return Err(OpsError::NotFound(tip_id)),
        Err(other) => return Err(other.into()),
    };
    require_stuck(&tip)?;
    let payout_id = tip.fiat.payout_id.clone().ok_or(OpsError::NoPayout)?;

    let mut tx = pool.begin().await.map_err(EngineError::from)?;
    tips::transition(
        &mut tx,
        tip_id,
        TipState::FiatStuck,
        TipState::FiatProcessing,
        &TipPatch::default(),
        json!({ "operator": operator, "action": "fiat-retry" }),
    )
    .await?;
    // The operator reopened it: statuses from here on count again.
    tips::set_fiat_details(
        &mut *tx,
        tip_id,
        &FiatDetails {
            provider_status: Some(PayoutStatus::Initiated.as_str().into()),
            ..FiatDetails::default()
        },
    )
    .await?;
    tx.commit().await.map_err(EngineError::from)?;
    tracing::warn!(%tip_id, operator, "operator retried a stuck fiat payout");

    let payout = provider.get_payout(&payout_id).await?;
    if let Some(status) = PayoutStatus::parse(&payout.status) {
        let mut tx = pool.begin().await.map_err(EngineError::from)?;
        let tip = tips::lock_for_update(&mut tx, tip_id).await?;
        apply_status(&mut tx, &tip, status).await?;
        tx.commit().await.map_err(EngineError::from)?;
    }
    Ok(tips::get(pool, tip_id).await?.state)
}

/// Records an operator refund to the sender (`fiat_stuck → refunded`). The operator pays
/// `bolt11` (the sender's invoice for the full tip) from the Bitnob balance; this checks the
/// invoice is for exactly the tip on the right network and writes it to the audit trail.
pub async fn fiat_refund(
    pool: &PgPool,
    tip_id: TipId,
    bolt11: &str,
    network: Network,
    operator: &str,
    now: OffsetDateTime,
) -> Result<TipState, OpsError> {
    let tip = match tips::get(pool, tip_id).await {
        Ok(tip) => tip,
        Err(mergetip_store::StoreError::NotFound) => return Err(OpsError::NotFound(tip_id)),
        Err(other) => return Err(other.into()),
    };
    require_stuck(&tip)?;
    let msats = tip
        .amount
        .to_msats()
        .ok_or_else(|| OpsError::BadRefund("tip amount overflows msat".into()))?;
    let refund = invoice::validate_provider_invoice(bolt11, msats, network, now)
        .map_err(|err| OpsError::BadRefund(err.to_string()))?;

    let mut tx = pool.begin().await.map_err(EngineError::from)?;
    tips::transition(
        &mut tx,
        tip_id,
        TipState::FiatStuck,
        TipState::Refunded,
        &TipPatch::default(),
        json!({
            "operator": operator,
            "action": "fiat-refund",
            "refund_bolt11": refund.bolt11,
            "refund_payment_hash": refund.payment_hash,
        }),
    )
    .await?;
    tx.commit().await.map_err(EngineError::from)?;
    tracing::warn!(%tip_id, operator, "operator refunded a stuck fiat payout");
    Ok(TipState::Refunded)
}
