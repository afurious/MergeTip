//! Tips waiting on their sender: resumed when the sender connects a wallet (journey C).

use mergetip_core::{TipState, UserId};
use mergetip_store::{
    StoreError,
    tips::{self, TipPatch},
};
use serde_json::json;

use crate::{
    Engine, EngineError,
    jobs::{self, VerifyTip},
};

impl Engine {
    /// Moves every unexpired `awaiting_sender` tip of `user_id` on to verification. A tip whose
    /// 24 h window has passed is left for the expiry job: connecting late must not revive it.
    pub async fn resume_sender_tips(&self, user_id: UserId) -> Result<(), EngineError> {
        let waiting = tips::list_awaiting_sender(&self.pool, user_id, self.clock.now()).await?;
        for tip in waiting {
            let mut tx = self.pool.begin().await?;
            match tips::transition(
                &mut tx,
                tip.id,
                TipState::AwaitingSender,
                TipState::Verifying,
                &TipPatch::default(),
                json!({ "reason": "wallet_connected" }),
            )
            .await
            {
                Ok(_) => {}
                Err(StoreError::Conflict) => continue,
                Err(other) => return Err(other.into()),
            }
            jobs::enqueue(&mut tx, &VerifyTip { tip_id: tip.id }).await?;
            tx.commit().await?;
            tracing::info!(tip_id = %tip.id, %user_id, "tip resumed: sender connected a wallet");
        }
        Ok(())
    }
}
