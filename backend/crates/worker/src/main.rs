//! MergeTip job runner: processes events, pays tips, retries and expires pledges.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use mergetip_core::SystemClock;
use mergetip_core::TipId;
use mergetip_engine::{Engine, EngineSettings, Payments};
use mergetip_github::OctocrabGitHub;
use mergetip_payer::{LnurlClient, nwc::NwcConnector};
use mergetip_runtime::{config::ProcessEnv, db, shutdown, telemetry};
use mergetip_worker::{
    Runner, WorkerConfig,
    cli::{Cli, Command, OpsCommand},
    handlers::{
        ExpireTipsHandler, NoopHandler, NotifyPledgeHandler, PayTipHandler, PostReplyHandler,
        ProcessBitnobEventHandler, ProcessEventHandler, ReconcilePaymentHandler,
        ResolveHeldHandler, ResumeSenderTipsHandler, SendDigestHandler, VerifyTipHandler,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let config = WorkerConfig::from_env(&ProcessEnv).context("invalid configuration")?;
    telemetry::init(config.app_env);

    let pool = db::connect(config.database_url.expose(), config.pool_size())
        .await
        .context("cannot connect to the database")?;
    db::migrate(&pool).await.context("migrations failed")?;

    if let Some(Command::Ops(command)) = cli.command {
        return run_ops(&pool, &config, command).await;
    }

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
    let engine = Engine::new(
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
    .with_mailer(mailer);
    let engine = Arc::new(match &config.bitnob {
        Some(bitnob) => engine.with_fiat(
            Arc::new(
                mergetip_fiat::BitnobClient::new(bitnob.clone())
                    .context("cannot set up the Bitnob client")?,
            ),
            config.lightning_network,
        ),
        None => engine,
    });

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
        .register(ProcessBitnobEventHandler(engine.clone()))
        .register(ReconcilePaymentHandler(engine))
        .run(shutdown::on_signal())
        .await;
    Ok(())
}

/// One operator action, then exit. Prints the tip's new state for the runbook's check step.
async fn run_ops(
    pool: &mergetip_store::PgPool,
    config: &WorkerConfig,
    command: OpsCommand,
) -> anyhow::Result<()> {
    let state = match command {
        OpsCommand::FiatRetry { tip_id, operator } => {
            let bitnob = config
                .bitnob
                .clone()
                .context("FIAT_PAYOUTS_ENABLED and the Bitnob credentials are needed to re-poll")?;
            let provider = mergetip_fiat::BitnobClient::new(bitnob)
                .context("cannot set up the Bitnob client")?;
            mergetip_engine::ops::fiat_retry(pool, &provider, TipId::from_uuid(tip_id), &operator)
                .await?
        }
        OpsCommand::FiatRefund {
            tip_id,
            bolt11,
            operator,
        } => {
            mergetip_engine::ops::fiat_refund(
                pool,
                TipId::from_uuid(tip_id),
                &bolt11,
                config.lightning_network,
                &operator,
                time::OffsetDateTime::now_utc(),
            )
            .await?
        }
    };
    tracing::info!(%state, "operator action done");
    Ok(())
}
