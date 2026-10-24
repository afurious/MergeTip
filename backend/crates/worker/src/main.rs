//! MergeTip job runner: processes events, pays tips, retries and expires pledges.

use std::sync::Arc;

use anyhow::Context;
use mergetip_core::SystemClock;
use mergetip_runtime::{config::ProcessEnv, db, shutdown, telemetry};
use mergetip_worker::{Runner, WorkerConfig, handlers::NoopHandler};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let config = WorkerConfig::from_env(&ProcessEnv).context("invalid configuration")?;
    telemetry::init(config.app_env);

    let pool = db::connect(config.database_url.expose(), config.pool_size())
        .await
        .context("cannot connect to the database")?;
    db::migrate(&pool).await.context("migrations failed")?;

    let shutdown = shutdown::on_signal();
    Runner::new(pool, Arc::new(SystemClock), config.runner)
        .context("cannot initialise the job runner")?
        .register(NoopHandler)
        .run(shutdown)
        .await;
    Ok(())
}
