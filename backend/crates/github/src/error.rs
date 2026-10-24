use std::time::Duration;

use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubError {
    #[error("not found on GitHub")]
    NotFound,
    #[error("GitHub rejected our credentials")]
    Unauthorized,
    /// Primary or secondary rate limit. Retrying before `reset_at` only burns more quota.
    #[error("GitHub rate limit hit; resets at {reset_at}")]
    RateLimited { reset_at: OffsetDateTime },
    #[error("GitHub returned {status}: {message}")]
    Http { status: u16, message: String },
    #[error("GitHub request failed: {0}")]
    Transport(String),
    #[error("unexpected GitHub response: {0}")]
    Decode(String),
    #[error("invalid input for a GitHub request: {0}")]
    InvalidInput(String),
}

impl GitHubError {
    /// Whether the same request could succeed later. Callers retry these and must never treat
    /// them as a verdict (e.g. "the receiver didn't contribute").
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport(_) => true,
            Self::Http { status, .. } => *status >= 500,
            Self::NotFound | Self::Unauthorized | Self::Decode(_) | Self::InvalidInput(_) => false,
        }
    }

    /// How long to wait before retrying, when GitHub said.
    pub fn retry_after(&self, now: OffsetDateTime) -> Option<Duration> {
        match self {
            Self::RateLimited { reset_at } => {
                let wait = (*reset_at - now).max(time::Duration::ZERO) + time::Duration::seconds(1);
                Some(Duration::try_from(wait).unwrap_or(Duration::from_secs(60)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn rate_limit_waits_until_reset() {
        let err = GitHubError::RateLimited {
            reset_at: datetime!(2026-11-02 10:01 UTC),
        };
        assert!(err.is_retryable());
        assert_eq!(
            err.retry_after(datetime!(2026-11-02 10:00 UTC)),
            Some(Duration::from_secs(61))
        );
        assert_eq!(
            err.retry_after(datetime!(2026-11-02 11:00 UTC)),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn client_errors_are_final_and_server_errors_are_not() {
        assert!(!GitHubError::NotFound.is_retryable());
        assert!(
            !GitHubError::Http {
                status: 422,
                message: String::new()
            }
            .is_retryable()
        );
        assert!(
            GitHubError::Http {
                status: 502,
                message: String::new()
            }
            .is_retryable()
        );
    }
}
