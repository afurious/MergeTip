//! Typed job payloads. Each payload type knows its `jobs.kind`, so producers and handlers cannot
//! disagree on a kind string or payload shape.

use mergetip_core::{GithubInstallationId, TipId, UserId};
use mergetip_github::Reaction;
use mergetip_store::{
    StoreResult,
    jobs::{self, Enqueued, NewJob},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::PgConnection;

pub trait JobPayload: Serialize + DeserializeOwned {
    const KIND: &'static str;

    fn job(&self) -> NewJob {
        NewJob::new(
            Self::KIND,
            serde_json::to_value(self).expect("job payloads serialise to JSON"),
        )
    }

    /// Decodes a claimed job's payload. `None` means the payload is malformed: retrying cannot
    /// fix it, so the handler should fail the job permanently.
    fn decode(payload: &serde_json::Value) -> Option<Self> {
        Self::deserialize(payload).ok()
    }
}

/// Enqueues `payload` inside the caller's transaction.
pub async fn enqueue<P: JobPayload>(conn: &mut PgConnection, payload: &P) -> StoreResult<Enqueued> {
    jobs::enqueue(conn, &payload.job()).await
}

/// Enqueues a reply unless one with the same key is already queued. This only avoids redundant
/// jobs; `sent_replies` is what guarantees a reply is posted at most once.
pub(crate) async fn enqueue_reply(
    conn: &mut PgConnection,
    reply: &PostReply,
) -> StoreResult<Enqueued> {
    jobs::enqueue(conn, &reply.job().dedupe_key(reply.key.clone())).await
}

/// Process one stored GitHub webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEvent {
    pub delivery_id: String,
}

impl JobPayload for ProcessEvent {
    const KIND: &'static str = "process_event";
}

/// Check the receiver's contribution for a tip in `verifying`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyTip {
    pub tip_id: TipId,
}

impl JobPayload for VerifyTip {
    const KIND: &'static str = "verify_tip";
}

/// Pay a tip in `paying`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayTip {
    pub tip_id: TipId,
}

impl JobPayload for PayTip {
    const KIND: &'static str = "pay_tip";
}

/// The sender just connected a wallet: move their waiting tips on to verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSenderTips {
    pub user_id: UserId,
}

impl JobPayload for ResumeSenderTips {
    const KIND: &'static str = "resume_sender_tips";
}

/// Resolve the `held` (on-merge) tips of one pull request against its current state on GitHub:
/// pay them if it merged, cancel them if it closed unmerged, leave them if it is still open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveHeld {
    pub installation: GithubInstallationId,
    pub repo: String,
    pub number: u32,
}

impl JobPayload for ResolveHeld {
    const KIND: &'static str = "resolve_held";
}

/// Settle the fate of a payment attempt whose outcome is unknown (the wallet didn't answer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcilePayment {
    pub tip_id: TipId,
    pub attempt_id: uuid::Uuid,
}

impl JobPayload for ReconcilePayment {
    const KIND: &'static str = "reconcile_payment";
}

/// Write something in a GitHub thread. Rendering happens when the job runs, from the tip's
/// current data, so a retried reply never shows stale facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostReply {
    /// Names what is said where, e.g. `reply:<tip id>:paid`. A key is posted at most once.
    pub key: String,
    pub installation: GithubInstallationId,
    pub repo: String,
    pub thread_number: u32,
    pub reply: Reply,
}

impl JobPayload for PostReply {
    const KIND: &'static str = "post_reply";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// Every malformed command in one comment, each as `(command text, help)`.
    ParseErrors {
        sender: String,
        errors: Vec<(String, String)>,
    },
    /// A message about one tip.
    Tip { tip_id: TipId, message: TipMessage },
    /// A reaction on the `/tip` comment.
    Reaction { comment_id: i64, reaction: Reaction },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipMessage {
    ConnectWallet,
    NeedsConfirmation,
    Rejected,
    Held,
    Paid,
    /// The receiver has no Lightning address: the tip waits for them.
    Pledged,
    /// A verified tip couldn't be paid (`tips.error_code` says why).
    Failed,
    /// The tip ended before any money moved (`tips.error_code` says why).
    Cancelled,
}

impl TipMessage {
    /// Status updates go into the thread's single collapsed MergeTip comment. Messages someone
    /// must act on are posted as new comments, because GitHub does not reliably notify people
    /// mentioned in an edited comment.
    pub const fn is_status(self) -> bool {
        matches!(self, Self::Held | Self::Paid | Self::Cancelled)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectWallet => "connect_wallet",
            Self::NeedsConfirmation => "needs_confirmation",
            Self::Rejected => "rejected",
            Self::Held => "held",
            Self::Paid => "paid",
            Self::Pledged => "pledged",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn payloads_round_trip_through_jobs() {
        let payload = VerifyTip {
            tip_id: TipId::new_random(),
        };
        let job = payload.job();
        assert_eq!(job.kind, "verify_tip");
        assert_eq!(VerifyTip::decode(&job.payload), Some(payload));
        assert_eq!(PayTip::decode(&json!({ "nope": 1 })), None);
    }

    #[test]
    fn reply_payload_shape_is_stable() {
        let reply = PostReply {
            key: "reaction:9:eyes".into(),
            installation: GithubInstallationId::new(7),
            repo: "acme/widgets".into(),
            thread_number: 42,
            reply: Reply::Reaction {
                comment_id: 9,
                reaction: Reaction::Eyes,
            },
        };
        assert_eq!(
            reply.job().payload,
            json!({
                "key": "reaction:9:eyes",
                "installation": 7, "repo": "acme/widgets", "thread_number": 42,
                "reply": { "type": "reaction", "comment_id": 9, "reaction": "eyes" }
            })
        );
    }
}
