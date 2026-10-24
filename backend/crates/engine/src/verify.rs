//! `verify_tip`: check the contribution, then decide what happens to the tip (FR-3, FR-12.2).

use mergetip_core::{GithubUserId, Sats, TipKind, TipState};
use mergetip_github::{
    Reaction,
    verifier::{self, VerifyError, VerifyRequest},
};
use mergetip_store::{
    installations,
    tips::{self, Tip, TipPatch},
    users::{self, GithubProfile, User},
};
use serde_json::json;
use time::Duration;

use crate::{
    Engine, EngineError,
    comments::{enqueue_reaction, enqueue_tip_reply},
    jobs::{self, PayTip, TipMessage},
};

/// How long a high-value tip waits for the sender to confirm it in the web app (FR-12.6).
pub const CONFIRMATION_WINDOW: Duration = Duration::hours(1);

/// What happens to a verified tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Reject(&'static str),
    /// Above the sender's threshold: only the signed-in sender can release it (FR-12).
    Confirm,
    /// `on-merge`: pays when the PR merges.
    Hold,
    Pay,
}

/// Pure decision for a verified tip, in priority order:
///
/// 1. Above the per-tip cap: rejected; no confirmation can override a hard cap.
/// 2. Above the confirmation threshold: needs confirmation. This comes before holding, so a
///    large on-merge tip is confirmed first and then waits for the merge.
/// 3. `on-merge`: held.
/// 4. Otherwise: paid now.
pub fn decide(amount: Sats, kind: TipKind, sender: &User) -> Decision {
    if amount > sender.per_tip_max {
        Decision::Reject("over_per_tip_cap")
    } else if amount > sender.confirm_threshold {
        Decision::Confirm
    } else if kind == TipKind::OnMerge {
        Decision::Hold
    } else {
        Decision::Pay
    }
}

