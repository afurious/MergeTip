//! Email notifications (FR-11, FR-12.5): pledge notices, reminders, digests and confirmation
//! codes.
//!
//! Everything sends through the [`Mailer`] trait, so the engine and the API never know which
//! provider is configured, and tests can capture what would have been sent.

mod mailer;
mod resend;
mod smtp;
pub mod templates;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod unsubscribe;

use std::sync::Arc;

pub use mailer::{DisabledMailer, Email, MailError, Mailer, SentEmail};
use mergetip_crypto::Redacted;
pub use resend::{DEFAULT_BASE_URL as RESEND_BASE_URL, ResendMailer};
pub use smtp::SmtpMailer;
pub use templates::Rendered;

/// Which provider to use, from `RESEND_API_KEY`, `SMTP_URL` and `EMAIL_FROM`.
#[derive(Debug, Clone)]
pub struct MailerConfig {
    pub resend_api_key: Option<Redacted<String>>,
    pub smtp_url: Option<String>,
    pub from: String,
}

/// Resend when it has a key; otherwise SMTP (Mailpit) when there is a URL; otherwise a mailer
/// that sends nothing, so notices fall back to GitHub mentions.
pub fn from_config(config: &MailerConfig) -> Result<Arc<dyn Mailer>, MailError> {
    if let Some(key) = &config.resend_api_key {
        return Ok(Arc::new(ResendMailer::new(
            RESEND_BASE_URL,
            key.clone(),
            config.from.clone(),
        )?));
    }
    if let Some(url) = &config.smtp_url {
        return Ok(Arc::new(SmtpMailer::new(url, &config.from)?));
    }
    tracing::warn!("no RESEND_API_KEY or SMTP_URL: emails are not sent");
    Ok(Arc::new(DisabledMailer))
}

/// Addresses a rendered message, with the one-click unsubscribe headers every email carries
/// (RFC 8058).
pub fn email(
    to: String,
    rendered: Rendered,
    unsubscribe_url: &str,
    tag: &str,
    idempotency_key: Option<String>,
) -> Email {
    Email {
        to,
        subject: rendered.subject,
        html: rendered.html,
        text: rendered.text,
        headers: vec![
            ("List-Unsubscribe".into(), format!("<{unsubscribe_url}>")),
            (
                "List-Unsubscribe-Post".into(),
                "List-Unsubscribe=One-Click".into(),
            ),
        ],
        tag: tag.into(),
        idempotency_key,
    }
}
