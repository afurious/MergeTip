//! `process_event`: route one stored webhook delivery to its use case.

use mergetip_core::GithubInstallationId;
use mergetip_github::webhook::{
    InstallationEvent, InstallationRepositoriesEvent, IssueCommentEvent,
};
use mergetip_store::{
    github_events,
    installations::{self, InstallationRepo},
};
use serde::de::DeserializeOwned;

use crate::{Engine, EngineError};

impl Engine {
    /// Handles one delivery. Safe to run more than once for the same delivery: every write it
    /// makes is idempotent.
    pub async fn process_event(&self, delivery_id: &str) -> Result<(), EngineError> {
        let event = match github_events::get(&self.pool, delivery_id).await {
            Ok(event) => event,
            Err(mergetip_store::StoreError::NotFound) => {
                return Err(EngineError::Malformed(format!(
                    "no stored delivery {delivery_id}"
                )));
            }
            Err(other) => return Err(other.into()),
        };
        if event.processed_at.is_some() {
            return Ok(());
        }

        match (event.event.as_str(), event.action.as_deref()) {
            ("issue_comment", Some("created")) => {
                self.handle_comment(parse::<IssueCommentEvent>(&event.payload)?)
                    .await?;
            }
            ("installation", _) => {
                self.handle_installation(parse::<InstallationEvent>(&event.payload)?)
                    .await?;
            }
            ("installation_repositories", _) => {
                self.handle_installation_repos(parse(&event.payload)?)
                    .await?;
            }
            ("pull_request", Some("closed")) => {
                // On-merge holds are resolved here from M4 on.
                tracing::debug!(delivery_id, "pull_request.closed received");
            }
            // Edited and deleted comments are ignored on purpose: replaying an edit must never
            // create a tip (FR-2.4). Everything else is not ours to act on.
            (name, action) => tracing::debug!(delivery_id, name, action, "event ignored"),
        }

        github_events::mark_processed(&self.pool, delivery_id, self.clock.now()).await?;
        Ok(())
    }

    async fn handle_installation(&self, event: InstallationEvent) -> Result<(), EngineError> {
        let id = GithubInstallationId::new(event.installation.id);
        let now = self.clock.now();
        let mut tx = self.pool.begin().await?;
        let login = || {
            event
                .installation
                .account
                .as_ref()
                .map(|a| a.login.clone())
                .ok_or_else(|| {
                    EngineError::Malformed("installation event without an account".into())
                })
        };
        // Only installing (again) or an explicit unsuspend may clear a suspension. Other actions,
        // such as `new_permissions_accepted`, arrive for suspended installations too.
        match event.action.as_str() {
            "deleted" => installations::delete(&mut tx, id, now).await?,
            "suspend" => installations::set_suspended(&mut *tx, id, Some(now)).await?,
            "created" => {
                let installation = installations::upsert(&mut *tx, id, &login()?).await?;
                if let Some(repos) = &event.repositories {
                    installations::set_repos(&mut tx, installation.id, &to_repos(repos)).await?;
                }
            }
            "unsuspend" => {
                installations::upsert(&mut *tx, id, &login()?).await?;
            }
            _ => {
                installations::ensure(&mut *tx, id, &login()?).await?;
            }
        }
        tx.commit().await?;
        tracing::info!(installation_id = %id, action = %event.action, "installation updated");
        Ok(())
    }

    async fn handle_installation_repos(
        &self,
        event: InstallationRepositoriesEvent,
    ) -> Result<(), EngineError> {
        let id = GithubInstallationId::new(event.installation.id);
        let mut tx = self.pool.begin().await?;
        let installation = match installations::get_by_github_id(&mut *tx, id).await {
            Ok(installation) => installation,
            Err(mergetip_store::StoreError::NotFound) => {
                let login = event
                    .installation
                    .account
                    .as_ref()
                    .map(|a| a.login.clone())
                    .unwrap_or_default();
                installations::upsert(&mut *tx, id, &login).await?
            }
            Err(other) => return Err(other.into()),
        };
        installations::add_repos(
            &mut *tx,
            installation.id,
            &to_repos(&event.repositories_added),
        )
        .await?;
        let removed: Vec<i64> = event.repositories_removed.iter().map(|r| r.id).collect();
        installations::remove_repos(&mut *tx, installation.id, &removed).await?;
        tx.commit().await?;
        Ok(())
    }
}

fn to_repos(repos: &[mergetip_github::webhook::EventRepo]) -> Vec<InstallationRepo> {
    repos
        .iter()
        .map(|r| InstallationRepo {
            github_repo_id: r.id,
            full_name: r.full_name.clone(),
        })
        .collect()
}

fn parse<T: DeserializeOwned>(payload: &serde_json::Value) -> Result<T, EngineError> {
    T::deserialize(payload)
        .map_err(|err| EngineError::Malformed(format!("unexpected payload shape: {err}")))
}
