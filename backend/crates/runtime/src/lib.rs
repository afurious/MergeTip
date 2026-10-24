//! Process plumbing shared by the `api` and `worker` binaries, so both start, log and shut down
//! the same way. No business logic lives here.

pub mod config;
pub mod db;
pub mod shutdown;
pub mod telemetry;

pub use config::{AppEnv, ConfigError};
