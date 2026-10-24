//! Building an api router for tests.

#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc};

use axum::Router;
use mergetip_api::{ApiConfig, AppState, Services, router};
use mergetip_core::{Clock, SystemClock};
use mergetip_crypto::Redacted;
use mergetip_github::oauth::OAuthClient;
use mergetip_payer::{LnurlClient, Network, ReceiverEndpoint, WalletConnector, nwc::NwcConnector};
use sqlx::PgPool;

pub const WEB: &str = "http://localhost:5173";
pub const API: &str = "http://localhost:8080";
pub const WEBHOOK_SECRET: &str = "test-secret";
pub const MASTER_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

pub fn env(github_base: &str) -> HashMap<&'static str, String> {
    HashMap::from([
        ("MASTER_KEY", MASTER_KEY.to_owned()),
        ("DATABASE_URL", "postgres://unused/db".to_owned()),
        ("PUBLIC_BASE_URL", WEB.to_owned()),
        ("API_PUBLIC_URL", API.to_owned()),
        ("GITHUB_WEBHOOK_SECRET", WEBHOOK_SECRET.to_owned()),
        ("GITHUB_CLIENT_ID", "Iv1.test".to_owned()),
        ("GITHUB_CLIENT_SECRET", "client-secret".to_owned()),
        ("GITHUB_OAUTH_URL", github_base.to_owned()),
        ("GITHUB_API_URL", github_base.to_owned()),
        ("LIGHTNING_NETWORK", "regtest".to_owned()),
    ])
}

pub fn state(pool: PgPool, github_base: &str, clock: Arc<dyn Clock>) -> AppState {
    let receivers = LnurlClient::new(Network::Regtest, true, clock.clone()).unwrap();
    state_with(
        pool,
        github_base,
        clock,
        Arc::new(NwcConnector),
        Arc::new(receivers),
    )
}

pub fn state_with(
    pool: PgPool,
    github_base: &str,
    clock: Arc<dyn Clock>,
    wallets: Arc<dyn WalletConnector>,
    receivers: Arc<dyn ReceiverEndpoint>,
) -> AppState {
    let env = env(github_base);
    let env: HashMap<&str, &str> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let config = Arc::new(ApiConfig::from_env(&env).unwrap());
    let oauth = OAuthClient::new(
        config.github_client_id.clone(),
        Redacted::new("client-secret".to_owned()),
        &config.github_oauth_url,
        &config.github_api_url,
    )
    .unwrap();
    AppState::new(
        pool,
        config,
        clock,
        Services {
            oauth: Arc::new(oauth),
            wallets,
            receivers,
        },
    )
}

pub fn app(pool: PgPool, github_base: &str) -> Router {
    router(state(pool, github_base, Arc::new(SystemClock)))
}

/// A router whose routes never touch the database or GitHub.
pub fn offline_app() -> Router {
    let pool = PgPool::connect_lazy("postgres://unused/db").unwrap();
    app(pool, "http://127.0.0.1:9")
}
