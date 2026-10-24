//! Checks that the receiver actually did the work before money moves (FR-3, PRD "Verification
//! rules").
//!
//! The verifier returns a verdict ([`Verification`] or [`Rejection`]) only when GitHub answered.
//! Any GitHub failure is returned as [`VerifyError::GitHub`] so the caller retries: an outage must
//! never turn into "the receiver didn't contribute".

use mergetip_core::{GithubInstallationId, ThreadRef};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

use crate::{Account, Commit, GitHubApi, GitHubError, RepoName};

/// Receivers must have had their GitHub account this long (Sybil resistance).
pub const MIN_ACCOUNT_AGE: Duration = Duration::days(30);

#[derive(Debug, Clone)]
pub struct VerifyRequest<'a> {
    pub installation: GithubInstallationId,
    pub thread: &'a ThreadRef,
    /// From the signed webhook payload, never from comment text.
    pub sender_github_id: i64,
    pub receiver_login: &'a str,
    pub on_merge: bool,
    pub now: OffsetDateTime,
}

/// A passed check: who the receiver is and what proves they contributed.
#[derive(Debug, Clone, PartialEq)]
pub struct Verification {
    pub receiver: Account,
    /// Stored in `tips.verified_by`, e.g. `{"rule":"pr_review","review_id":123}`.
    pub evidence: Value,
}

/// Why a tip can't be paid. Each has a stable code stored in `tips.error_code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    UnknownUser,
    SelfTip,
    BotReceiver,
    AccountTooNew,
    NoContribution,
}

impl Rejection {
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnknownUser => "unknown_user",
            Self::SelfTip => "self_tip",
            Self::BotReceiver => "bot_receiver",
            Self::AccountTooNew => "account_too_new",
            Self::NoContribution => "no_contribution",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("tip rejected: {}", .0.code())]
    Rejected(Rejection),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
}

/// Runs the identity checks, then the contribution rules for the thread type.
pub async fn verify(
    github: &dyn GitHubApi,
    request: &VerifyRequest<'_>,
) -> Result<Verification, VerifyError> {
    let receiver = match github
        .get_user(request.installation, request.receiver_login)
        .await
    {
        Ok(user) => user,
        Err(GitHubError::NotFound) => return Err(VerifyError::Rejected(Rejection::UnknownUser)),
        Err(other) => return Err(other.into()),
    };
    if receiver.id == request.sender_github_id {
        return Err(VerifyError::Rejected(Rejection::SelfTip));
    }
    if receiver.is_bot() {
        return Err(VerifyError::Rejected(Rejection::BotReceiver));
    }
    if receiver
        .created_at
        .is_some_and(|created| request.now - created < MIN_ACCOUNT_AGE)
    {
        return Err(VerifyError::Rejected(Rejection::AccountTooNew));
    }

    let repo = RepoName::new(&request.thread.owner, &request.thread.repo)?;
    let evidence = if request.thread.is_pr {
        pull_request_evidence(github, request, &repo, &receiver).await?
    } else {
        issue_evidence(github, request, &repo, &receiver).await?
    };
    match evidence {
        Some(evidence) => Ok(Verification { receiver, evidence }),
        None => Err(VerifyError::Rejected(Rejection::NoContribution)),
    }
}

async fn pull_request_evidence(
    github: &dyn GitHubApi,
    request: &VerifyRequest<'_>,
    repo: &RepoName,
    receiver: &Account,
) -> Result<Option<Value>, GitHubError> {
    let number = request.thread.number;
    let pull = github.get_pull(request.installation, repo, number).await?;
    if pull.user.as_ref().is_some_and(|u| u.id == receiver.id) {
        return Ok(Some(json!({ "rule": "pr_author", "pr": number })));
    }

    // Reviews count for an immediate tip only; an on-merge tip rewards the code that merges.
    if !request.on_merge {
        let reviews = github
            .list_pull_reviews(request.installation, repo, number)
            .await?;
        if let Some(review) = reviews
            .iter()
            .find(|r| r.state != "PENDING" && r.user.as_ref().is_some_and(|u| u.id == receiver.id))
        {
            return Ok(Some(json!({ "rule": "pr_review", "review_id": review.id })));
        }
    }

    let commits = github
        .list_pull_commits(request.installation, repo, number)
        .await?;
    if let Some(commit) = commits.iter().find(|c| authored_by(c, receiver)) {
        return Ok(Some(json!({ "rule": "commit_author", "sha": commit.sha })));
    }
    if let Some(commit) = commits.iter().find(|c| co_authored_by(c, receiver)) {
        return Ok(Some(
            json!({ "rule": "commit_co_author", "sha": commit.sha }),
        ));
    }
    Ok(None)
}

