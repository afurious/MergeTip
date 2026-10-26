//! Closing an account (`DELETE /me`): the sender's tips that are still waiting end here, so
//! nothing is paid later from an account that no longer exists.

use mergetip_core::{TipState, UserId};
use mergetip_store::{
    StoreError, installations,
    tips::{self, TipPatch},
};
use serde_json::json;
use sqlx::PgConnection;

use crate::{
    EngineError,
    comments::{cancel_tip, enqueue_tip_reply, reject_verifying},
    jobs::TipMessage,
};

const REASON: &str = "account_deleted";

/// Ends every waiting tip the user sent, inside the caller's transaction. Tips with a payment
/// in flight (`paying`) are left to finish: their payment fails cleanly once the wallet is
/// revoked. Returns how many tips were closed.
pub async fn close_sender_tips(
    conn: &mut PgConnection,
    user: UserId,
) -> Result<usize, EngineError> {
    let mut closed = 0;
    for tip in tips::lock_open_by_sender(conn, user).await? {
        let done = match tip.state {
            TipState::AwaitingSender | TipState::AwaitingConfirmation | TipState::Held => {
                cancel_tip(conn, &tip, tip.state, REASON).await?
            }
            TipState::Verifying => {
                let installation = installations::get(&mut *conn, tip.installation_id).await?;
                reject_verifying(
                    conn,
                    installation.github_installation_id,
                    &tip,
                    &TipPatch::error(REASON, None),
                )
                .await?;
                true
            }
            TipState::Pledged => {
                tips::clear_claim_token(&mut *conn, tip.id).await?;
                match tips::transition(
                    conn,
                    tip.id,
                    TipState::Pledged,
                    TipState::Expired,
                    &TipPatch::error(REASON, None),
                    json!({ "reason": REASON }),
                )
                .await
                {
                    Ok(expired) => {
                        // Its deadline is still ahead, so without this the digest would keep
                        // counting it as an expiry notice that hasn't gone out.
                        tips::mark_notified(
                            &mut *conn,
                            &[expired.id],
                            expired
                                .expires_at
                                .unwrap_or_else(time::OffsetDateTime::now_utc),
                            "none",
                        )
                        .await?;
                        let installation =
                            installations::get(&mut *conn, expired.installation_id).await?;
                        enqueue_tip_reply(
                            conn,
                            installation.github_installation_id,
                            &expired,
                            TipMessage::Cancelled,
                        )
                        .await?;
                        true
                    }
                    Err(StoreError::Conflict) => false,
                    Err(other) => return Err(other.into()),
                }
            }
            _ => false,
        };
        closed += usize::from(done);
    }
    Ok(closed)
}
