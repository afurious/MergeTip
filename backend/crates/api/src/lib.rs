//! MergeTip HTTP API: webhooks, auth and the `/v1` REST surface.
//!
//! Handlers stay thin: they parse input, call into `store` (and, from M2, `engine`), and map
//! results to the error contract in [`error`].

pub mod config;
mod cookies;
pub mod error;
pub mod extract;
pub mod github_token;
pub mod hardening;
pub mod openapi;
pub mod routes;
pub mod state;

pub use config::ApiConfig;
pub use error::ApiError;
pub use routes::router;
pub use state::{AppState, Services};
