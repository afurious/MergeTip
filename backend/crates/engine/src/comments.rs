//! `issue_comment.created` → tips (FR-1.3, FR-2, FR-4.2, PRD "Limits").

use mergetip_core::{GithubInstallationId, GithubUserId, ThreadRef, TipKind, TipState};
use mergetip_github::{Reaction, webhook::IssueCommentEvent};
use mergetip_parser::{TipCommand, parse_comment};
use mergetip_store::{
    installations,
    tips::{self, NewTip, Tip, TipPatch},
    users::{self, GithubProfile},
    wallets,
};
use serde_json::json;
use sqlx::PgConnection;
use time::Duration;

use crate::{
    Engine, EngineError, TIPS_PER_HOUR, TIPS_PER_PAIR,
    jobs::{self, PostReply, Reply, TipMessage, VerifyTip},
};

/// How long a tip waits for its sender to connect a wallet (Journey C).
pub(crate) const AWAITING_SENDER_WINDOW: Duration = Duration::hours(24);

impl Engine {
    pub(crate) async fn handle_comment(&self, event: IssueCommentEvent) -> Result<(), EngineError> {
        // The sender is the authenticated author GitHub put in the signed payload. Bots
        // (including this App) are skipped so the bot can never tip or loop on its own replies.
        let author = &event.comment.user;
        if author.is_bot() {
            return Ok(());
        }
        let Some(body) = event.comment.body.as_deref() else {
            return Ok(());
        };
        let results = parse_comment(body);
        if results.is_empty() {
            return Ok(());
        }
        let Some(installation) = event.installation.as_ref() else {
            tracing::warn!(
                comment_id = event.comment.id,
                "comment event without installation"
            );
            return Ok(());
        };
        let github_installation = GithubInstallationId::new(installation.id);
        let thread = ThreadRef {
            owner: event.repository.owner.login.clone(),
            repo: event.repository.name.clone(),
            number: event.issue.number,
            is_pr: event.issue.is_pull_request(),
        };

        let mut tx = self.pool.begin().await?;
        let installation =
            match installations::get_by_github_id(&mut *tx, github_installation).await {
                Ok(found) => found,
                Err(mergetip_store::StoreError::NotFound) => {
                    installations::upsert(&mut *tx, github_installation, &thread.owner).await?
                }
                Err(other) => return Err(other.into()),
            };
        if installation.suspended_at.is_some() {
            return Ok(());
        }
        let sender = users::upsert_by_github_id(
            &mut tx,
            &GithubProfile {
                github_id: GithubUserId::new(author.id),
                login: author.login.clone(),
                avatar_url: author.avatar_url.clone(),
                created_at: None,
            },
        )
        .await?;
        // The limit checks below count this sender's tips and then insert more; the row lock
        // makes concurrent comments from one sender take turns, so none passes on a stale count.
        users::lock(&mut tx, sender.id).await?;

        let context = CommentContext {
            github_installation,
            installation_id: installation.id,
            sender_id: sender.id,
            thread,
            comment_id: event.comment.id,
        };
        let mut parse_errors = Vec::new();
        let mut accepted = false;
        for result in results {
            match result {
                Ok(command) => accepted |= self.create_tip(&mut tx, &context, &command).await?,
                Err(err) => parse_errors.push((err.text, err.error.help())),
            }
        }

        if !parse_errors.is_empty() {
            let reply = context.reply(
                format!("parse_errors:{}", context.comment_id),
                Reply::ParseErrors {
                    sender: author.login.clone(),
                    errors: parse_errors,
                },
            );
            jobs::enqueue_reply(&mut tx, &reply).await?;
        }
        if accepted {
            context.react(&mut tx, Reaction::Eyes).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Creates one tip and moves it to its first real state. Returns whether it was accepted
    /// (rejected tips and replays of already-seen commands return `false`).
    async fn create_tip(
        &self,
        tx: &mut PgConnection,
        context: &CommentContext,
        command: &TipCommand,
    ) -> Result<bool, EngineError> {
        let new = NewTip {
            idempotency_key: tips::idempotency_key(
                context.github_installation,
                context.comment_id,
                command.index,
            ),
            installation_id: context.installation_id,
            sender_id: context.sender_id,
            receiver_login: command.receiver.clone(),
            thread: context.thread.clone(),
            comment_id: context.comment_id,
            command_index: command.index,
            kind: if command.on_merge {
                TipKind::OnMerge
            } else {
                TipKind::Now
            },
            amount: command.amount,
            note: command.note.clone(),
        };
        let Some(tip) = tips::insert_idempotent(tx, &new).await? else {
            return Ok(false);
        };

        if let Some(code) = self.limit_breach(tx, &tip).await? {
            tips::transition(
                tx,
                tip.id,
                TipState::Received,
                TipState::Verifying,
                &TipPatch::default(),
                json!({}),
            )
            .await?;
            reject_verifying(
                tx,
                context.github_installation,
                &tip,
                &TipPatch::error(code, None),
            )
            .await?;
            return Ok(false);
        }

        let has_wallet =
            self.settings.stub_payer || wallets::has_active(&mut *tx, tip.sender_id).await?;
        if has_wallet {
            tips::transition(
                tx,
                tip.id,
                TipState::Received,
                TipState::Verifying,
                &TipPatch::default(),
                json!({}),
            )
            .await?;
            jobs::enqueue(tx, &VerifyTip { tip_id: tip.id }).await?;
        } else {
            tips::transition(
                tx,
                tip.id,
                TipState::Received,
                TipState::AwaitingSender,
                &TipPatch {
                    expires_at: Some(self.clock.now() + AWAITING_SENDER_WINDOW),
                    ..TipPatch::default()
                },
                json!({ "reason": "no_wallet" }),
            )
            .await?;
            context
                .tip_reply(tx, &tip, TipMessage::ConnectWallet)
                .await?;
        }
        Ok(true)
    }

    /// Checks that need nothing from GitHub, in the order a sender can fix them.
    async fn limit_breach(
        &self,
        tx: &mut PgConnection,
        tip: &Tip,
    ) -> Result<Option<&'static str>, EngineError> {
        if tip.kind == TipKind::OnMerge && !tip.thread.is_pr {
            return Ok(Some("on_merge_not_pr"));
        }
        let hour_ago = self.clock.now() - Duration::hours(1);
        if tips::count_by_sender_since(&mut *tx, tip.sender_id, hour_ago, tip.id).await?
            >= i64::from(TIPS_PER_HOUR)
        {
            return Ok(Some("rate_limited_sender"));
        }
        if tips::count_for_pair_in_thread(
            &mut *tx,
            tip.sender_id,
            &tip.receiver_login,
            &tip.thread,
            tip.id,
        )
        .await?
            >= i64::from(TIPS_PER_PAIR)
        {
            return Ok(Some("pair_limit"));
        }
        Ok(None)
    }
}

/// Where a comment's tips live, for building replies.
pub(crate) struct CommentContext {
    pub github_installation: GithubInstallationId,
    pub installation_id: mergetip_core::InstallationId,
    pub sender_id: mergetip_core::UserId,
    pub thread: ThreadRef,
    pub comment_id: i64,
}

impl CommentContext {
    fn reply(&self, key: String, reply: Reply) -> PostReply {
        PostReply {
            key,
            installation: self.github_installation,
            repo: self.thread.full_name(),
            thread_number: self.thread.number,
            reply,
        }
    }

