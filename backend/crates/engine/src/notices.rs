//! Telling pledge receivers by email (FR-11, RFC 0003 "Email"): find an address, then send at
//! most one email a day listing every pledge that needs a notice.

use mergetip_core::{TipId, TipState, UserId};
use mergetip_github::RepoName;
use mergetip_notify::{
    MailError,
    templates::{self, PledgeLine},
    unsubscribe,
};
use mergetip_store::{
    email_messages, feature_flags, installations,
    jobs::NewJob,
    tips::{self, Tip},
    users::{self, EmailSource, User},
};
use sqlx::PgConnection;
use time::{Duration, OffsetDateTime};

use crate::{
    Engine, EngineError,
    jobs::{JobPayload, SendDigest},
};

/// Reminders go out this long before a pledge expires (day 23 of 30).
pub const REMINDER_BEFORE_EXPIRY: Duration = Duration::days(7);
/// At most one email per receiver in this window.
const DIGEST_INTERVAL: Duration = Duration::hours(24);

/// GitHub's private-email relay addresses can't receive mail from us.
fn usable(email: &str) -> bool {
    let email = email.trim().to_ascii_lowercase();
    email.contains('@') && !email.ends_with("users.noreply.github.com")
}

/// Queues the receiver's digest: as soon as possible, but no sooner than a day after their last
/// email. The dedupe key names the day it runs, so every reason to email them before then folds
/// into the one queued job.
pub(crate) async fn enqueue_digest(
    conn: &mut PgConnection,
    user_id: UserId,
    now: OffsetDateTime,
) -> Result<(), EngineError> {
    let last = users::last_email_sent_at(&mut *conn, user_id).await?;
    let run_at = last.map_or(now, |last| (last + DIGEST_INTERVAL).max(now));
    let payload = SendDigest { user_id };
    let job: NewJob = payload
        .job()
        .run_at(run_at)
        .dedupe_key(format!("digest:{user_id}:{}", run_at.date()));
    mergetip_store::jobs::enqueue(conn, &job).await?;
    Ok(())
}

impl Engine {
    /// Finds an email for a new pledge's receiver (FR-11.2), stores it encrypted, and queues their
    /// digest. Sources, best first: the verified email from signing in; the public profile email;
    /// the email on their commit in the PR (when commit emails are allowed).
    pub async fn notify_pledge(&self, tip_id: TipId) -> Result<(), EngineError> {
        let tip = tips::get(&self.pool, tip_id).await?;
        let Some(receiver_id) = tip.receiver_id else {
            return Ok(());
        };
        if tip.state != TipState::Pledged {
            return Ok(());
        }
        let receiver = users::get(&self.pool, receiver_id).await?;
        if receiver.email_opt_out {
            tips::set_email_status_before_notice(&self.pool, tip.id, "none").await?;
            return Ok(());
        }

        let secrets = users::secrets(&self.pool, receiver.id).await?;
        if secrets.email_source.as_deref() != Some(EmailSource::Verified.as_str())
            && let Some((email, source)) = self.find_email(&tip, &receiver).await?
        {
            let sealed = self
                .payments
                .keyring
                .seal(
                    email.as_bytes(),
                    format!("users.email:{}", receiver.id).as_bytes(),
                )
                .map_err(|err| EngineError::Malformed(format!("cannot seal an email: {err}")))?;
            users::set_email(&self.pool, receiver.id, &sealed, source).await?;
        }

        let has_email = users::secrets(&self.pool, receiver.id)
            .await?
            .email_ciphertext
            .is_some();
        // A digest for an earlier pledge may already have covered this one.
        let mut tx = self.pool.begin().await?;
        let status = if has_email { "pending" } else { "none" };
        if tips::set_email_status_before_notice(&mut *tx, tip.id, status).await? && has_email {
            enqueue_digest(&mut tx, receiver.id, self.clock.now()).await?;
        }
        tx.commit().await?;
        tracing::info!(%tip_id, user_id = %receiver.id, has_email, "pledge notice prepared");
        Ok(())
    }

