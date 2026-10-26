//! Mock NWC wallet: answers NIP-47 requests on a relay, paying invoices minted by `mock-lnurl`
//! with the same `MOCK_SEED`. Prints the connection string to paste into the web app.
//!
//! Magic amounts (sats): 666 insufficient balance, 777 pays but the answer is lost,
//! 888 quota exceeded, 999 never answers.
//!
//! Environment:
//! - `RELAY_URL`: relay to connect to (`ws://relay:8080` inside Docker Compose).
//! - `PUBLIC_RELAY_URL`: relay written into the URI, as clients reach it (`ws://localhost:7000`).
//! - `MOCK_SEED`: 32-byte hex, shared with `mock-lnurl`.
//! - `MOCK_NWC_WALLET_SECRET`, `MOCK_NWC_APP_SECRET`: optional 32-byte hex keys, for a URI that
//!   stays the same across restarts.

use anyhow::{Context, bail};
use mergetip_payer::testing::{MockWallet, MockWalletOptions};
use tracing_subscriber::EnvFilter;

fn hex32(name: &str, value: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(value.trim()).with_context(|| format!("{name} is not hex"))?;
    match <[u8; 32]>::try_from(bytes.as_slice()) {
        Ok(array) => Ok(array),
        Err(_) => bail!("{name} must be 32 bytes of hex"),
    }
}

fn optional_hex32(name: &str) -> anyhow::Result<Option<[u8; 32]>> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => hex32(name, &value).map(Some),
        _ => Ok(None),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let relay = std::env::var("RELAY_URL").unwrap_or_else(|_| "ws://localhost:7000".into());
    let seed = hex32(
        "MOCK_SEED",
        &std::env::var("MOCK_SEED").context("MOCK_SEED is required")?,
    )?;
    let wallet = MockWallet::start_with(MockWalletOptions {
        relay,
        advertised_relay: std::env::var("PUBLIC_RELAY_URL").ok(),
        seed,
        wallet_secret: optional_hex32("MOCK_NWC_WALLET_SECRET")?,
        app_secret: optional_hex32("MOCK_NWC_APP_SECRET")?,
    })
    .await
    .context("cannot start the mock wallet")?;
    // A mock wallet holds no real funds, so printing its connection string is fine here.
    tracing::info!(
        uri = wallet.uri(),
        "mock NWC wallet ready; paste this URI into the web app"
    );

    tokio::signal::ctrl_c().await?;
    wallet.stop().await;
    Ok(())
}
