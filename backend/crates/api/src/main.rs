//! MergeTip HTTP API server.

use std::sync::Arc;

use anyhow::Context;
use mergetip_api::{ApiConfig, AppState, router};
use mergetip_core::SystemClock;
use mergetip_runtime::{config::ProcessEnv, db, shutdown, telemetry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let config = Arc::new(ApiConfig::from_env(&ProcessEnv).context("invalid configuration")?);
    telemetry::init(config.app_env);

    let pool = db::connect(config.database_url.expose(), 10)
        .await
        .context("cannot connect to the database")?;
    let state = AppState::new(pool, config.clone(), Arc::new(SystemClock));

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("cannot bind {}", config.addr))?;
    tracing::info!(addr = %config.addr, env = %config.app_env, "mergetip-api listening");

    let shutdown = shutdown::on_signal();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("server error")?;
    Ok(())
}
