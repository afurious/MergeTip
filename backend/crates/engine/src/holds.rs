//! On-merge holds: `pull_request.closed` → pay or cancel the PR's `held` tips (FR-5, journey B).

use mergetip_core::{GithubInstallationId, TipKind, TipState};
use mergetip_github::{
    RepoName,
    verifier::{self, VerifyError, VerifyRequest},
    webhook::PullRequestEvent,
};
use mergetip_store::{
    StoreError, installations,
    tips::{self, Tip, TipPatch},
    users,
};
use serde_json::json;

use crate::{
    Engine, EngineError,
    comments::cancel_tip,
    jobs::{self, PayTip, ResolveHeld},
};

impl Engine {
    /// Queues the resolution of the PR's holds. The payload's `merged` flag is not trusted: the
    /// job asks GitHub, so a stale or replayed delivery can't pay anything early.
    pub(crate) async fn handle_pull_closed(
        &self,
        event: PullRequestEvent,
    ) -> Result<(), EngineError> {
        let Some(installation) = event.installation else {
            return Ok(());
        };
        let mut tx = self.pool.begin().await?;
        jobs::enqueue(
            &mut tx,
            &ResolveHeld {
                installation: GithubInstallationId::new(installation.id),
                repo: event.repository.full_name,
                number: event.pull_request.number,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pays the PR's held tips if it merged, cancels them if it closed unmerged, and leaves them
    /// while it is open. Safe to run any number of times: each tip moves at most once.
    pub async fn resolve_held(&self, job: &ResolveHeld) -> Result<(), EngineError> {
        let held = tips::list_in_thread(&self.pool, &job.repo, job.number, TipState::Held).await?;
        if held.is_empty() {
            return Ok(());
        }
        let installation = match installations::get_by_github_id(&self.pool, job.installation).await
        {
            Ok(installation) => installation,
            Err(StoreError::NotFound) => return Ok(()),
            Err(other) => return Err(other.into()),
        };
        if installation.suspended_at.is_some() {
            return Ok(());
        }
        let repo: RepoName = job.repo.parse()?;
        let pull = match self
            .github
            .get_pull(job.installation, &repo, job.number)
            .await
        {
            Ok(pull) => pull,
            Err(err) if err.is_retryable() => return Err(err.into()),
            // The PR or the App's access is gone for good: the holds can never resolve.
            Err(err) => {
                tracing::warn!(repo = %job.repo, number = job.number, error = %err,
                    "held tips can no longer be checked");
                for tip in &held {
                    self.cancel_held(tip, "verification_unavailable").await?;
                }
                return Ok(());
            }
        };

        let merged = pull.merged == Some(true) || pull.merged_at.is_some();
        if merged {
            for tip in &held {
                self.release_held(tip).await?;
            }
        } else if pull.state == "closed" {
            for tip in &held {
                self.cancel_held(tip, "pr_closed").await?;
            }
        }
        Ok(())
    }

    /// The PR merged: check the receiver's code is still on it, then pay.
    async fn release_held(&self, tip: &Tip) -> Result<(), EngineError> {
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
            Err(VerifyError::GitHub(err)) if err.is_retryable() => return Err(err.into()),
            Err(VerifyError::GitHub(_)) => {
                return self.cancel_held(tip, "verification_unavailable").await;
            }
            Err(VerifyError::Rejected(rejection)) => {
                return self.cancel_held(tip, rejection.code()).await;
            }
        };

        let mut tx = self.pool.begin().await?;
        let receiver = users::upsert_by_github_id(
            &mut tx,
            &users::GithubProfile {
                github_id: mergetip_core::GithubUserId::new(verification.receiver.id),
                login: verification.receiver.login.clone(),
                avatar_url: verification.receiver.avatar_url.clone(),
                created_at: verification.receiver.created_at,
            },
        )
        .await?;
        let moved = tips::transition(
            &mut tx,
            tip.id,
            TipState::Held,
            TipState::Paying,
            &TipPatch {
                receiver_id: Some(receiver.id),
                verified_by: Some(verification.evidence),
                ..TipPatch::default()
            },
            json!({ "reason": "pr_merged" }),
        )
        .await;
        match moved {
            Ok(_) => {}
            // Another run already moved it.
            Err(StoreError::Conflict) => return Ok(()),
            Err(other) => return Err(other.into()),
        }
        jobs::enqueue(&mut tx, &PayTip { tip_id: tip.id }).await?;
        tx.commit().await?;
        tracing::info!(tip_id = %tip.id, "held tip released by merge");
        Ok(())
    }

    async fn cancel_held(&self, tip: &Tip, code: &str) -> Result<(), EngineError> {
        let mut tx = self.pool.begin().await?;
        if cancel_tip(&mut tx, tip, TipState::Held, code).await? {
            tx.commit().await?;
            tracing::info!(tip_id = %tip.id, code, "held tip cancelled");
        }
        Ok(())
    }
}
