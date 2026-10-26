//! Postgres repositories, migrations and the `FOR UPDATE SKIP LOCKED` job queue.
//!
//! Functions take `impl PgExecutor` when they run a single statement (pass a pool or a
//! transaction) and `&mut PgConnection` when they need their own nested transaction.

pub mod bot_comments;
mod error;
pub mod feature_flags;
pub mod github_events;
pub mod installations;
pub mod jobs;
pub mod payment_attempts;
pub mod sent_replies;
pub mod sessions;
pub mod tips;
pub mod users;
pub mod wallets;

pub use error::{StoreError, StoreResult};
pub use sqlx::PgPool;

/// Embedded migrations from `backend/migrations`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
