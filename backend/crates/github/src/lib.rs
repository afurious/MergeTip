//! GitHub integration: the [`GitHubApi`] trait the rest of the backend codes against, its
//! octocrab implementation, webhook payload types, the verifier (FR-3) and bot replies
//! (FR-1.4, FR-1.5).

mod api;
mod client;
mod error;
pub mod replies;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod verifier;
pub mod webhook;

pub use api::{
    Account, AccountKind, Commit, CommitDetail, GitActor, GitHubApi, Issue, IssueComment,
    PostedComment, PullRequest, Reaction, RepoName, Review, TimelineEvent, TimelineSource,
};
pub use client::OctocrabGitHub;
pub use error::GitHubError;
