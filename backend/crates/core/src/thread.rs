//! A pull request or issue thread on GitHub.

use serde::{Deserialize, Serialize};

/// Identifies the thread a tip was made in. GitHub numbers PRs and issues from one sequence per
/// repository, so `number` alone is unambiguous; `is_pr` decides which rules and URLs apply.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadRef {
    pub owner: String,
    pub repo: String,
    pub number: u32,
    pub is_pr: bool,
}

impl ThreadRef {
    /// `owner/name`, the form stored in `tips.repo`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    pub fn html_url(&self) -> String {
        let kind = if self.is_pr { "pull" } else { "issues" };
        format!(
            "https://github.com/{}/{}/{kind}/{}",
            self.owner, self.repo, self.number
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_depend_on_thread_kind() {
        let mut thread = ThreadRef {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 42,
            is_pr: true,
        };
        assert_eq!(thread.full_name(), "acme/widgets");
        assert_eq!(thread.html_url(), "https://github.com/acme/widgets/pull/42");
        thread.is_pr = false;
        assert_eq!(
            thread.html_url(),
            "https://github.com/acme/widgets/issues/42"
        );
    }
}
