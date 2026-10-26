//! The `Tip` shape every tip-returning endpoint shares (frozen contract, section 0.9).

use mergetip_core::{TipKind, TipState as Core};
use mergetip_store::{
    tips::Tip as StoredTip,
    users::{self, User},
};
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::{ApiError, AppState};

/// A tip's state as clients see it. Internal fiat states map onto the closest public one until
/// fiat payouts add their own values (M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TipState {
    Received,
    AwaitingSender,
    Verifying,
    AwaitingConfirmation,
    Held,
    Paying,
    Paid,
    Pledged,
    Rejected,
    Cancelled,
    Expired,
    Failed,
}

impl From<Core> for TipState {
    fn from(state: Core) -> Self {
        match state {
            Core::Received => Self::Received,
            Core::AwaitingSender => Self::AwaitingSender,
            Core::Verifying => Self::Verifying,
            Core::AwaitingConfirmation => Self::AwaitingConfirmation,
            Core::Held => Self::Held,
            Core::Paying | Core::FiatProcessing | Core::FiatStuck => Self::Paying,
            Core::Paid => Self::Paid,
            Core::Pledged => Self::Pledged,
            Core::Rejected => Self::Rejected,
            Core::Cancelled => Self::Cancelled,
            Core::Expired => Self::Expired,
            Core::Failed | Core::Refunded => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TipKindBody {
    Now,
    OnMerge,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserRef {
    pub login: String,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Tip {
    pub id: String,
    pub state: TipState,
    pub amount_sats: u64,
    #[schema(inline)]
    pub kind: TipKindBody,
    pub note: Option<String>,
    pub sender: UserRef,
    pub receiver: UserRef,
    /// `owner/name`.
    pub repo: String,
    pub thread_number: u32,
    pub thread_url: String,
    pub payment_hash: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
    /// When the tip's current wait ends: the confirmation deadline while it awaits confirmation,
    /// otherwise the sender or pledge deadline.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<OffsetDateTime>,
}

/// `{ tip }`, the body of the claim and confirmation endpoints.
#[derive(Debug, Serialize, ToSchema)]
pub struct TipEnvelope {
    pub tip: Tip,
}

impl Tip {
    /// Builds the public shape. `sender` is the sender's row; `receiver` is the receiver's row
    /// once verification linked one (before that, only the login from the comment is known).
    pub(crate) fn from_parts(tip: &StoredTip, sender: &User, receiver: Option<&User>) -> Self {
        let kind = match tip.kind {
            TipKind::Now => TipKindBody::Now,
            TipKind::OnMerge => TipKindBody::OnMerge,
        };
        let receiver = match receiver {
            Some(user) => user_ref(user),
            None => UserRef {
                login: tip.receiver_login.clone(),
                avatar_url: fallback_avatar(&tip.receiver_login),
            },
        };
        let path = if tip.thread.is_pr { "pull" } else { "issues" };
        Self {
            id: tip.id.to_string(),
            state: tip.state.into(),
            amount_sats: tip.amount.get(),
            kind,
            note: tip.note.clone(),
            sender: user_ref(sender),
            receiver,
            repo: tip.thread.full_name(),
            thread_number: tip.thread.number,
            thread_url: format!(
                "https://github.com/{}/{path}/{}",
                tip.thread.full_name(),
                tip.thread.number
            ),
            payment_hash: tip.payment_hash.clone(),
            created_at: tip.created_at,
            updated_at: tip.updated_at,
            expires_at: if tip.state == Core::AwaitingConfirmation {
                tip.confirm_expires_at
            } else {
                tip.expires_at
            },
        }
    }

    /// Loads the people a tip refers to and builds the public shape.
    pub(crate) async fn load(state: &AppState, tip: &StoredTip) -> Result<Self, ApiError> {
        let sender = users::get(state.pool(), tip.sender_id).await?;
        let receiver = match tip.receiver_id {
            Some(id) => Some(users::get(state.pool(), id).await?),
            None => None,
        };
        Ok(Self::from_parts(tip, &sender, receiver.as_ref()))
    }
}

fn user_ref(user: &User) -> UserRef {
    UserRef {
        login: user.github_login.clone(),
        avatar_url: user
            .avatar_url
            .clone()
            .unwrap_or_else(|| fallback_avatar(&user.github_login)),
    }
}

/// GitHub serves every account's avatar at this address, so a ref never lacks one.
fn fallback_avatar(login: &str) -> String {
    format!("https://github.com/{login}.png")
}
