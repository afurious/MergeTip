//! `pay_tip`. Until real payments land (M3), only the staging stub exists: it marks the tip paid
//! with a random but internally consistent proof, so the whole flow up to the receipt can be
//! exercised without a wallet.

use mergetip_core::TipState;
use mergetip_github::Reaction;
use mergetip_store::{
    installations,
    tips::{self, TipPatch},
};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    Engine, EngineError,
    comments::{enqueue_reaction, enqueue_tip_reply},
    jobs::TipMessage,
};

impl Engine {
    /// Settles a tip in `paying` with a fake payment. Refuses unless `stub_payer` is on.
    pub async fn pay_tip_stub(&self, tip_id: mergetip_core::TipId) -> Result<(), EngineError> {
        if !self.settings.stub_payer {
            return Err(EngineError::Malformed(
                "the stub payer is disabled; real payments are not available yet".into(),
            ));
        }
        let tip = tips::get(&self.pool, tip_id).await?;
        if tip.state != TipState::Paying {
            return Ok(());
        }
        let installation = installations::get(&self.pool, tip.installation_id).await?;

        let preimage = mergetip_crypto::random_bytes::<32>()
            .map_err(|err| EngineError::Malformed(err.to_string()))?;
        let payment_hash = hex::encode(Sha256::digest(preimage));

        let mut tx = self.pool.begin().await?;
        let paid = tips::transition(
            &mut tx,
            tip.id,
            TipState::Paying,
            TipState::Paid,
            &TipPatch {
                payment_hash: Some(payment_hash),
                preimage: Some(hex::encode(preimage)),
                ..TipPatch::default()
            },
            json!({ "payer": "stub" }),
        )
        .await?;
        let github_installation = installation.github_installation_id;
        enqueue_tip_reply(&mut tx, github_installation, &paid, TipMessage::Paid).await?;
        enqueue_reaction(
            &mut tx,
            github_installation,
            &paid.thread,
            paid.comment_id,
            Reaction::Rocket,
        )
        .await?;
        tx.commit().await?;
        tracing::info!(tip_id = %tip.id, "tip paid by the stub payer");
        Ok(())
    }
}
