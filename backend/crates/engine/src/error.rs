use std::time::Duration;

use mergetip_github::GitHubError;
use mergetip_store::StoreError;
use time::OffsetDateTime;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    /// Input that can never be processed (an unparseable payload, a missing event row).
    #[error("malformed input: {0}")]
    Malformed(String),
}

impl From<sqlx::Error> for EngineError {
    fn from(err: sqlx::Error) -> Self {
        Self::Store(err.into())
    }
}

impl EngineError {
    /// Whether running the same job again could succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            // Database trouble and lost races are transient; a re-run sees the new state.
            Self::Store(StoreError::NotFound | StoreError::Corrupt(_)) => false,
            Self::Store(_) => true,
            Self::GitHub(err) => err.is_retryable(),
            Self::Malformed(_) => false,
        }
    }

    /// A specific wait GitHub asked for (rate limits).
    pub fn retry_after(&self, now: OffsetDateTime) -> Option<Duration> {
        match self {
            Self::GitHub(err) => err.retry_after(now),
            _ => None,
        }
    }
}
