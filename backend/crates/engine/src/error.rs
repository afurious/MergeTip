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
    /// Not now: try again after `after` (a receiver's server down, a provider outage).
    #[error("{reason}")]
    Retry { reason: String, after: Duration },
    /// Payments are switched off (`payments_enabled`). Not a failure: the job waits without
    /// using up its attempts, however long the pause lasts.
    #[error("payments are paused")]
    Paused { after: Duration },
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
            Self::Retry { .. } | Self::Paused { .. } => true,
        }
    }

    /// A specific wait GitHub asked for (rate limits).
    pub fn retry_after(&self, now: OffsetDateTime) -> Option<Duration> {
        match self {
            Self::GitHub(err) => err.retry_after(now),
            Self::Retry { after, .. } | Self::Paused { after } => Some(*after),
            _ => None,
        }
    }
}
