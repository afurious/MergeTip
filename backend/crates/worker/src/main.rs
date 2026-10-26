//! MergeTip job runner: processes events, pays tips, retries and expires pledges.

use std::sync::Arc;

use anyhow::Context;
use mergetip_core::SystemClock;
use mergetip_engine::{Engine, EngineSettings, Payments};
use mergetip_github::OctocrabGitHub;
use mergetip_payer::{LnurlClient, nwc::NwcConnector};
use mergetip_runtime::{config::ProcessEnv, db, shutdown, telemetry};
use mergetip_worker::{
    Runner, WorkerConfig,
    handlers::{
        ExpireTipsHandler, NoopHandler, NotifyPledgeHandler, PayTipHandler, PostReplyHandler,
        ProcessEventHandler, ReconcilePaymentHandler, ResolveHeldHandler, ResumeSenderTipsHandler,
        SendDigestHandler, VerifyTipHandler,
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
    // Plain-HTTP Lightning addresses are only for the local mock server.
    let receivers = LnurlClient::new(
        config.lightning_network,
        config.app_env.is_dev(),
        clock.clone(),
    )
    .context("cannot set up the LNURL client")?;
    let mut settings = EngineSettings::new(config.web_base_url.clone(), config.stub_payer);
    settings.api_base_url = config.api_base_url.clone();
    settings.production = config.app_env == mergetip_runtime::AppEnv::Production;
    let mailer =
        mergetip_notify::from_config(&config.mailer).context("cannot set up email delivery")?;
    let engine = Arc::new(
        Engine::new(
            pool.clone(),
            Arc::new(github),
            clock.clone(),
            settings,
            Payments {
                receivers: Arc::new(receivers),
                wallets: Arc::new(NwcConnector),
                keyring: config.keyring.clone(),
            },
        )
        .with_mailer(mailer),
    );

    // The recurring expiry job; a no-op when another worker already created it.
    let mut conn = pool.acquire().await.context("cannot reach the database")?;
    mergetip_engine::jobs::ensure_expiry_job(&mut conn)
        .await
        .context("cannot schedule the expiry job")?;
    drop(conn);

    if config.stub_payer {
        tracing::warn!("STUB_PAYER is on: tips are marked paid without moving any money");
    }
    Runner::new(pool, clock, config.runner)
        .context("cannot initialise the job runner")?
        .register(NoopHandler)
        .register(ProcessEventHandler(engine.clone()))
        .register(VerifyTipHandler(engine.clone()))
        .register(PostReplyHandler(engine.clone()))
        .register(PayTipHandler(engine.clone()))
        .register(ResolveHeldHandler(engine.clone()))
        .register(ResumeSenderTipsHandler(engine.clone()))
        .register(NotifyPledgeHandler(engine.clone()))
        .register(SendDigestHandler(engine.clone()))
        .register(ExpireTipsHandler(engine.clone()))
        .register(ReconcilePaymentHandler(engine))
        .run(shutdown::on_signal())
        .await;
    Ok(())
}
