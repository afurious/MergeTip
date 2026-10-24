//! Webhook payloads, reduced to the fields MergeTip reads.
//!
//! Signature verification happens in the API before anything is stored
//! (`mergetip_crypto::verify_github_signature`); these types only parse already-trusted payloads.

use serde::Deserialize;

use crate::{Account, Issue};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventInstallation {
    pub id: i64,
    /// Present on `installation` events, absent on most others.
    #[serde(default)]
    pub account: Option<Account>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventRepository {
    pub id: i64,
    pub name: String,
    pub full_name: String,
    pub owner: Account,
}

/// A repository as listed in installation events (no owner object).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventRepo {
    pub id: i64,
    pub full_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventComment {
    pub id: i64,
    #[serde(default)]
    pub body: Option<String>,
    /// The comment author. GitHub sets this from the authenticated session that posted the
    /// comment, so it is the only trustworthy identity for the sender (FR-1.3).
    pub user: Account,
    pub html_url: String,
}

/// `issue_comment` (on issues and on pull requests).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueCommentEvent {
    pub action: String,
    pub comment: EventComment,
    pub issue: Issue,
    pub repository: EventRepository,
    pub installation: Option<EventInstallation>,
}

/// `installation`: created, deleted, suspend, unsuspend, new_permissions_accepted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstallationEvent {
    pub action: String,
    pub installation: EventInstallation,
    #[serde(default)]
    pub repositories: Option<Vec<EventRepo>>,
}

/// `installation_repositories`: added or removed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstallationRepositoriesEvent {
    pub action: String,
    pub installation: EventInstallation,
    #[serde(default)]
    pub repositories_added: Vec<EventRepo>,
    #[serde(default)]
    pub repositories_removed: Vec<EventRepo>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::AccountKind;

    #[test]
    fn parses_an_issue_comment_on_a_pull_request() {
        let event: IssueCommentEvent = serde_json::from_value(json!({
            "action": "created",
            "comment": {
                "id": 1001, "body": "/tip @alice 2100",
                "html_url": "https://github.com/acme/widgets/pull/42#issuecomment-1001",
                "user": { "id": 10, "login": "maintainer", "type": "User" }
            },
            "issue": {
                "number": 42,
                "user": { "id": 11, "login": "alice", "type": "User" },
                "pull_request": { "url": "https://api.github.com/repos/acme/widgets/pulls/42" }
            },
            "repository": {
                "id": 5, "name": "widgets", "full_name": "acme/widgets",
                "owner": { "id": 3, "login": "acme", "type": "Organization" }
            },
            "installation": { "id": 77 },
            "sender": { "id": 10, "login": "maintainer", "type": "User" }
        }))
        .unwrap();
        assert!(event.issue.is_pull_request());
        assert_eq!(event.comment.user.login, "maintainer");
        assert_eq!(event.repository.owner.kind, AccountKind::Organization);
        assert_eq!(event.installation.map(|i| i.id), Some(77));
    }
}
