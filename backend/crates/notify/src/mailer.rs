use async_trait::async_trait;

/// One message, already rendered. Addresses are PII: never log `to`.
#[derive(Clone, PartialEq, Eq)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub html: String,
    pub text: String,
    /// Extra headers, e.g. `List-Unsubscribe`.
    pub headers: Vec<(String, String)>,
    /// What kind of email this is (`pledge`, `digest`, `otp`…), for provider analytics and logs.
    pub tag: String,
    /// Makes a retried send a no-op at the provider (Resend `Idempotency-Key`).
    pub idempotency_key: Option<String>,
}

impl std::fmt::Debug for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Email")
            .field("subject", &self.subject)
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

/// What the provider said about an accepted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentEmail {
    /// The provider's message ID, matched against delivery webhooks. SMTP has none.
    pub provider_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// No provider is configured; the caller treats the notice as not sent.
    #[error("email is not configured")]
    NotConfigured,
    /// The provider couldn't be reached or failed; worth retrying.
    #[error("email provider unavailable: {0}")]
    Unavailable(String),
    /// The provider refused this message (bad address, invalid request); retrying won't help.
    #[error("email refused: {0}")]
    Rejected(String),
}

impl MailError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, email: &Email) -> Result<SentEmail, MailError>;
}

/// Used when neither Resend nor SMTP is configured: every send reports
/// [`MailError::NotConfigured`], and notices fall back to GitHub mentions only.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledMailer;

#[async_trait]
impl Mailer for DisabledMailer {
    async fn send(&self, _email: &Email) -> Result<SentEmail, MailError> {
        Err(MailError::NotConfigured)
    }
}
