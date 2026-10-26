//! A [`Mailer`] that records every message instead of sending it.

use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::{Email, MailError, Mailer, SentEmail};

#[derive(Debug, Default)]
pub struct RecordingMailer {
    sent: Mutex<Vec<Email>>,
}

impl RecordingMailer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sent(&self) -> MutexGuard<'_, Vec<Email>> {
        self.sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl Mailer for RecordingMailer {
    async fn send(&self, email: &Email) -> Result<SentEmail, MailError> {
        let mut sent = self.sent();
        sent.push(email.clone());
        Ok(SentEmail {
            provider_id: Some(format!("msg-{}", sent.len())),
        })
    }
}
