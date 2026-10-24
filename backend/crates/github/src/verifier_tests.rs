//! Every verification rule and rejection, against an in-memory GitHub.

use mergetip_core::{GithubInstallationId, ThreadRef};
use serde_json::json;
use time::macros::datetime;

use super::*;
use crate::{
    AccountKind, CommitDetail, GitActor, Issue, IssueComment, PullRequest, Review, TimelineEvent,
    TimelineSource, testing::FakeGitHub,
};

const SENDER: i64 = 10;
const REPO: &str = "acme/widgets";
const NOW: OffsetDateTime = datetime!(2026-11-02 12:00 UTC);

fn thread(is_pr: bool) -> ThreadRef {
    ThreadRef {
        owner: "acme".into(),
        repo: "widgets".into(),
        number: 42,
        is_pr,
    }
}

fn github() -> FakeGitHub {
    let gh = FakeGitHub::new();
    gh.add_user(FakeGitHub::account(SENDER, "maintainer"));
    gh.add_user(FakeGitHub::account(11, "alice"));
    gh.state().pulls.insert(
        (REPO.into(), 42),
        PullRequest {
            number: 42,
            user: Some(FakeGitHub::account(12, "author")),
            state: "open".into(),
            merged: Some(false),
            merged_at: None,
        },
    );
    gh.state().issues.insert(
        (REPO.into(), 42),
        Issue {
            number: 42,
            user: Some(FakeGitHub::account(12, "author")),
            pull_request: None,
        },
    );
    gh
}

async fn check(
    gh: &FakeGitHub,
    is_pr: bool,
    receiver: &str,
    on_merge: bool,
) -> Result<Value, VerifyError> {
    let thread = thread(is_pr);
    let request = VerifyRequest {
        installation: GithubInstallationId::new(1),
        thread: &thread,
        sender_github_id: SENDER,
        receiver_login: receiver,
        on_merge,
        now: NOW,
    };
    verify(gh, &request).await.map(|v| v.evidence)
}

fn rejected(rejection: Rejection) -> Result<Value, VerifyError> {
    Err(VerifyError::Rejected(rejection))
}

fn review(id: i64, user: i64, state: &str) -> Review {
    Review {
        id,
        user: Some(FakeGitHub::account(user, "someone")),
        state: state.into(),
    }
}

fn commit(sha: &str, author: Option<i64>, message: &str) -> Commit {
    Commit {
        sha: sha.into(),
        author: author.map(|id| FakeGitHub::account(id, "someone")),
        committer: None,
        commit: CommitDetail {
            message: message.into(),
            author: Some(GitActor {
                name: Some("Someone".into()),
                email: Some("someone@example.com".into()),
            }),
            committer: None,
        },
    }
}

#[tokio::test]
async fn identity_checks() {
    let gh = github();
    let mut bot = FakeGitHub::account(20, "helper[bot]");
    bot.kind = AccountKind::Bot;
    gh.add_user(bot);
    let mut fresh = FakeGitHub::account(21, "fresh");
    fresh.created_at = Some(NOW - time::Duration::days(29));
    gh.add_user(fresh);

    assert_eq!(
        check(&gh, true, "ghost", false).await,
        rejected(Rejection::UnknownUser)
    );
    assert_eq!(
        check(&gh, true, "Maintainer", false).await,
        rejected(Rejection::SelfTip)
    );
    assert_eq!(
        check(&gh, true, "helper[bot]", false).await,
        rejected(Rejection::BotReceiver)
    );
    assert_eq!(
        check(&gh, true, "fresh", false).await,
        rejected(Rejection::AccountTooNew)
    );
}

#[tokio::test]
async fn pr_author_passes() {
    let gh = github();
    gh.add_user(FakeGitHub::account(12, "author"));
    assert_eq!(
        check(&gh, true, "author", true).await,
        Ok(json!({ "rule": "pr_author", "pr": 42 }))
    );
}

#[tokio::test]
async fn submitted_review_passes_but_pending_does_not() {
    let gh = github();
    gh.state()
        .reviews
        .insert((REPO.into(), 42), vec![review(1, 11, "PENDING")]);
    assert_eq!(
        check(&gh, true, "alice", false).await,
        rejected(Rejection::NoContribution)
    );

    gh.state()
        .reviews
        .get_mut(&(REPO.into(), 42))
        .unwrap()
        .push(review(2, 11, "COMMENTED"));
    assert_eq!(
        check(&gh, true, "alice", false).await,
        Ok(json!({ "rule": "pr_review", "review_id": 2 }))
    );
}

#[tokio::test]
async fn on_merge_tips_need_code_not_a_review() {
    let gh = github();
    gh.state()
        .reviews
        .insert((REPO.into(), 42), vec![review(2, 11, "APPROVED")]);
    assert_eq!(
        check(&gh, true, "alice", true).await,
        rejected(Rejection::NoContribution)
    );

    gh.state()
        .commits
        .insert((REPO.into(), 42), vec![commit("c1", Some(11), "Fix")]);
    assert_eq!(
        check(&gh, true, "alice", true).await,
        Ok(json!({ "rule": "commit_author", "sha": "c1" }))
    );
}

#[tokio::test]
async fn co_authors_pass() {
    let gh = github();
    gh.state().commits.insert(
        (REPO.into(), 42),
        vec![commit(
            "c9",
            Some(12),
            "Pair on it\n\nCo-authored-by: Alice <11+alice@users.noreply.github.com>",
        )],
    );
    assert_eq!(
        check(&gh, true, "alice", true).await,
        Ok(json!({ "rule": "commit_co_author", "sha": "c9" }))
    );
}

#[tokio::test]
async fn issue_rules() {
    let gh = github();
    gh.add_user(FakeGitHub::account(12, "author"));
    gh.add_user(FakeGitHub::account(13, "fixer"));
    gh.add_user(FakeGitHub::account(14, "mentioner"));
    gh.state().comments.insert(
        (REPO.into(), 42),
        vec![IssueComment {
            id: 77,
            user: Some(FakeGitHub::account(11, "alice")),
            body: Some("Repro steps…".into()),
        }],
    );
    let cross_ref = |number, user, is_pr: bool| TimelineEvent {
        event: Some("cross-referenced".into()),
        source: Some(TimelineSource {
            issue: Some(Issue {
                number,
                user: Some(FakeGitHub::account(user, "x")),
                pull_request: is_pr.then(|| json!({})),
            }),
        }),
    };
    gh.state().timeline.insert(
        (REPO.into(), 42),
        vec![cross_ref(50, 14, false), cross_ref(51, 13, true)],
    );

    assert_eq!(
        check(&gh, false, "author", false).await,
        Ok(json!({ "rule": "issue_author", "issue": 42 }))
    );
    assert_eq!(
        check(&gh, false, "alice", false).await,
        Ok(json!({ "rule": "issue_comment", "comment_id": 77 }))
    );
    assert_eq!(
        check(&gh, false, "fixer", false).await,
        Ok(json!({ "rule": "linked_pr", "pr": 51 }))
    );
    assert_eq!(
        check(&gh, false, "mentioner", false).await,
        rejected(Rejection::NoContribution),
        "a cross-reference from another issue is not a contribution"
    );
}

#[tokio::test]
async fn github_failures_are_errors_not_verdicts() {
    let gh = github();
    gh.state().fail_next = Some(GitHubError::Http {
        status: 502,
        message: "Bad Gateway".into(),
    });
    let result = check(&gh, true, "alice", false).await;
    assert!(
        matches!(result, Err(VerifyError::GitHub(ref e)) if e.is_retryable()),
        "{result:?}"
    );
}
