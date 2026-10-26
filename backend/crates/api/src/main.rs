//! MergeTip HTTP API server.

use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use mergetip_api::{ApiConfig, AppState, Services, openapi, router};
use mergetip_core::SystemClock;
use mergetip_github::oauth::OAuthClient;
use mergetip_payer::{LnurlClient, nwc::NwcConnector};
use mergetip_runtime::{config::ProcessEnv, db, shutdown, telemetry};

#[derive(Debug, Parser)]
#[command(name = "mergetip-api", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the HTTP API (the default).
    Serve,
    /// Print the OpenAPI document to stdout.
    Openapi,
    /// Apply pending database migrations and exit (Fly `release_command`). Migrations are
    /// additive, so the running version keeps working while the new one rolls out.
    Migrate,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Openapi => {
            print!("{}", openapi::json());
            Ok(())
        }
        Command::Serve => serve().await,
        Command::Migrate => migrate().await,
    }
}

async fn migrate() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let app_env = std::env::var("APP_ENV")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(mergetip_runtime::AppEnv::Dev);
    let sentry_dsn = std::env::var("SENTRY_DSN")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let _logs = telemetry::init(app_env, sentry_dsn.as_deref());
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let pool = db::connect(&url, 1)
        .await
        .context("cannot connect to the database")?;
    db::migrate(&pool).await.context("migrations failed")?;
    tracing::info!("migrations applied");
    Ok(())
}

async fn serve() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let config = Arc::new(ApiConfig::from_env(&ProcessEnv).context("invalid configuration")?);
    let _sentry = telemetry::init(
        config.app_env,
        config.sentry_dsn.as_ref().map(|dsn| dsn.expose().as_str()),
    );
    if let Some(addr) = config.metrics_addr {
        mergetip_runtime::metrics::install(addr).context("cannot start the metrics exporter")?;
    }

    let pool = db::connect(config.database_url.expose(), 10)
        .await
        .context("cannot connect to the database")?;
    let oauth = OAuthClient::new(
        config.github_client_id.clone(),
        config.github_client_secret.clone(),
        &config.github_oauth_url,
        &config.github_api_url,
    )
    .context("cannot set up GitHub sign-in")?;
    let clock = Arc::new(SystemClock);
    let receivers = LnurlClient::new(
        config.lightning_network,
        config.app_env.is_dev(),
        clock.clone(),
    )
    .context("cannot set up the LNURL client")?;
    let services = Services {
        oauth: Arc::new(oauth),
        wallets: Arc::new(NwcConnector),
        receivers: Arc::new(receivers),
        mailer: mergetip_notify::from_config(&config.mailer)
            .context("cannot set up email delivery")?,
    };
    let state = AppState::new(pool, config.clone(), clock, services);

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("cannot bind {}", config.addr))?;
    tracing::info!(addr = %config.addr, env = %config.app_env, "mergetip-api listening");

    let shutdown = shutdown::on_signal();
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.cancelled_owned())
    .await
    .context("server error")?;
    Ok(())
}