    /// The public profile email, else (if allowed) the receiver's commit email on the PR.
    async fn find_email(
        &self,
        tip: &Tip,
        receiver: &User,
    ) -> Result<Option<(String, EmailSource)>, EngineError> {
        let installation = installations::get(&self.pool, tip.installation_id)
            .await?
            .github_installation_id;
        match self
            .github
            .get_user(installation, &receiver.github_login)
            .await
        {
            Ok(account) => {
                if let Some(email) = account.email.filter(|e| usable(e)) {
                    return Ok(Some((email, EmailSource::PublicProfile)));
                }
            }
            Err(err) if err.is_retryable() => return Err(err.into()),
            Err(_) => {}
        }

        if !tip.thread.is_pr || !self.commit_emails_allowed().await? {
            return Ok(None);
        }
        let repo = RepoName::new(&tip.thread.owner, &tip.thread.repo)?;
        let commits = match self
            .github
            .list_pull_commits(installation, &repo, tip.thread.number)
            .await
        {
            Ok(commits) => commits,
            Err(err) if err.is_retryable() => return Err(err.into()),
            Err(_) => return Ok(None),
        };
        let email = commits
            .iter()
            .filter(|c| {
                c.author
                    .as_ref()
                    .is_some_and(|a| a.id == receiver.github_id.get())
            })
            .filter_map(|c| c.commit.author.as_ref()?.email.clone())
            .find(|email| usable(email));
        Ok(email.map(|email| (email, EmailSource::Commit)))
    }

    async fn commit_emails_allowed(&self) -> Result<bool, EngineError> {
        Ok(
            feature_flags::lookup(&self.pool, feature_flags::EMAIL_FROM_COMMITS)
                .await?
                .unwrap_or(!self.settings.production),
        )
    }

    /// Queues a digest for every receiver whose 7-day reminder has come due. Run by the expiry
    /// job every minute; the digest's dedupe key keeps that from queuing duplicates.
    pub async fn schedule_reminders(&self) -> Result<usize, EngineError> {
        let now = self.clock.now();
        let due = tips::receivers_due_a_reminder(&self.pool, now, REMINDER_BEFORE_EXPIRY).await?;
        for user in &due {
            let mut conn = self.pool.acquire().await?;
            enqueue_digest(&mut conn, *user, now).await?;
        }
        Ok(due.len())
    }

    /// Emails the user every pledge notice now due, in one message, and records it. With no
    /// address, no provider or an opt-out, the due tips are marked as handled (email status
    /// `none`) so they aren't picked up again.
    pub async fn send_digest(&self, user_id: UserId) -> Result<(), EngineError> {
        let now = self.clock.now();
        let user = users::get(&self.pool, user_id).await?;
        let candidates = tips::notice_candidates(&self.pool, user_id).await?;
        let notices: Vec<(Due, &Tip)> = candidates
            .iter()
            .map(|tip| (notice_for(tip, now), tip))
            .collect();
        if !notices.iter().any(|(notice, _)| notice.is_due()) {
            return Ok(());
        }
        let ids: Vec<TipId> = candidates.iter().map(|t| t.id).collect();

        let to = if user.email_opt_out {
            None
        } else {
            self.email_of(user_id).await?
        };
        let Some(to) = to else {
            tips::mark_notified(&self.pool, &ids, now, "none").await?;
            return Ok(());
        };

        let mut lines = Vec::with_capacity(notices.len());
        for (notice, tip) in &notices {
            lines.push((notice.into_template(), self.pledge_line(tip).await?));
        }
        let unsubscribe_url = unsubscribe::url(
            &self.settings.api_base_url,
            &self.payments.keyring,
            user_id.as_uuid(),
        );
        let (tag, rendered) = match lines.as_slice() {
            [(templates::Notice::New, line)] => {
                ("pledge", templates::pledge(line, &unsubscribe_url))
            }
            [(templates::Notice::Reminder, line)] => {
                ("reminder", templates::reminder(line, &unsubscribe_url))
            }
            [(templates::Notice::Expired, line)] => {
                ("expiry", templates::expiry(line, &unsubscribe_url))
            }
            _ => ("digest", templates::digest(&lines, &unsubscribe_url)),
        };
        let email = mergetip_notify::email(
            to,
            rendered,
            &unsubscribe_url,
            tag,
            Some(format!("digest:{user_id}:{}", now.date())),
        );

        match self.mailer.send(&email).await {
            Ok(sent) => {
                let mut tx = self.pool.begin().await?;
                if let Some(provider_id) = &sent.provider_id {
                    email_messages::record(&mut *tx, provider_id, user_id, &ids, now).await?;
                }
                users::set_last_email_sent_at(&mut *tx, user_id, now).await?;
                tips::mark_notified(&mut *tx, &ids, now, "sent").await?;
                tx.commit().await?;
                tracing::info!(%user_id, tag, tips = ids.len(), "pledge email sent");
            }
            Err(err) if err.is_retryable() => {
                return Err(EngineError::Retry {
                    reason: err.to_string(),
                    after: std::time::Duration::from_secs(300),
                });
            }
            Err(MailError::NotConfigured) => {
                tips::mark_notified(&self.pool, &ids, now, "none").await?;
            }
            Err(err) => {
                tracing::warn!(%user_id, error = %err, "pledge email refused");
                tips::mark_notified(&self.pool, &ids, now, "failed").await?;
            }
        }

        // A pledge that arrived while this ran waits for tomorrow's digest.
        let late = tips::notice_candidates(&self.pool, user_id)
            .await?
            .iter()
            .any(|tip| !ids.contains(&tip.id) && notice_for(tip, now).is_due());
        if late {
            let mut conn = self.pool.acquire().await?;
            enqueue_digest(&mut conn, user_id, now).await?;
        }
        Ok(())
    }

