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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Openapi => {
            print!("{}", openapi::json());
            Ok(())
        }
        Command::Serve => serve().await,
    }
}

async fn serve() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let config = Arc::new(ApiConfig::from_env(&ProcessEnv).context("invalid configuration")?);
    telemetry::init(config.app_env);

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
        fiat: match &config.bitnob {
            Some(bitnob) => Some(Arc::new(
                mergetip_fiat::BitnobClient::new(bitnob.clone())
                    .context("cannot set up the Bitnob client")?,
            )),
            None => None,
        },
    };
    let state = AppState::new(pool, config.clone(), clock, services);

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
