//! [`GitHubApi`] over octocrab, authenticated as the GitHub App.
//!
//! octocrab caches an installation token inside the `Octocrab` instance for that installation,
//! so instances are kept per installation; recreating one per call would mint a fresh token
//! every time. octocrab's own retries are disabled: rate limits and server errors surface as
//! [`GitHubError`]s and the job queue retries them with backoff (or at the reset time).

use std::{collections::HashMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use mergetip_core::GithubInstallationId;
use octocrab::{Octocrab, service::middleware::retry::RetryConfig};
use serde::de::DeserializeOwned;
use serde_json::json;
use time::OffsetDateTime;

use crate::{
    Account, Commit, GitHubApi, GitHubError, Issue, IssueComment, PostedComment, PullRequest,
    Reaction, RepoName, Review, TimelineEvent,
};

const PER_PAGE: usize = 100;
/// Bounds the cost of one listing. A thread with more than 1,000 reviews, commits or comments is
/// rare; past this, verification works from the first 1,000 and logs a warning.
const MAX_PAGES: usize = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

enum Auth {
    /// Production: App JWT, then per-installation tokens.
    App(Octocrab),
    /// Tests and local mocks: one fixed token for every installation.
    Token(Octocrab),
}

pub struct OctocrabGitHub {
    auth: Auth,
    installations: Mutex<HashMap<i64, Octocrab>>,
}

impl std::fmt::Debug for OctocrabGitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OctocrabGitHub").finish_non_exhaustive()
    }
}

impl OctocrabGitHub {
    /// Authenticates as the App with its RSA private key (PEM). `base_uri` overrides
    /// `https://api.github.com` (GitHub Enterprise, local mocks).
    pub fn from_app_key(
        app_id: u64,
        private_key_pem: &[u8],
        base_uri: Option<&str>,
    ) -> Result<Self, GitHubError> {
        let builder = builder(base_uri)?
            .app_from_rsa_pem(app_id.into(), private_key_pem)
            .map_err(|err| GitHubError::InvalidInput(format!("invalid App private key: {err}")))?;
        let app = builder.build().map_err(transport)?;
        Ok(Self {
            auth: Auth::App(app),
            installations: Mutex::new(HashMap::new()),
        })
    }

    /// Uses one bearer token for every installation. For tests and local mock servers only.
    pub fn with_token(token: &str, base_uri: &str) -> Result<Self, GitHubError> {
        let client = builder(Some(base_uri))?
            .personal_token(token.to_owned())
            .build()
            .map_err(transport)?;
        Ok(Self {
            auth: Auth::Token(client),
            installations: Mutex::new(HashMap::new()),
        })
    }

    fn client(&self, installation: GithubInstallationId) -> Result<Octocrab, GitHubError> {
        match &self.auth {
            Auth::Token(client) => Ok(client.clone()),
            Auth::App(app) => {
                let mut cache = self
                    .installations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(client) = cache.get(&installation.get()) {
                    return Ok(client.clone());
                }
                let id = u64::try_from(installation.get())
                    .map_err(|_| GitHubError::InvalidInput("negative installation id".into()))?;
                let client = app.installation(id.into()).map_err(transport)?;
                cache.insert(installation.get(), client.clone());
                Ok(client)
            }
        }
    }

    async fn get<T: DeserializeOwned>(
        &self,
        installation: GithubInstallationId,
        route: &str,
    ) -> Result<T, GitHubError> {
        let client = self.client(installation)?;
        client.get(route, None::<&()>).await.map_err(map_error)
    }

    /// Fetches up to [`MAX_PAGES`] pages of a list endpoint.
    async fn list<T: DeserializeOwned>(
        &self,
        installation: GithubInstallationId,
        route: &str,
    ) -> Result<Vec<T>, GitHubError> {
        let client = self.client(installation)?;
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{route}?per_page={PER_PAGE}&page={page}");
            let batch: Vec<T> = client.get(url, None::<&()>).await.map_err(map_error)?;
            let full = batch.len() == PER_PAGE;
            items.extend(batch);
            if !full {
                return Ok(items);
            }
        }
        tracing::warn!(
            route,
            pages = MAX_PAGES,
            "listing truncated at the page limit"
        );
        Ok(items)
    }
}

fn builder(
    base_uri: Option<&str>,
) -> Result<
    octocrab::OctocrabBuilder<
        octocrab::NoSvc,
        octocrab::DefaultOctocrabBuilderConfig,
        octocrab::NoAuth,
        octocrab::NotLayerReady,
    >,
    GitHubError,
