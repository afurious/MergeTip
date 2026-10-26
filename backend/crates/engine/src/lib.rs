//! Tip use cases: turning GitHub events into tips, verifying them, paying them and replying in the
//! thread. Shared by the `api` (which enqueues work) and the `worker` (which runs it).
//!
//! Every step that changes a tip's state does so in one transaction together with enqueuing the
//! follow-up jobs, so a crash can never leave a tip in a state with no job to move it on.

mod comments;
mod error;
mod events;
pub mod jobs;
mod pay;
mod replies;
mod verify;

use std::sync::Arc;

use mergetip_core::Clock;
use mergetip_crypto::Keyring;
use mergetip_github::GitHubApi;
use mergetip_payer::{ReceiverEndpoint, WalletConnector};
use mergetip_store::PgPool;

pub use error::EngineError;
pub use pay::{MAX_PAYMENT_ATTEMPTS, PAY_TIMEOUT, ReconcileOutcome};
pub use verify::{Decision, decide};

/// Tips a sender may create per rolling hour (PRD "Limits").
pub const TIPS_PER_HOUR: u32 = 20;
/// Tips from one sender to one receiver per thread.
pub const TIPS_PER_PAIR: u32 = 3;

#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// The web app's base URL, for links in bot replies (no trailing slash).
    pub web_base_url: String,
    /// Staging only: pay tips with a fake proof instead of a wallet, and treat every sender as
    /// having a wallet. Configuration refuses it in production.
    pub stub_payer: bool,
    /// How long the sender's wallet gets to pay before the outcome counts as unknown.
    pub pay_timeout: std::time::Duration,
}

impl EngineSettings {
    pub fn new(web_base_url: impl Into<String>, stub_payer: bool) -> Self {
        Self {
            web_base_url: web_base_url.into(),
            stub_payer,
            pay_timeout: pay::PAY_TIMEOUT,
        }
    }
}

/// What paying needs: the receiver side (LNURL), the sender side (NWC) and the keyring that
/// decrypts NWC URIs. Only the worker builds an engine with a real keyring.
pub struct Payments {
    pub receivers: Arc<dyn ReceiverEndpoint>,
    pub wallets: Arc<dyn WalletConnector>,
    pub keyring: Arc<Keyring>,
}

pub struct Engine {
    pool: PgPool,
    github: Arc<dyn GitHubApi>,
    clock: Arc<dyn Clock>,
    settings: EngineSettings,
    payments: Payments,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Engine {
    pub fn new(
        pool: PgPool,
        github: Arc<dyn GitHubApi>,
        clock: Arc<dyn Clock>,
        settings: EngineSettings,
        payments: Payments,
    ) -> Self {
        Self {
            pool,
            github,
            clock,
            settings,
            payments,
        }
    }

    pub fn settings(&self) -> &EngineSettings {
        &self.settings
    }

    fn link(&self, path: &str) -> String {
        format!("{}{path}", self.settings.web_base_url.trim_end_matches('/'))
    }
}
