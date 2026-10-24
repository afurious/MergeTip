mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{MASTER_KEY, WEB};
use mergetip_api::router;
use mergetip_core::{GithubUserId, SystemClock};
use mergetip_crypto::{Keyring, random_token};
use mergetip_payer::{
    LnurlClient, Network, PaymentProof, SenderWallet, WalletConnector,
    nwc::{Encryption, Lookup, NwcConnector, PayError, WalletInfo},
    testing::MockWallet,
};
use mergetip_store::{
    sessions::{self, SessionKind},
    users::{self, GithubProfile},
};
use nostr_sdk::prelude::MockRelay;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct Signed {
    app: Router,
    token: String,
    pool: PgPool,
}

async fn signed_in(pool: PgPool, wallets: Arc<dyn WalletConnector>) -> Signed {
    let clock = Arc::new(SystemClock);
    let receivers = LnurlClient::new(Network::Regtest, true, clock.clone()).unwrap();
    let state = common::state_with(
        pool.clone(),
        "http://127.0.0.1:9",
        clock,
        wallets,
        Arc::new(receivers),
    );
    let mut conn = pool.acquire().await.unwrap();
    let user = users::upsert_by_github_id(
        &mut conn,
        &GithubProfile {
            github_id: GithubUserId::new(42),
            login: "alice".into(),
            avatar_url: None,
            created_at: None,
        },
    )
    .await
    .unwrap();
    let token = random_token().unwrap();
    sessions::create(
        &pool,
        user.id,
        SessionKind::Web,
        &token.hash,
        OffsetDateTime::now_utc() + Duration::days(1),
        None,
    )
    .await
    .unwrap();
    Signed {
        app: router(state),
        token: token.raw.expose().clone(),
        pool,
    }
}

impl Signed {
    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, format!("mt_session={}", self.token))
            .header(header::ORIGIN, WEB);
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        self.app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }
}

async fn json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 16).await.unwrap()).unwrap()
}

