//! MergeTip job runner: processes events, pays tips, retries and expires pledges.

use std::sync::Arc;

use anyhow::Context;
use mergetip_core::SystemClock;
use mergetip_engine::{Engine, EngineSettings};
use mergetip_github::OctocrabGitHub;
use mergetip_runtime::{config::ProcessEnv, db, shutdown, telemetry};
use mergetip_worker::{
    Runner, WorkerConfig,
    handlers::{
        NoopHandler, PayTipStubHandler, PostReplyHandler, ProcessEventHandler, VerifyTipHandler,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let config = WorkerConfig::from_env(&ProcessEnv).context("invalid configuration")?;
    telemetry::init(config.app_env);

    let pool = db::connect(config.database_url.expose(), config.pool_size())
        .await
        .context("cannot connect to the database")?;
    db::migrate(&pool).await.context("migrations failed")?;

    let key_path = &config.github.private_key_path;
    let private_key = std::fs::read(key_path)
        .with_context(|| format!("cannot read the GitHub App key at {}", key_path.display()))?;
    let github = OctocrabGitHub::from_app_key(
        config.github.app_id,
        &private_key,
        config.github.api_url.as_deref(),
    )
    .context("cannot set up the GitHub App client")?;

    let clock = Arc::new(SystemClock);
    let engine = Arc::new(Engine::new(
        pool.clone(),
        Arc::new(github),
        clock.clone(),
        EngineSettings {
            web_base_url: config.web_base_url.clone(),
            stub_payer: config.stub_payer,
        },
    ));

    let mut runner = Runner::new(pool, clock, config.runner)
        .context("cannot initialise the job runner")?
        .register(NoopHandler)
        .register(ProcessEventHandler(engine.clone()))
        .register(VerifyTipHandler(engine.clone()))
        .register(PostReplyHandler(engine.clone()));
    if config.stub_payer {
        tracing::warn!("STUB_PAYER is on: tips are marked paid without moving any money");
        runner = runner.register(PayTipStubHandler(engine));
    }
    runner.run(shutdown::on_signal()).await;
    Ok(())
}
