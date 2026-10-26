//! MergeTip job runner: claims jobs from Postgres and dispatches them to handlers.

pub mod cli;
pub mod config;
pub mod handlers;
pub mod runner;
pub mod sampler;

pub use config::WorkerConfig;
pub use runner::{JobContext, JobError, JobHandler, JobOutcome, Runner, RunnerConfig};