async fn error_code(response: Response) -> String {
    json(response).await["error"]["code"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A wallet connector that always fails or succeeds the same way.
struct FakeConnector(Result<WalletInfo, PayError>);
struct FakeWallet(Result<WalletInfo, PayError>);

#[async_trait]
impl WalletConnector for FakeConnector {
    async fn connect(&self, _uri: &str) -> Result<Box<dyn SenderWallet>, PayError> {
        match &self.0 {
            Err(PayError::InvalidUri) => Err(PayError::InvalidUri),
            other => Ok(Box::new(FakeWallet(other.clone()))),
        }
    }
}

#[async_trait]
impl SenderWallet for FakeWallet {
    async fn get_info(&self) -> Result<WalletInfo, PayError> {
        self.0.clone()
    }
    async fn pay_invoice(
        &self,
        _: &str,
        _: &str,
        _: std::time::Duration,
    ) -> Result<PaymentProof, PayError> {
        unreachable!("settings never pay")
    }
    async fn lookup_invoice(&self, _: &str) -> Result<Lookup, PayError> {
        unreachable!("settings never look up")
    }
}

fn info(methods: &[&str]) -> WalletInfo {
    WalletInfo {
        methods: methods.iter().map(|m| (*m).to_owned()).collect(),
        encryption: Encryption::Nip44,
        wallet_pubkey: "ab".repeat(32),
        relays: vec!["wss://relay.example".into()],
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn connecting_a_real_wallet_stores_it_encrypted(pool: PgPool) -> sqlx::Result<()> {
    let relay = MockRelay::run().await.unwrap();
    let wallet = MockWallet::start(&relay.url().await.to_string(), [3; 32])
        .await
        .unwrap();
    let s = signed_in(pool, Arc::new(NwcConnector)).await;

    let response = s
        .send(
            "PUT",
            "/v1/me/wallet",
            Some(json!({ "nwc_uri": wallet.uri() })),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["wallet"]["connected"], true);
    assert!(
        body["wallet"]["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("pay_invoice"))
    );
    assert!(
        !body.to_string().contains("nostr+walletconnect"),
        "the URI is never echoed"
    );

    let row = sqlx::query!("SELECT id, nwc_uri_ciphertext, encryption FROM wallet_connections")
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(row.encryption, "nip44");
    assert!(!String::from_utf8_lossy(&row.nwc_uri_ciphertext).contains("walletconnect"));
    let keyring = Keyring::from_hex(MASTER_KEY, None).unwrap();
    let plain = keyring
        .open(
            &row.nwc_uri_ciphertext,
            format!("wallet_connections.nwc_uri:{}", row.id).as_bytes(),
        )
        .unwrap();
    assert_eq!(plain.expose().as_slice(), wallet.uri().as_bytes());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn reconnecting_replaces_and_disconnecting_revokes(pool: PgPool) -> sqlx::Result<()> {
    let s = signed_in(
        pool,
        Arc::new(FakeConnector(Ok(info(&["pay_invoice", "get_info"])))),
    )
    .await;
    for _ in 0..2 {
        let response = s
            .send("PUT", "/v1/me/wallet", Some(json!({ "nwc_uri": "x" })))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let active = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM wallet_connections WHERE revoked_at IS NULL"#
    )
    .fetch_one(&s.pool)
    .await?;
    assert_eq!(active, 1);

    assert_eq!(
        s.send("DELETE", "/v1/me/wallet", None).await.status(),
        StatusCode::NO_CONTENT
    );
    let body = json(s.send("GET", "/v1/me", None).await).await;
    assert_eq!(
        body["wallet"],
        json!({ "connected": false, "relay": null, "methods": [] })
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn unusable_wallets_are_refused_with_specific_codes(pool: PgPool) -> sqlx::Result<()> {
    let cases = [
        (Err(PayError::InvalidUri), "nwc_invalid_uri"),
        (Err(PayError::Timeout), "nwc_unreachable"),
        (Err(PayError::Relay("down".into())), "nwc_unreachable"),
        (
            Ok(info(&["get_info", "get_balance"])),
            "nwc_missing_pay_invoice",
        ),
    ];
    for (result, code) in cases {
        let s = signed_in(pool.clone(), Arc::new(FakeConnector(result))).await;
        let response = s
            .send("PUT", "/v1/me/wallet", Some(json!({ "nwc_uri": "x" })))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{code}"
        );
        assert_eq!(error_code(response).await, code);
        sqlx::query!("DELETE FROM sessions").execute(&pool).await?;
        sqlx::query!("DELETE FROM users").execute(&pool).await?;
    }
    let stored = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM wallet_connections"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn lightning_addresses_are_checked_before_saving(pool: PgPool) -> sqlx::Result<()> {
    let lnurl = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/lnurlp/alice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tag": "payRequest", "callback": format!("{}/cb", lnurl.uri()),
            "minSendable": 1000, "maxSendable": 1000000000, "metadata": "[]"
        })))
        .mount(&lnurl)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/lnurlp/down"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&lnurl)
        .await;
    let s = signed_in(pool, Arc::new(NwcConnector)).await;
    let host = lnurl.address().to_string();

    let response = s
        .send(
            "PUT",
            "/v1/me/lightning-address",
            Some(json!({ "address": format!("Alice@{host}") })),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json(response).await["lightning_address"],
        format!("alice@{host}")
    );

    for (address, code) in [
        ("not-an-address".to_owned(), "lnurl_invalid"),
        (format!("nobody@{host}"), "lnurl_invalid"),
        (format!("down@{host}"), "lnurl_unreachable"),
    ] {
        let response = s
            .send(
                "PUT",
                "/v1/me/lightning-address",
                Some(json!({ "address": address })),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{address}"
        );
        assert_eq!(error_code(response).await, code, "{address}");
    }
    let saved = sqlx::query_scalar!("SELECT lightning_address FROM users")
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(
        saved,
        Some(format!("alice@{host}")),
        "failed checks keep the old address"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn settings_are_validated_and_partial(pool: PgPool) -> sqlx::Result<()> {
    let s = signed_in(pool, Arc::new(NwcConnector)).await;

    let response = s
        .send(
            "PATCH",
            "/v1/me/settings",
            Some(json!({ "daily_max_sats": 200000, "per_tip_max_sats": 20000 })),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["caps"]["per_tip_max_sats"], 20000);
    assert_eq!(body["caps"]["daily_max_sats"], 200000);
    assert_eq!(
        body["confirm_threshold_sats"], 50000,
        "untouched fields keep their value"
    );

    let response = s
        .send(
            "PATCH",
            "/v1/me/settings",
            Some(json!({ "confirm_otp_enabled": true })),
        )
        .await;
    assert_eq!(json(response).await["confirm_otp_enabled"], true);

    for invalid in [
        json!({ "per_tip_max_sats": 0 }),
        json!({ "per_tip_max_sats": 300000 }),
        json!({ "confirm_threshold_sats": 999 }),
        json!({ "daily_max_sats": 100000001u64 }),
    ] {
        let response = s
            .send("PATCH", "/v1/me/settings", Some(invalid.clone()))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{invalid}"
        );
        assert_eq!(error_code(response).await, "invalid_setting");
    }
    let response = s
        .send(
            "PATCH",
            "/v1/me/settings",
            Some(json!({ "is_admin": true })),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown fields are refused"
    );
    Ok(())
}
