//! Domain types, the tip state machine and shared errors. Pure: no I/O, no database types.

pub mod clock;
pub mod ids;
pub mod money;
pub mod state;
pub mod thread;
pub mod tip;

pub use clock::{Clock, FixedClock, SystemClock};
pub use ids::{GithubInstallationId, GithubUserId, InstallationId, TipId, UserId};
pub use money::{AmountOutOfRange, Msats, Sats};
pub use state::{IllegalTransition, TipState, UnknownTipState, transition};
pub use thread::ThreadRef;
pub use tip::{TipKind, UnknownTipKind};
