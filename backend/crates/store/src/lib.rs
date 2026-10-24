//! Postgres repositories, migrations and the `FOR UPDATE SKIP LOCKED` job queue.
//!
//! Functions take `impl PgExecutor` when they run a single statement (pass a pool or a
//! transaction) and `&mut PgConnection` when they need their own nested transaction.

mod error;
pub mod github_events;
pub mod installations;
pub mod jobs;
pub mod tips;
pub mod users;

pub use error::{StoreError, StoreResult};
pub use sqlx::PgPool;

/// Embedded migrations from `backend/migrations`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
