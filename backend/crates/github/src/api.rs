//! The GitHub operations MergeTip needs, as a trait so verification and replies can be tested
//! against fakes, plus the response types (only the fields we read).

use std::{fmt, str::FromStr};

use async_trait::async_trait;
use mergetip_core::GithubInstallationId;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::GitHubError;

/// `owner/name`, validated so it can be interpolated into an API path safely.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoName {
    owner: String,
    name: String,
}

impl RepoName {
    pub fn new(owner: &str, name: &str) -> Result<Self, GitHubError> {
        let valid = |part: &str| {
            !part.is_empty()
                && part.len() <= 100
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if valid(owner) && valid(name) {
            Ok(Self {
                owner: owner.to_owned(),
                name: name.to_owned(),
            })
        } else {
            Err(GitHubError::InvalidInput(format!(
                "invalid repository {owner}/{name}"
            )))
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl FromStr for RepoName {
    type Err = GitHubError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (owner, name) = s
            .split_once('/')
            .ok_or_else(|| GitHubError::InvalidInput(format!("invalid repository {s}")))?;
        Self::new(owner, name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum AccountKind {
    User,
    Bot,
    Organization,
    #[serde(other)]
    Other,
}

/// A GitHub account as it appears in API responses and webhook payloads. Fields other than
/// `id`, `login` and `type` are only present on some endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Account {
    pub id: i64,
    pub login: String,
    #[serde(rename = "type")]
    pub kind: AccountKind,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub email: Option<String>,
}

impl Account {
    /// Bots can't receive tips: covers App bot accounts and the `[bot]` login convention.
    pub fn is_bot(&self) -> bool {
        self.kind == AccountKind::Bot || self.login.ends_with("[bot]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequest {
    pub number: u32,
    pub user: Option<Account>,
    pub state: String,
    #[serde(default)]
    pub merged: Option<bool>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub merged_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Review {
    pub id: i64,
    pub user: Option<Account>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitActor {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommitDetail {
    pub message: String,
    pub author: Option<GitActor>,
    pub committer: Option<GitActor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Commit {
    pub sha: String,
    /// The GitHub account matched to the commit author email; `None` when the email isn't
    /// linked to any account.
    pub author: Option<Account>,
    pub committer: Option<Account>,
    pub commit: CommitDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    pub number: u32,
    pub user: Option<Account>,
    /// Present (with link fields we don't read) when the issue is a pull request.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

impl Issue {
    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueComment {
    pub id: i64,
    pub user: Option<Account>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineSource {
    pub issue: Option<Issue>,
}

/// An issue timeline entry; we only read cross-references from pull requests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineEvent {
    pub event: Option<String>,
    #[serde(default)]
    pub source: Option<TimelineSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PostedComment {
    pub id: i64,
    pub html_url: String,
}

/// Reactions the bot puts on a `/tip` comment (FR-1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reaction {
    /// Seen and accepted.
    Eyes,
    /// Paid.
    Rocket,
    /// Rejected or failed.
    Confused,
}

impl Reaction {
    /// The `content` value GitHub's reactions API expects.
    pub const fn content(self) -> &'static str {
        match self {
            Self::Eyes => "eyes",
            Self::Rocket => "rocket",
            Self::Confused => "confused",
        }
    }
}

/// Every call acts as one App installation, with that installation's token and rate limit.
#[async_trait]
pub trait GitHubApi: Send + Sync {
    async fn get_user(
        &self,
        installation: GithubInstallationId,
        login: &str,
    ) -> Result<Account, GitHubError>;

    async fn get_pull(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<PullRequest, GitHubError>;

    async fn list_pull_reviews(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<Review>, GitHubError>;

    async fn list_pull_commits(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<Commit>, GitHubError>;

    async fn get_issue(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Issue, GitHubError>;

    async fn list_issue_comments(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<IssueComment>, GitHubError>;

    async fn list_issue_timeline(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<TimelineEvent>, GitHubError>;

    async fn create_comment(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
        body: &str,
    ) -> Result<PostedComment, GitHubError>;

    async fn update_comment(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        comment_id: i64,
        body: &str,
    ) -> Result<(), GitHubError>;

    async fn create_reaction(
        &self,
        installation: GithubInstallationId,
        repo: &RepoName,
        comment_id: i64,
        reaction: Reaction,
    ) -> Result<(), GitHubError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_are_validated_for_path_safety() {
        assert_eq!(
            "acme/widgets.rs".parse::<RepoName>().unwrap().to_string(),
            "acme/widgets.rs"
        );
        for bad in [
            "acme",
            "acme/",
            "/x",
            "acme/../x",
            "a/b/c",
            "acme/wid gets",
            "acme/..",
        ] {
            assert!(bad.parse::<RepoName>().is_err(), "{bad}");
        }
    }

    #[test]
    fn bots_are_detected_by_type_or_login() {
        let account = |login: &str, kind| Account {
            id: 1,
            login: login.into(),
            kind,
            avatar_url: None,
            created_at: None,
            email: None,
        };
        assert!(account("dependabot[bot]", AccountKind::Bot).is_bot());
        assert!(account("renovate[bot]", AccountKind::User).is_bot());
        assert!(!account("alice", AccountKind::User).is_bot());
    }
}