    async fn tip_reply(
        &self,
        tx: &mut PgConnection,
        tip: &Tip,
        message: TipMessage,
    ) -> Result<(), EngineError> {
        enqueue_tip_reply(tx, self.github_installation, tip, message).await
    }

    async fn react(&self, tx: &mut PgConnection, reaction: Reaction) -> Result<(), EngineError> {
        enqueue_reaction(
            tx,
            self.github_installation,
            &self.thread,
            self.comment_id,
            reaction,
        )
        .await
    }
}

/// Rejects a tip in `verifying`: records why (`patch.error_code`), replies in the thread and
/// reacts 😕 on the `/tip` comment. The one place rejection side effects are defined.
pub(crate) async fn reject_verifying(
    tx: &mut PgConnection,
    installation: GithubInstallationId,
    tip: &Tip,
    patch: &TipPatch,
) -> Result<(), EngineError> {
    tips::transition(
        tx,
        tip.id,
        TipState::Verifying,
        TipState::Rejected,
        patch,
        json!({ "reason": patch.error_code }),
    )
    .await?;
    enqueue_tip_reply(tx, installation, tip, TipMessage::Rejected).await?;
    enqueue_reaction(
        tx,
        installation,
        &tip.thread,
        tip.comment_id,
        Reaction::Confused,
    )
    .await?;
    Ok(())
}

/// Cancels a tip in `from` before any money moved: records why, and adds a line to the thread's
/// status comment. Returns `false` (and writes nothing) when the tip already left `from`, so a
/// timer or redelivery that fires twice is a no-op. The one place cancellation side effects are
/// defined.
pub(crate) async fn cancel_tip(
    tx: &mut PgConnection,
    tip: &Tip,
    from: TipState,
    code: &str,
) -> Result<bool, EngineError> {
    let cancelled = match tips::transition(
        tx,
        tip.id,
        from,
        TipState::Cancelled,
        &TipPatch::error(code, None),
        json!({ "reason": code }),
    )
    .await
    {
        Ok(tip) => tip,
        Err(mergetip_store::StoreError::Conflict) => return Ok(false),
        Err(other) => return Err(other.into()),
    };
    let installation = installations::get(&mut *tx, cancelled.installation_id).await?;
    enqueue_tip_reply(
        tx,
        installation.github_installation_id,
        &cancelled,
        TipMessage::Cancelled,
    )
    .await?;
    Ok(true)
}

/// Queues a message about `tip` in its thread, keyed per tip and message kind.
pub(crate) async fn enqueue_tip_reply(
    tx: &mut PgConnection,
    installation: GithubInstallationId,
    tip: &Tip,
    message: TipMessage,
) -> Result<(), EngineError> {
    let reply = PostReply {
        key: format!("reply:{}:{}", tip.id, message.as_str()),
        installation,
        repo: tip.thread.full_name(),
        thread_number: tip.thread.number,
        reply: Reply::Tip {
            tip_id: tip.id,
            message,
        },
    };
    jobs::enqueue_reply(tx, &reply).await?;
    Ok(())
}

/// Queues a reaction on a `/tip` comment, keyed per comment and reaction.
pub(crate) async fn enqueue_reaction(
    tx: &mut PgConnection,
    installation: GithubInstallationId,
    thread: &ThreadRef,
    comment_id: i64,
    reaction: Reaction,
) -> Result<(), EngineError> {
    let reply = PostReply {
        key: format!("reaction:{comment_id}:{}", reaction.content()),
        installation,
        repo: thread.full_name(),
        thread_number: thread.number,
        reply: Reply::Reaction {
            comment_id,
            reaction,
        },
    };
    jobs::enqueue_reply(tx, &reply).await?;
    Ok(())
}