> {
    let mut builder = Octocrab::builder()
        .add_retry_config(RetryConfig::None)
        .set_connect_timeout(Some(CONNECT_TIMEOUT))
        .set_read_timeout(Some(READ_TIMEOUT));
    if let Some(uri) = base_uri {
        builder = builder
            .base_uri(uri)
            .map_err(|err| GitHubError::InvalidInput(format!("invalid GitHub API URL: {err}")))?;
    }
    Ok(builder)
}

fn transport(err: octocrab::Error) -> GitHubError {
    GitHubError::Transport(err.to_string())
}

fn map_error(err: octocrab::Error) -> GitHubError {
    match err {
        octocrab::Error::GitHub { source, .. } => {
            let status = source.status_code.as_u16();
            let header = |name: &str| {
                source
                    .headers
                    .as_ref()
                    .and_then(|h| h.get(name))
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            let exhausted = header("x-ratelimit-remaining").as_deref() == Some("0");
            let retry_after = header("retry-after").and_then(|v| v.parse::<i64>().ok());
            match status {
                403 | 429 if exhausted || retry_after.is_some() || status == 429 => {
                    let now = OffsetDateTime::now_utc();
                    let reset_at = match (retry_after, source.rate_limit_reset) {
                        (Some(secs), _) => now + time::Duration::seconds(secs),
                        (None, Some(reset)) => i64::try_from(reset)
                            .ok()
                            .and_then(|r| OffsetDateTime::from_unix_timestamp(r).ok())
                            .unwrap_or(now + time::Duration::minutes(1)),
                        (None, None) => now + time::Duration::minutes(1),
                    };
                    GitHubError::RateLimited { reset_at }
                }
                401 => GitHubError::Unauthorized,
                404 => GitHubError::NotFound,
                _ => GitHubError::Http {
                    status,
                    message: source.message,
                },
            }
        }
        octocrab::Error::Serde { source, .. } => GitHubError::Decode(source.to_string()),
        octocrab::Error::Json { source, .. } => GitHubError::Decode(source.to_string()),
        other => GitHubError::Transport(other.to_string()),
    }
}

/// Logins come from comment text and webhook payloads; only GitHub's login characters may reach
/// an API path.
fn login_path(login: &str) -> Result<&str, GitHubError> {
    let valid = !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if valid {
        Ok(login)
    } else {
        Err(GitHubError::InvalidInput(format!(
            "invalid login {login:?}"
        )))
    }
}

#[async_trait]
impl GitHubApi for OctocrabGitHub {
    async fn get_user(
        &self,
        installation: GithubInstallationId,
        login: &str,
    ) -> Result<Account, GitHubError> {
        let login = login_path(login)?;
        self.get(installation, &format!("/users/{login}")).await
    }

    async fn get_pull(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<PullRequest, GitHubError> {
        self.get(installation, &format!("/repos/{repo}/pulls/{number}"))
            .await
    }

    async fn list_pull_reviews(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<Review>, GitHubError> {
        self.list(
            installation,
            &format!("/repos/{repo}/pulls/{number}/reviews"),
        )
        .await
    }

    async fn list_pull_commits(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<Commit>, GitHubError> {
        self.list(
            installation,
            &format!("/repos/{repo}/pulls/{number}/commits"),
        )
        .await
    }

    async fn get_issue(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Issue, GitHubError> {
        self.get(installation, &format!("/repos/{repo}/issues/{number}"))
            .await
    }

    async fn list_issue_comments(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<IssueComment>, GitHubError> {
        self.list(
            installation,
            &format!("/repos/{repo}/issues/{number}/comments"),
        )
        .await
    }

    async fn list_issue_timeline(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<TimelineEvent>, GitHubError> {
        self.list(
            installation,
            &format!("/repos/{repo}/issues/{number}/timeline"),
        )
        .await
    }

    async fn create_comment(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
        body: &str,
    ) -> Result<PostedComment, GitHubError> {
        let client = self.client(installation)?;
        client
            .post(
                format!("/repos/{repo}/issues/{number}/comments"),
                Some(&json!({ "body": body })),
            )
            .await
            .map_err(map_error)
    }

    async fn update_comment(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        comment_id: i64,
        body: &str,
    ) -> Result<(), GitHubError> {
        let client = self.client(installation)?;
        let _: serde_json::Value = client
            .patch(
                format!("/repos/{repo}/issues/comments/{comment_id}"),
                Some(&json!({ "body": body })),
            )
            .await
            .map_err(map_error)?;
        Ok(())
    }

    async fn create_reaction(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        comment_id: i64,
        reaction: Reaction,
    ) -> Result<(), GitHubError> {
        let client = self.client(installation)?;
        let _: serde_json::Value = client
            .post(
                format!("/repos/{repo}/issues/comments/{comment_id}/reactions"),
                Some(&json!({ "content": reaction.content() })),
            )
            .await
            .map_err(map_error)?;
        Ok(())
    }
}