impl Engine {
    pub async fn verify_tip(&self, tip_id: mergetip_core::TipId) -> Result<(), EngineError> {
        let tip = tips::get(&self.pool, tip_id).await?;
        if tip.state != TipState::Verifying {
            // Already decided by an earlier run of this job.
            return Ok(());
        }
        let sender = users::get(&self.pool, tip.sender_id).await?;
        let installation = installations::get(&self.pool, tip.installation_id).await?;
        let request = VerifyRequest {
            installation: installation.github_installation_id,
            thread: &tip.thread,
            sender_github_id: sender.github_id.get(),
            receiver_login: &tip.receiver_login,
            on_merge: tip.kind == TipKind::OnMerge,
            now: self.clock.now(),
        };

        let verification = match verifier::verify(self.github.as_ref(), &request).await {
            Ok(verification) => verification,
            Err(VerifyError::GitHub(err)) => return Err(err.into()),
            Err(VerifyError::Rejected(rejection)) => {
                return self
                    .reject(&tip, installation.github_installation_id, rejection.code())
                    .await;
            }
        };

        let mut tx = self.pool.begin().await?;
        let receiver = users::upsert_by_github_id(
            &mut tx,
            &GithubProfile {
                github_id: GithubUserId::new(verification.receiver.id),
                login: verification.receiver.login.clone(),
                avatar_url: verification.receiver.avatar_url.clone(),
                created_at: verification.receiver.created_at,
            },
        )
        .await?;
        let verified = TipPatch {
            receiver_id: Some(receiver.id),
            verified_by: Some(verification.evidence.clone()),
            ..TipPatch::default()
        };
        let github_installation = installation.github_installation_id;

        match decide(tip.amount, tip.kind, &sender) {
            Decision::Reject(code) => {
                tips::transition(
                    &mut tx,
                    tip.id,
                    TipState::Verifying,
                    TipState::Rejected,
                    &TipPatch {
                        error_code: Some(code.to_owned()),
                        ..verified
                    },
                    json!({ "reason": code }),
                )
                .await?;
                enqueue_tip_reply(&mut tx, github_installation, &tip, TipMessage::Rejected).await?;
                enqueue_reaction(
                    &mut tx,
                    github_installation,
                    &tip.thread,
                    tip.comment_id,
                    Reaction::Confused,
                )
                .await?;
            }
            Decision::Confirm => {
                tips::transition(
                    &mut tx,
                    tip.id,
                    TipState::Verifying,
                    TipState::AwaitingConfirmation,
                    &TipPatch {
                        confirm_expires_at: Some(self.clock.now() + CONFIRMATION_WINDOW),
                        ..verified
                    },
                    json!({ "threshold_sats": sender.confirm_threshold.get() }),
                )
                .await?;
                enqueue_tip_reply(
                    &mut tx,
                    github_installation,
                    &tip,
                    TipMessage::NeedsConfirmation,
                )
                .await?;
            }
            Decision::Hold => {
                tips::transition(
                    &mut tx,
                    tip.id,
                    TipState::Verifying,
                    TipState::Held,
                    &verified,
                    json!({}),
                )
                .await?;
                enqueue_tip_reply(&mut tx, github_installation, &tip, TipMessage::Held).await?;
            }
            Decision::Pay => {
                tips::transition(
                    &mut tx,
                    tip.id,
                    TipState::Verifying,
                    TipState::Paying,
                    &verified,
                    json!({}),
                )
                .await?;
                jobs::enqueue(&mut tx, &PayTip { tip_id: tip.id }).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn reject(
        &self,
        tip: &Tip,
        installation: mergetip_core::GithubInstallationId,
        code: &'static str,
    ) -> Result<(), EngineError> {
        let mut tx = self.pool.begin().await?;
        tips::transition(
            &mut tx,
            tip.id,
            TipState::Verifying,
            TipState::Rejected,
            &TipPatch::error(code, None),
            json!({ "reason": code }),
        )
        .await?;
        enqueue_tip_reply(&mut tx, installation, tip, TipMessage::Rejected).await?;
        enqueue_reaction(
            &mut tx,
            installation,
            &tip.thread,
            tip.comment_id,
            Reaction::Confused,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mergetip_core::{GithubUserId, UserId};
    use time::macros::datetime;

    use super::*;

    fn sender(per_tip: u64, threshold: u64) -> User {
        User {
            id: UserId::new_random(),
            github_id: GithubUserId::new(1),
            github_login: "maintainer".into(),
            avatar_url: None,
            github_created_at: None,
            lightning_address: None,
            per_tip_max: Sats::new(per_tip),
            daily_max: Sats::new(1_000_000),
            confirm_threshold: Sats::new(threshold),
            confirm_otp_enabled: false,
            email_opt_out: false,
            created_at: datetime!(2026-01-01 0:00 UTC),
            deleted_at: None,
        }
    }

    #[test]
    fn decisions_in_priority_order() {
        let caps = sender(100_000, 50_000);
        let cases = [
            (100_001, TipKind::Now, Decision::Reject("over_per_tip_cap")),
            (
                100_001,
                TipKind::OnMerge,
                Decision::Reject("over_per_tip_cap"),
            ),
            (50_001, TipKind::Now, Decision::Confirm),
            (50_001, TipKind::OnMerge, Decision::Confirm),
            (50_000, TipKind::Now, Decision::Pay),
            (50_000, TipKind::OnMerge, Decision::Hold),
            (1, TipKind::Now, Decision::Pay),
        ];
        for (amount, kind, expected) in cases {
            assert_eq!(
                decide(Sats::new(amount), kind, &caps),
                expected,
                "{amount} {kind}"
            );
        }
    }

    #[test]
    fn cap_below_threshold_still_rejects_first() {
        let caps = sender(10_000, 50_000);
        assert_eq!(
            decide(Sats::new(20_000), TipKind::Now, &caps),
            Decision::Reject("over_per_tip_cap")
        );
    }
}
