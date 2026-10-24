//! An in-memory [`GitHubApi`] for tests in any crate (enable the `testing` feature).
//!
//! Seed it with fixtures, run the code under test, then inspect what was posted.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use mergetip_core::GithubInstallationId;

use crate::{
    Account, AccountKind, Commit, GitHubApi, GitHubError, Issue, IssueComment, PostedComment,
    PullRequest, Reaction, RepoName, Review, TimelineEvent,
};

/// A comment the code under test created or edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenComment {
    pub repo: String,
    pub number: u32,
    pub comment_id: i64,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct State {
    pub users: HashMap<String, Account>,
    pub pulls: HashMap<(String, u32), PullRequest>,
    pub reviews: HashMap<(String, u32), Vec<Review>>,
    pub commits: HashMap<(String, u32), Vec<Commit>>,
    pub issues: HashMap<(String, u32), Issue>,
    pub comments: HashMap<(String, u32), Vec<IssueComment>>,
    pub timeline: HashMap<(String, u32), Vec<TimelineEvent>>,
    /// Comments created, in order. Edits replace the body in place.
    pub posted: Vec<WrittenComment>,
    /// Every edit, in order (comment id, new body).
    pub edits: Vec<(i64, String)>,
    pub reactions: Vec<(i64, Reaction)>,
    /// Returned (once) by the next call of any method, to simulate outages.
    pub fail_next: Option<GitHubError>,
    next_comment_id: i64,
}

#[derive(Debug, Default)]
pub struct FakeGitHub {
    state: Mutex<State>,
}

impl FakeGitHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A regular user account created long ago.
    pub fn account(id: i64, login: &str) -> Account {
        Account {
            id,
            login: login.to_owned(),
            kind: AccountKind::User,
            avatar_url: Some(format!("https://avatars.example/{id}")),
            created_at: Some(time::macros::datetime!(2020-01-01 0:00 UTC)),
            email: None,
        }
    }

    pub fn add_user(&self, account: Account) {
        self.state()
            .users
            .insert(account.login.to_lowercase(), account);
    }

    pub fn posted_bodies(&self) -> Vec<String> {
        self.state().posted.iter().map(|c| c.body.clone()).collect()
    }

    fn take_failure(&self) -> Result<MutexGuard<'_, State>, GitHubError> {
        let mut state = self.state();
        match state.fail_next.take() {
            Some(err) => Err(err),
            None => Ok(state),
        }
    }
}

fn key(repo: &RepoName, number: u32) -> (String, u32) {
    (repo.to_string(), number)
}

#[async_trait]
impl GitHubApi for FakeGitHub {
    async fn get_user(
        &self,
        _installation: GithubInstallationId,
        login: &str,
    ) -> Result<Account, GitHubError> {
        let state = self.take_failure()?;
        state
            .users
            .get(&login.to_lowercase())
            .cloned()
            .ok_or(GitHubError::NotFound)
    }

    async fn get_pull(
        &self,
        _installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<PullRequest, GitHubError> {
        let state = self.take_failure()?;
        state
            .pulls
            .get(&key(repo, number))
            .cloned()
            .ok_or(GitHubError::NotFound)
    }

    async fn list_pull_reviews(
        &self,
        _installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<Review>, GitHubError> {
        let state = self.take_failure()?;
        Ok(state
            .reviews
            .get(&key(repo, number))
            .cloned()
            .unwrap_or_default())
    }

    async fn list_pull_commits(
        &self,
        _installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<Commit>, GitHubError> {
        let state = self.take_failure()?;
        Ok(state
            .commits
            .get(&key(repo, number))
            .cloned()
            .unwrap_or_default())
    }

    async fn get_issue(
        &self,
        _installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Issue, GitHubError> {
        let state = self.take_failure()?;
        state
            .issues
            .get(&key(repo, number))
            .cloned()
            .ok_or(GitHubError::NotFound)
    }

    async fn list_issue_comments(
        &self,
        _installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<IssueComment>, GitHubError> {
        let state = self.take_failure()?;
        Ok(state
            .comments
            .get(&key(repo, number))
            .cloned()
            .unwrap_or_default())
    }

    async fn list_issue_timeline(
        &self,
        _installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
    ) -> Result<Vec<TimelineEvent>, GitHubError> {
        let state = self.take_failure()?;
        Ok(state
            .timeline
            .get(&key(repo, number))
            .cloned()
            .unwrap_or_default())
    }

    async fn create_comment(
        &self,
        _installation: GithubInstallationId,
        repo: &RepoName,
        number: u32,
        body: &str,
    ) -> Result<PostedComment, GitHubError> {
        let mut state = self.take_failure()?;
        state.next_comment_id += 1;
        let id = 9_000 + state.next_comment_id;
        state.posted.push(WrittenComment {
            repo: repo.to_string(),
            number,
            comment_id: id,
            body: body.to_owned(),
        });
        Ok(PostedComment {
            id,
            html_url: format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}"),
        })
    }

    async fn update_comment(
        &self,
        _installation: GithubInstallationId,
        _repo: &RepoName,
        comment_id: i64,
        body: &str,
    ) -> Result<(), GitHubError> {
        let mut state = self.take_failure()?;
        let comment = state
            .posted
            .iter_mut()
            .find(|c| c.comment_id == comment_id)
            .ok_or(GitHubError::NotFound)?;
        comment.body = body.to_owned();
        state.edits.push((comment_id, body.to_owned()));
        Ok(())
    }

    async fn create_reaction(
        &self,
        _installation: GithubInstallationId,
        _repo: &RepoName,
        comment_id: i64,
        reaction: Reaction,
    ) -> Result<(), GitHubError> {
        let mut state = self.take_failure()?;
        state.reactions.push((comment_id, reaction));
        Ok(())
    }
}
