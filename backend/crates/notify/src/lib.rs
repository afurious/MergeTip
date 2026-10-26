//! Email notifications (FR-11, FR-12.5): pledge notices, reminders, digests and confirmation
//! codes.
//!
//! Everything sends through the [`Mailer`] trait, so the engine and the API never know which
//! provider is configured, and tests can capture what would have been sent.

mod mailer;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use mailer::{DisabledMailer, Email, MailError, Mailer, SentEmail};
