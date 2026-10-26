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
/// `whsec_` + base64("resend-test-secret").
pub const RESEND_SECRET: &str = "whsec_cmVzZW5kLXRlc3Qtc2VjcmV0";
pub const BITNOB_SECRET: &str = "bitnob-webhook-secret";
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
        ("RESEND_WEBHOOK_SECRET", RESEND_SECRET.to_owned()),
        ("BITNOB_WEBHOOK_SECRET", BITNOB_SECRET.to_owned()),
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
    state_with_mailer(
        pool,
        github_base,
        clock,
        wallets,
        receivers,
        Arc::new(mergetip_notify::DisabledMailer),
    )
}

pub fn state_with_mailer(
    pool: PgPool,
    github_base: &str,
    clock: Arc<dyn Clock>,
    wallets: Arc<dyn WalletConnector>,
    receivers: Arc<dyn ReceiverEndpoint>,
    mailer: Arc<dyn mergetip_notify::Mailer>,
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
            mailer,
            fiat: None,
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

/// Lightning addresses for tests: every address resolves except `down@…` (unreachable) and
/// `bad@…` (not LNURL-pay). Invoices are never needed by the API.
pub struct FakeReceivers;

#[async_trait::async_trait]
impl ReceiverEndpoint for FakeReceivers {
    async fn invoice_for(
        &self,
        _address: &mergetip_payer::LightningAddress,
        _amount: mergetip_core::Sats,
        _comment: Option<&str>,
    ) -> Result<mergetip_payer::ValidatedInvoice, mergetip_payer::ResolveError> {
        Err(mergetip_payer::ResolveError::Invalid(
            "the api never pays".into(),
        ))
    }

    async fn probe(
        &self,
        address: &mergetip_payer::LightningAddress,
    ) -> Result<(), mergetip_payer::ResolveError> {
        match address.user() {
            "down" => Err(mergetip_payer::ResolveError::Unreachable("down".into())),
            "bad" => Err(mergetip_payer::ResolveError::Invalid("bad".into())),
            _ => Ok(()),
        }
    }
}

/// An app whose GitHub is unreachable and whose addresses come from [`FakeReceivers`].
pub fn fake_app(pool: PgPool) -> Router {
    router(state_with(
        pool,
        "http://127.0.0.1:9",
        Arc::new(SystemClock),
        Arc::new(NwcConnector),
        Arc::new(FakeReceivers),
    ))
}

/// Creates (or finds) the GitHub account's user and a web session for it, as signing in does.
/// Returns the raw session token.
pub async fn session_for(pool: &PgPool, github_id: i64, login: &str) -> String {
    let mut conn = pool.acquire().await.unwrap();
    let user = mergetip_store::users::upsert_by_github_id(
        &mut conn,
        &mergetip_store::users::GithubProfile {
            github_id: mergetip_core::GithubUserId::new(github_id),
            login: login.into(),
            avatar_url: None,
            created_at: None,
        },
    )
    .await
    .unwrap();
    let token = mergetip_crypto::random_token().unwrap();
    mergetip_store::sessions::create(
        pool,
        user.id,
        mergetip_store::sessions::SessionKind::Web,
        &token.hash,
        time::OffsetDateTime::now_utc() + time::Duration::days(1),
        None,
    )
    .await
    .unwrap();
    token.raw.expose().clone()
}

/// Sends a request from the web app, signed in with `session` when given, and returns the
/// status and the JSON body (`Null` when empty).
pub async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    session: Option<&str>,
    body: Option<serde_json::Value>,
) -> (axum::http::StatusCode, serde_json::Value) {
    use axum::http::header;
    use tower::ServiceExt;

    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(header::ORIGIN, WEB);
    if let Some(session) = session {
        request = request.header(header::COOKIE, format!("mt_session={session}"));
    }
    let body = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(json.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// An app with fiat payouts on (provider and runtime flag), backed by `fiat`.
pub async fn fiat_app(pool: PgPool, fiat: Arc<mergetip_fiat::testing::FakeFiat>) -> Router {
    mergetip_store::feature_flags::set(&pool, mergetip_store::feature_flags::FIAT_PAYOUTS, true)
        .await
        .unwrap();
    let env = env("http://127.0.0.1:9");
    let env: HashMap<&str, &str> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let config = Arc::new(ApiConfig::from_env(&env).unwrap());
    let oauth = OAuthClient::new(
        config.github_client_id.clone(),
        Redacted::new("client-secret".to_owned()),
        &config.github_oauth_url,
        &config.github_api_url,
    )
    .unwrap();
    router(AppState::new(
        pool,
        config,
        Arc::new(SystemClock),
        Services {
            oauth: Arc::new(oauth),
            wallets: Arc::new(NwcConnector),
            receivers: Arc::new(FakeReceivers),
            mailer: Arc::new(mergetip_notify::DisabledMailer),
            fiat: Some(fiat),
        },
    ))
}

/// A Bitnob-like fake with Nigeria (bank) and Ghana (mobile money) set up, and one account of
/// each that Account Lookup resolves.
pub fn fake_fiat() -> Arc<mergetip_fiat::testing::FakeFiat> {
    use mergetip_fiat::{
        Bank, Corridor, Country, CountryRequirements, FormField, Limit, Rail, ResolvedAccount,
    };

    let fiat = Arc::new(mergetip_fiat::testing::FakeFiat::new());
    {
        let mut state = fiat.state();
        state.countries = vec![
            Country {
                code: "NG".into(),
                name: "Nigeria".into(),
                corridors: vec![Corridor {
                    currency: "NGN".into(),
                    destination_types: vec!["bank".into()],
                }],
            },
            Country {
                code: "GH".into(),
                name: "Ghana".into(),
                corridors: vec![Corridor {
                    currency: "GHS".into(),
                    destination_types: vec!["mobile_money".into()],
                }],
            },
        ];
        let field = |key: &str, label: &str| FormField {
            key: key.into(),
            label: label.into(),
            kind: "string".into(),
            required: true,
            component: Some("input".into()),
            pattern: None,
            min_length: Some(10),
            max_length: Some(10),
            placeholder: None,
            options: None,
        };
        state.requirements.insert(
            "NG".into(),
            CountryRequirements {
                code: "NG".into(),
                name: "Nigeria".into(),
                destination_types: [(
                    "bank".to_owned(),
                    Rail {
                        label: Some("Bank account".into()),
                        fields: vec![
                            field("account_number", "Account number"),
                            field("bank_code", "Bank"),
                        ],
                        banks: Some(vec![Bank {
                            bank_code: "058".into(),
                            bank_name: "GTBank".into(),
                        }]),
                    },
                )]
                .into(),
            },
        );
        state.limits = vec![Limit {
            country: "NG".into(),
            currency: "NGN".into(),
            min_amount: rust_decimal::Decimal::from(1_000),
            max_amount: rust_decimal::Decimal::from(5_000_000),
        }];
        state.accounts.insert(
            "0123456789".into(),
            ResolvedAccount {
                account_name: "Ada Obi".into(),
                bank_name: Some("GTBank".into()),
                bank_code: Some("058".into()),
                is_verified: true,
            },
        );
        state.accounts.insert(
            "+233241234567".into(),
            ResolvedAccount {
                account_name: "Kofi Mensah".into(),
                bank_name: Some("MTN".into()),
                bank_code: Some("MTN".into()),
                is_verified: true,
            },
        );
    }
    fiat
}