async fn issue_evidence(
    github: &dyn GitHubApi,
    request: &VerifyRequest<'_>,
    repo: &RepoName,
    receiver: &Account,
) -> Result<Option<Value>, GitHubError> {
    let number = request.thread.number;
    let issue = github.get_issue(request.installation, repo, number).await?;
    if issue.user.as_ref().is_some_and(|u| u.id == receiver.id) {
        return Ok(Some(json!({ "rule": "issue_author", "issue": number })));
    }

    let comments = github
        .list_issue_comments(request.installation, repo, number)
        .await?;
    if let Some(comment) = comments
        .iter()
        .find(|c| c.user.as_ref().is_some_and(|u| u.id == receiver.id))
    {
        return Ok(Some(
            json!({ "rule": "issue_comment", "comment_id": comment.id }),
        ));
    }

    let timeline = github
        .list_issue_timeline(request.installation, repo, number)
        .await?;
    let linked_pr = timeline
        .iter()
        .filter(|e| e.event.as_deref() == Some("cross-referenced"))
        .filter_map(|e| e.source.as_ref()?.issue.as_ref())
        .find(|source| {
            source.is_pull_request() && source.user.as_ref().is_some_and(|u| u.id == receiver.id)
        });
    Ok(linked_pr.map(|pr| json!({ "rule": "linked_pr", "pr": pr.number })))
}

fn authored_by(commit: &Commit, receiver: &Account) -> bool {
    [commit.author.as_ref(), commit.committer.as_ref()]
        .into_iter()
        .flatten()
        .any(|account| account.id == receiver.id)
        || commit
            .commit
            .author
            .as_ref()
            .and_then(|a| a.email.as_deref())
            .is_some_and(|email| email_belongs_to(email, receiver))
}

fn co_authored_by(commit: &Commit, receiver: &Account) -> bool {
    co_author_emails(&commit.commit.message).any(|email| email_belongs_to(email, receiver))
}

/// Emails from `Co-authored-by: Name <email>` trailers.
fn co_author_emails(message: &str) -> impl Iterator<Item = &str> {
    message.lines().filter_map(|line| {
        let line = line.trim();
        let (key, value) = line.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("co-authored-by") {
            return None;
        }
        let start = value.find('<')?;
        let end = value[start..].find('>')? + start;
        Some(value[start + 1..end].trim())
    })
}

/// An email is the receiver's if it is their public email or one of GitHub's noreply forms
/// (`login@users.noreply.github.com` or `id+login@users.noreply.github.com`).
fn email_belongs_to(email: &str, receiver: &Account) -> bool {
    let email = email.to_ascii_lowercase();
    let login = receiver.login.to_ascii_lowercase();
    if receiver
        .email
        .as_deref()
        .is_some_and(|public| public.eq_ignore_ascii_case(&email))
    {
        return true;
    }
    let Some(local) = email.strip_suffix("@users.noreply.github.com") else {
        return false;
    };
    local == login || local == format!("{}+{login}", receiver.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountKind, CommitDetail, GitActor};

    fn alice() -> Account {
        Account {
            id: 11,
            login: "Alice".into(),
            kind: AccountKind::User,
            avatar_url: None,
            created_at: None,
            email: Some("alice@example.com".into()),
        }
    }

    fn commit(message: &str, author_email: &str) -> Commit {
        Commit {
            sha: "abc".into(),
            author: None,
            committer: None,
            commit: CommitDetail {
                message: message.into(),
                author: Some(GitActor {
                    name: Some("x".into()),
                    email: Some(author_email.into()),
                }),
                committer: None,
            },
        }
    }

    #[test]
    fn co_author_trailers_match_public_and_noreply_emails() {
        let receiver = alice();
        for trailer in [
            "Co-authored-by: Alice <alice@example.com>",
            "co-authored-by: A <11+alice@users.noreply.github.com>",
            "CO-AUTHORED-BY:A<ALICE@users.noreply.github.com>",
        ] {
            let c = commit(&format!("Fix parser\n\n{trailer}"), "bob@example.com");
            assert!(co_authored_by(&c, &receiver), "{trailer}");
        }
        for trailer in [
            "Co-authored-by: Mallory <12+alice@users.noreply.github.com>",
            "Signed-off-by: Alice <alice@example.com>",
            "Co-authored-by: Alice alice@example.com",
        ] {
            let c = commit(&format!("Fix\n\n{trailer}"), "bob@example.com");
            assert!(!co_authored_by(&c, &receiver), "{trailer}");
        }
    }

    #[test]
    fn unlinked_commit_author_matches_by_email() {
        assert!(authored_by(&commit("x", "Alice@Example.com"), &alice()));
        assert!(!authored_by(&commit("x", "bob@example.com"), &alice()));
    }
}

#[cfg(test)]
#[path = "verifier_tests.rs"]
mod rule_tests;
