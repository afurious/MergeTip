//! `post_reply`: render a reply from current data and write it to GitHub (FR-1.4, FR-1.5).

use mergetip_github::{
    GitHubError, RepoName,
    replies::{self, Limits, TipContext},
};
use mergetip_parser::SYNTAX;
use mergetip_store::{
    bot_comments::{self, BotComment},
    tips::{self, Tip},
    users,
};

use crate::{
    Engine, EngineError, TIPS_PER_HOUR, TIPS_PER_PAIR,
    jobs::{PostReply, Reply, TipMessage},
};

impl Engine {
    pub async fn post_reply(&self, job: &PostReply) -> Result<(), EngineError> {
        let repo: RepoName = job.repo.parse()?;
        match &job.reply {
            Reply::ParseErrors { sender, errors } => {
                let body = replies::parse_errors(sender, errors, SYNTAX);
                self.github
                    .create_comment(job.installation, &repo, job.thread_number, &body)
                    .await?;
            }
            Reply::Reaction {
                comment_id,
                reaction,
            } => {
                self.github
                    .create_reaction(job.installation, &repo, *comment_id, *reaction)
                    .await?;
            }
            Reply::Tip { tip_id, message } => {
                let tip = tips::get(&self.pool, *tip_id).await?;
                let text = self.render(&tip, *message).await?;
                if message.is_status() {
                    self.append_status(job, &repo, text).await?;
                } else {
                    self.github
                        .create_comment(job.installation, &repo, job.thread_number, &text)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn render(&self, tip: &Tip, message: TipMessage) -> Result<String, EngineError> {
        let sender = users::get(&self.pool, tip.sender_id).await?;
        let context = TipContext {
            sender: &sender.github_login,
            receiver: &tip.receiver_login,
            amount: tip.amount,
            note: tip.note.as_deref(),
        };
        Ok(match message {
            TipMessage::ConnectWallet => {
                replies::connect_wallet(&context, &self.link("/settings/wallet"))
            }
            TipMessage::NeedsConfirmation => replies::needs_confirmation(
                &context,
                sender.confirm_threshold,
                &self.link(&format!("/confirm/{}", tip.id)),
            ),
            TipMessage::Rejected => replies::rejected(
                tip.error_code.as_deref().unwrap_or("unknown"),
                &context,
                &Limits {
                    per_tip_max: sender.per_tip_max,
                    tips_per_hour: TIPS_PER_HOUR,
                    tips_per_pair: TIPS_PER_PAIR,
                },
                &self.link("/settings/limits"),
            ),
            TipMessage::Held => replies::held(&context),
            TipMessage::Paid => {
                replies::receipt(&context, tip.payment_hash.as_deref().unwrap_or_default())
            }
        })
    }

    /// Appends a line to the thread's collapsed status comment, creating it on first use.
    ///
    /// The advisory lock serialises concurrent updates for one thread, and is held across the
    /// GitHub call so the stored lines and the comment on GitHub never diverge. If someone
    /// deleted the comment, a new one is started.
    async fn append_status(
        &self,
        job: &PostReply,
        repo: &RepoName,
        line: String,
    ) -> Result<(), EngineError> {
        let mut tx = self.pool.begin().await?;
        bot_comments::lock_thread(&mut tx, &job.repo, job.thread_number).await?;
        let existing = bot_comments::get(&mut tx, &job.repo, job.thread_number).await?;

        let mut lines = existing
            .as_ref()
            .map(|c| c.lines.clone())
            .unwrap_or_default();
        lines.push(line);
        let body = replies::status_comment(&lines);

        let updated = match &existing {
            Some(comment) => match self
                .github
                .update_comment(job.installation, repo, comment.github_comment_id, &body)
                .await
            {
                Ok(()) => Some(comment.github_comment_id),
                Err(GitHubError::NotFound) => None,
                Err(other) => return Err(other.into()),
            },
            None => None,
        };
        let github_comment_id = match updated {
            Some(id) => id,
            None => {
                let fresh_lines = vec![lines.pop().unwrap_or_default()];
                let fresh = replies::status_comment(&fresh_lines);
                lines = fresh_lines;
                self.github
                    .create_comment(job.installation, repo, job.thread_number, &fresh)
                    .await?
                    .id
            }
        };
        bot_comments::upsert(
            &mut tx,
            &job.repo,
            job.thread_number,
            &BotComment {
                github_comment_id,
                lines,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