    async fn email_of(&self, user_id: UserId) -> Result<Option<String>, EngineError> {
        let Some(sealed) = users::secrets(&self.pool, user_id).await?.email_ciphertext else {
            return Ok(None);
        };
        let plain = self
            .payments
            .keyring
            .open(&sealed, format!("users.email:{user_id}").as_bytes())
            .map_err(|err| EngineError::Malformed(format!("cannot open an email: {err}")))?;
        Ok(String::from_utf8(plain.expose().clone()).ok())
    }

    async fn pledge_line(&self, tip: &Tip) -> Result<PledgeLine, EngineError> {
        let sender = users::get(&self.pool, tip.sender_id).await?;
        let path = if tip.thread.is_pr { "pull" } else { "issues" };
        Ok(PledgeLine {
            sender: sender.github_login,
            amount: tip.amount.to_string(),
            note: tip.note.clone(),
            thread: format!("{}#{}", tip.thread.full_name(), tip.thread.number),
            thread_url: format!(
                "https://github.com/{}/{path}/{}",
                tip.thread.full_name(),
                tip.thread.number
            ),
            claim_url: if tip.state == TipState::Pledged {
                self.claim_link(tip).await?
            } else {
                None
            },
            expires_on: tip
                .expires_at
                .map(|at| at.date().to_string())
                .unwrap_or_default(),
        })
    }
}

/// Why a pledge would be in an email right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    New,
    Reminder,
    Expired,
    /// Open, but nothing new to say; listed for context only.
    Open,
}

impl Due {
    fn is_due(self) -> bool {
        self != Self::Open
    }

    fn into_template(self) -> templates::Notice {
        match self {
            Self::New => templates::Notice::New,
            Self::Reminder => templates::Notice::Reminder,
            Self::Expired => templates::Notice::Expired,
            Self::Open => templates::Notice::Open,
        }
    }
}

fn notice_for(tip: &Tip, now: OffsetDateTime) -> Due {
    if tip.state == TipState::Expired {
        return Due::Expired;
    }
    let Some(last) = tip.last_notified_at else {
        return Due::New;
    };
    match tip.expires_at {
        Some(expires) if now >= expires - REMINDER_BEFORE_EXPIRY => {
            if last < expires - REMINDER_BEFORE_EXPIRY {
                Due::Reminder
            } else {
                Due::Open
            }
        }
        _ => Due::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noreply_addresses_are_skipped() {
        assert!(usable("alice@example.com"));
        assert!(!usable("12345+alice@users.noreply.github.com"));
        assert!(!usable("alice@USERS.NOREPLY.GITHUB.COM"));
        assert!(!usable("not-an-email"));
    }
}
