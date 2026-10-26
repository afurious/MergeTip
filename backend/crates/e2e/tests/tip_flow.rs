//! A `/tip` comment, end to end: sign in → connect a wallet → signed webhook → api → Postgres
//! queue → worker → a real NWC payment → GitHub replies.
//!
//! Everything is real except the outside world: GitHub is a `wiremock` server driven through the
//! production octocrab and OAuth clients, the receiver's Lightning address is a `wiremock` LNURL
//! server minting real signed invoices, and the sender's wallet is the NIP-47 mock wallet on an
//! in-process relay, reached through the production NWC client.

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use mergetip_api::{ApiConfig, AppState, Services, router};
use mergetip_core::{Msats, SystemClock};
use mergetip_crypto::Keyring;
use mergetip_engine::{Engine, EngineSettings, Payments};
use mergetip_github::{OctocrabGitHub, oauth::OAuthClient};
use mergetip_payer::{
    LnurlClient, Network,
    nwc::NwcConnector,
    testing::{MintParams, MockWallet, behaviour, mint_invoice},
};
use mergetip_worker::{
    Runner, RunnerConfig,
    handlers::{
        PayTipHandler, PostReplyHandler, ProcessEventHandler, ReconcilePaymentHandler,
        ResolveHeldHandler, ResumeSenderTipsHandler, VerifyTipHandler,
    },
};
use nostr_sdk::prelude::MockRelay;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate,
    matchers::{body_string_contains, header as has_header, method, path, path_regex, query_param},
};

const WEB: &str = "https://mergetip.dev";
const WEBHOOK_SECRET: &str = "e2e-secret";
const MASTER_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const SEED: [u8; 32] = [9; 32];
const METADATA: &str = r#"[["text/plain","Sats for reviewer"]]"#;
const FIXTURE: &str = include_str!("fixtures/issue_comment_created.json");

/// LNURL-pay for any user: the callback mints a real invoice for the requested amount, payable
/// by the mock wallet because both share [`SEED`]. `wrongamount` asks for 1 sat more.
struct Lnurl {
    base: String,
}

impl Respond for Lnurl {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let url_path = request.url.path();
        if let Some(user) = url_path.strip_prefix("/.well-known/lnurlp/") {
            return ResponseTemplate::new(200).set_body_json(json!({
                "tag": "payRequest",
                "callback": format!("{}/cb/{user}", self.base),
                "minSendable": 1_000, "maxSendable": 1_000_000_000,
                "metadata": METADATA, "commentAllowed": 140
            }));
        }
        let user = url_path.trim_start_matches("/cb/");
        let amount: u64 = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "amount")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let amount = if user == "wrongamount" {
            amount + 1_000
        } else {
            amount
        };
        let minted = mint_invoice(&MintParams {
            amount: Msats::new(amount),
            metadata: METADATA.into(),
            seed: SEED,
            ..MintParams::default()
        });
        ResponseTemplate::new(200).set_body_json(json!({ "pr": minted.bolt11, "routes": [] }))
    }
}

/// How a worker process is run, so a test can kill one mid-payment and start another.
struct WorkerOptions {
    pay_timeout: Duration,
    lease: Duration,
    drain_timeout: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            pay_timeout: Duration::from_secs(10),
            lease: Duration::from_secs(30),
            drain_timeout: Duration::from_secs(5),
        }
    }
}

struct Worker {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Worker {
    async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.unwrap();
    }
}

struct System {
    pool: PgPool,
    api: Router,
    github: MockServer,
    lnurl: MockServer,
    wallet: MockWallet,
    _relay: MockRelay,
}

impl System {
    async fn start(pool: PgPool) -> Self {
        let github = MockServer::start().await;
        mock_github(&github).await;
        let lnurl = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(".*"))
            .respond_with(Lnurl { base: lnurl.uri() })
            .mount(&lnurl)
            .await;
        let relay = MockRelay::run().await.unwrap();
        let wallet = MockWallet::start(&relay.url().await.to_string(), SEED)
            .await
            .unwrap();

        let env = HashMap::from([
            ("MASTER_KEY", MASTER_KEY),
            ("DATABASE_URL", "postgres://unused/db"),
            ("PUBLIC_BASE_URL", WEB),
            ("GITHUB_WEBHOOK_SECRET", WEBHOOK_SECRET),
            ("API_PUBLIC_URL", "https://api.mergetip.dev"),
            ("GITHUB_CLIENT_ID", "Iv1.e2e"),
            ("GITHUB_CLIENT_SECRET", "e2e-client-secret"),
            ("LIGHTNING_NETWORK", "regtest"),
        ]);
        let config = Arc::new(ApiConfig::from_env(&env).unwrap());
        let oauth = OAuthClient::new(
            config.github_client_id.clone(),
            config.github_client_secret.clone(),
            &github.uri(),
            &github.uri(),
        )
        .unwrap();
        let services = Services {
            oauth: Arc::new(oauth),
            wallets: Arc::new(NwcConnector),
            receivers: Arc::new(
                LnurlClient::new(Network::Regtest, true, Arc::new(SystemClock)).unwrap(),
            ),
        };
        let api = router(AppState::new(
            pool.clone(),
            config,
            Arc::new(SystemClock),
            services,
        ));

        Self {
            pool,
            api,
            github,
            lnurl,
            wallet,
            _relay: relay,
        }
    }

    /// A worker process: the production runner and handlers around a real-payments engine.
    fn worker(&self, options: WorkerOptions) -> Worker {
        let client = OctocrabGitHub::with_token("installation-token", &self.github.uri()).unwrap();
        let mut settings = EngineSettings::new(WEB, false);
        settings.pay_timeout = options.pay_timeout;
        let engine = Arc::new(Engine::new(
            self.pool.clone(),
            Arc::new(client),
            Arc::new(SystemClock),
            settings,
            Payments {
                receivers: Arc::new(
                    LnurlClient::new(Network::Regtest, true, Arc::new(SystemClock)).unwrap(),
                ),
                wallets: Arc::new(NwcConnector),
                keyring: Arc::new(Keyring::from_hex(MASTER_KEY, None).unwrap()),
            },
        ));
        let runner = Runner::new(
            self.pool.clone(),
            Arc::new(SystemClock),
            RunnerConfig {
                concurrency: 4,
                lease: options.lease,
                poll_interval: Duration::from_millis(50),
                drain_timeout: options.drain_timeout,
                retry_base: Duration::from_millis(50),
                retry_cap: Duration::from_millis(200),
            },
        )
        .unwrap()
        .register(ProcessEventHandler(engine.clone()))
        .register(VerifyTipHandler(engine.clone()))
        .register(PostReplyHandler(engine.clone()))
        .register(PayTipHandler(engine.clone()))
        .register(ResolveHeldHandler(engine.clone()))
        .register(ResumeSenderTipsHandler(engine.clone()))
        .register(ReconcilePaymentHandler(engine));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(runner.run(shutdown.clone()));
        Worker { shutdown, task }
    }

    async fn call(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response: Response = self.api.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Signs in through GitHub OAuth, as the browser would, and returns the session cookie.
    async fn sign_in(&self, code: &str) -> String {
        let login = self
            .api
            .clone()
            .oneshot(
                Request::get("/v1/auth/github/login?redirect=/settings/wallet")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let state = login.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_owned();
        let oauth_cookie = cookie(&login, "mt_oauth");
        let callback = self
            .api
            .clone()
            .oneshot(
                Request::get(format!(
                    "/v1/auth/github/callback?code={code}&state={state}"
                ))
                .header(header::COOKIE, oauth_cookie)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(callback.status(), StatusCode::SEE_OTHER);
        cookie(&callback, "mt_session")
    }

    /// A cookie-authenticated JSON write from the web app.
    async fn write(
        &self,
        verb: &str,
        uri: &str,
        session: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        self.call(
            Request::builder()
                .method(verb)
                .uri(uri)
                .header(header::COOKIE, session)
                .header(header::ORIGIN, WEB)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn me(&self, session: &str) -> Value {
        let (status, me) = self
            .call(
                Request::get("/v1/me")
                    .header(header::COOKIE, session)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{me}");
        me
    }

    /// Journey A, step 2: the maintainer signs in, connects their wallet and sets caps; the
    /// reviewer signs in and links `{address_user}@<lnurl server>`. Returns the maintainer's
    /// session.
    async fn onboard(&self, address_user: &str) -> String {
        self.link_address(address_user).await;
        self.connect_wallet().await
    }

    /// The maintainer signs in, connects the mock wallet and sets caps. Returns their session.
    async fn connect_wallet(&self) -> String {
        let maintainer = self.sign_in("maintainer-code").await;
        let (status, me) = self
            .write(
                "PUT",
                "/v1/me/wallet",
                &maintainer,
                json!({ "nwc_uri": self.wallet.uri() }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{me}");
        assert_eq!(me["wallet"]["connected"], true);
        let (status, me) = self
            .write(
                "PATCH",
                "/v1/me/settings",
                &maintainer,
                json!({ "per_tip_max_sats": 10_000, "daily_max_sats": 50_000 }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{me}");
        maintainer
    }

    /// The reviewer signs in and links `{address_user}@<lnurl server>`. Returns their session.
    async fn link_address(&self, address_user: &str) -> String {
        let reviewer = self.sign_in("reviewer-code").await;
        let address = format!("{address_user}@{}", self.lnurl.address());
        let (status, me) = self
            .write(
                "PUT",
                "/v1/me/lightning-address",
                &reviewer,
                json!({ "address": address }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{me}");
        assert_eq!(me["lightning_address"], address);
        reviewer
    }

    /// Sends a signed delivery to the api, as GitHub would.
    async fn deliver(&self, delivery_id: &str, body: &str) -> StatusCode {
        let payload = FIXTURE.replace("{{BODY}}", body);
        let signature = format!(
            "sha256={}",
            hex::encode(mergetip_crypto::hmac_sha256(
                WEBHOOK_SECRET.as_bytes(),
                payload.as_bytes()
            ))
        );
        let request = Request::post("/v1/webhooks/github")
            .header("content-type", "application/json")
            .header("x-github-event", "issue_comment")
            .header("x-github-delivery", delivery_id)
            .header("x-hub-signature-256", signature)
            .body(Body::from(payload))
            .unwrap();
        self.api.clone().oneshot(request).await.unwrap().status()
    }

    async fn wait_until(&self, what: &str, mut done: impl AsyncFnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !done().await {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    async fn wait_for_idle_queue(&self) {
        let pool = self.pool.clone();
        self.wait_until("every job to finish", async || {
            sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM jobs WHERE dead_at IS NULL"#)
                .fetch_one(&pool)
                .await
                .unwrap()
                == 0
        })
        .await;
        let dead = sqlx::query!("SELECT kind, last_error FROM jobs WHERE dead_at IS NOT NULL")
            .fetch_all(&self.pool)
            .await
            .unwrap();
        assert!(
            dead.is_empty(),
            "dead jobs: {:?}",
            dead.iter()
                .map(|j| (&j.kind, &j.last_error))
                .collect::<Vec<_>>()
        );
    }

    async fn requests(&self, verb: &str, url_path: &str) -> Vec<Value> {
        self.github
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r: &MockRequest| r.method.as_str() == verb && r.url.path() == url_path)
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }

    async fn comments(&self) -> Vec<String> {
        let posted = self
            .requests("POST", "/repos/acme/widgets/issues/42/comments")
            .await;
        let edited = self
            .requests("PATCH", "/repos/acme/widgets/issues/comments/900")
            .await;
        posted
            .iter()
            .chain(&edited)
            .map(|c| c["body"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn attempts(&self) -> Vec<String> {
        sqlx::query_scalar!("SELECT status FROM payment_attempts ORDER BY started_at")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }
}

fn cookie(response: &Response, name: &str) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|c| c.starts_with(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{name} cookie"))
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn mock_github(server: &MockServer) {
    let user = |id: i64, login: &str| {
        json!({
            "id": id, "login": login, "type": "User",
            "avatar_url": format!("https://avatars.githubusercontent.com/u/{id}"),
            "created_at": "2019-05-01T00:00:00Z"
        })
    };
    for (id, login) in [(10, "maintainer"), (11, "reviewer")] {
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .and(body_string_contains(format!("code={login}-code")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": format!("ghu_{login}"), "expires_in": 28800,
                "refresh_token": format!("ghr_{login}"), "refresh_token_expires_in": 15897600,
                "token_type": "bearer", "scope": ""
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(has_header("authorization", format!("Bearer ghu_{login}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(user(id, login)))
            .mount(server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/user/emails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/users/reviewer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(user(11, "reviewer")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42, "state": "open", "merged": false, "merged_at": null,
            "user": user(12, "author")
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/42/reviews"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 501, "state": "APPROVED", "user": user(11, "reviewer"),
            "body": "LGTM", "submitted_at": "2026-11-02T09:00:00Z"
        }])))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 900, "html_url": "https://github.com/acme/widgets/pull/42#issuecomment-900"
        })))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/widgets/issues/comments/900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 900 })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/comments/1001/reactions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .mount(server)
        .await;
}

/// Journey A from the PRD: sign in, connect a wallet, `/tip`, verified, paid over NWC, receipt.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn maintainer_tips_a_reviewer_from_their_own_wallet(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    let maintainer = system.onboard("reviewer").await;
    let worker = system.worker(WorkerOptions::default());

    let status = system
        .deliver(
            "delivery-1",
            "Great review, thanks!\\n/tip @reviewer 2100 for the review",
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    system.wait_for_idle_queue().await;

    let tip = sqlx::query!("SELECT state, amount_sats, verified_by, payment_hash FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(tip.state, "paid");
    assert_eq!(tip.amount_sats, 2_100);
    assert_eq!(
        tip.verified_by,
        Some(json!({ "rule": "pr_review", "review_id": 501 }))
    );
    let hash = tip.payment_hash.expect("payment hash");

    // The money moved once, from the maintainer's wallet, and the proof checks out.
    assert_eq!(system.wallet.paid_hashes(), std::slice::from_ref(&hash));
    let attempt = sqlx::query!(
        "SELECT a.status, a.amount_msat, t.preimage
         FROM payment_attempts a JOIN tips t ON t.id = a.tip_id"
    )
    .fetch_one(&system.pool)
    .await?;
    assert_eq!(attempt.status, "succeeded");
    assert_eq!(attempt.amount_msat, 2_100_000);
    let preimage = hex::decode(attempt.preimage.expect("preimage")).unwrap();
    assert_eq!(hex::encode(Sha256::digest(&preimage)), hash);

    let comments = system
        .requests("POST", "/repos/acme/widgets/issues/42/comments")
        .await;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let body = comments[0]["body"].as_str().unwrap();
    assert!(body.starts_with("### ⚡ MergeTip"), "{body}");
    assert!(
        body.contains(&format!(
            "⚡ @maintainer tipped @reviewer **2,100 sats** for the review · payment `{}…`",
            &hash[..8]
        )),
        "{body}"
    );
    let reactions: Vec<String> = system
        .requests("POST", "/repos/acme/widgets/issues/comments/1001/reactions")
        .await
        .iter()
        .map(|r| r["content"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(reactions, ["eyes", "rocket"]);

    let me = system.me(&maintainer).await;
    assert_eq!(me["caps"]["spent_today_sats"], 2_100);

    worker.stop().await;
    Ok(())
}

/// Journey C: the tip arrives before the sender has a wallet, waits, and is paid once they
/// connect one in the web app.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_tip_waits_for_the_senders_wallet_then_pays(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.link_address("reviewer").await;
    let worker = system.worker(WorkerOptions::default());

    let status = system
        .deliver("delivery-1", "/tip @reviewer 2100 for the review")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    system.wait_for_idle_queue().await;
    let state = sqlx::query_scalar!("SELECT state FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(state, "awaiting_sender");
    assert!(
        system
            .comments()
            .await
            .iter()
            .any(|body| body.starts_with("🔌 @maintainer, connect your wallet")),
        "connect prompt posted"
    );
    assert!(system.wallet.paid_hashes().is_empty());

    system.connect_wallet().await;
    system.wait_for_idle_queue().await;
    let tip = sqlx::query!("SELECT state, payment_hash FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(tip.state, "paid");
    assert_eq!(
        system.wallet.paid_hashes(),
        [tip.payment_hash.expect("payment hash")]
    );

    worker.stop().await;
    Ok(())
}

/// A receiver without a Lightning address gets a pledge, then claims it with the link from the
/// thread: the sender's wallet pays it to the address they give.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_pledge_is_claimed_and_paid(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.connect_wallet().await;
    let worker = system.worker(WorkerOptions::default());

    system
        .deliver("delivery-1", "/tip @reviewer 2100 for the review")
        .await;
    system.wait_for_idle_queue().await;
    let state = sqlx::query_scalar!("SELECT state FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(state, "pledged");
    assert!(
        system.wallet.paid_hashes().is_empty(),
        "nothing charged yet"
    );
    let link = claim_link(&system.comments().await);

    let reviewer = system.sign_in("reviewer-code").await;
    let address = format!("reviewer@{}", system.lnurl.address());
    let (status, body) = system
        .write(
            "POST",
            &format!("/v1/claims/{link}"),
            &reviewer,
            json!({ "lightning_address": address }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    system.wait_for_idle_queue().await;

    let tip = sqlx::query!("SELECT state, payment_hash FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(tip.state, "paid");
    assert_eq!(
        system.wallet.paid_hashes(),
        [tip.payment_hash.expect("payment hash")]
    );
    worker.stop().await;
    Ok(())
}

/// The claim token from the pledge reply's link.
fn claim_link(comments: &[String]) -> String {
    let marker = "https://mergetip.dev/claim/";
    let body = comments
        .iter()
        .find(|b| b.contains(marker))
        .expect("a pledge reply with a claim link");
    let start = body.find(marker).unwrap() + marker.len();
    body[start..].split(')').next().unwrap().to_owned()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn redeliveries_pay_once(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.onboard("reviewer").await;
    let worker = system.worker(WorkerOptions::default());

    // GitHub redelivers the same delivery, and a second delivery of the same comment arrives.
    for delivery in ["delivery-1", "delivery-1", "delivery-1", "delivery-2"] {
        let status = system.deliver(delivery, "/tip @reviewer 100").await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    system.wait_for_idle_queue().await;

    let states: Vec<String> = sqlx::query_scalar!("SELECT state FROM tips")
        .fetch_all(&system.pool)
        .await?;
    assert_eq!(states, ["paid"]);
    let events = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM github_events"#)
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(events, 2, "two distinct deliveries stored, one tip");
    assert_eq!(system.wallet.paid_hashes().len(), 1, "paid exactly once");
    assert_eq!(system.attempts().await, ["succeeded"]);
    let comments = system
        .requests("POST", "/repos/acme/widgets/issues/42/comments")
        .await;
    assert_eq!(comments.len(), 1);

    worker.stop().await;
    Ok(())
}

/// A Lightning address that returns an invoice for the wrong amount never reaches the wallet.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_wrong_amount_invoice_is_never_paid(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.onboard("wrongamount").await;
    let worker = system.worker(WorkerOptions::default());

    let status = system
        .deliver("delivery-1", "/tip @reviewer 2100 for the review")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    system.wait_for_idle_queue().await;

    let tip = sqlx::query!("SELECT state, error_code, payment_hash FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(tip.state, "failed");
    assert_eq!(tip.error_code.as_deref(), Some("receiver_unpayable"));
    assert_eq!(tip.payment_hash, None);
    assert!(system.wallet.paid_hashes().is_empty(), "nothing was paid");
    assert!(system.attempts().await.is_empty(), "nothing was reserved");
    assert!(
        !system
            .comments()
            .await
            .iter()
            .any(|body| body.contains("⚡ @maintainer tipped")),
        "no receipt"
    );

    worker.stop().await;
    Ok(())
}

/// The worker dies after `pay_invoice` went out: the wallet paid, but nothing was recorded. Once
/// its lease expires another worker picks the job up, reconciles with `lookup_invoice` instead of
/// paying again, and the tip ends up paid exactly once.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_worker_killed_mid_payment_never_pays_twice(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.onboard("reviewer").await;
    // This worker waits far longer for the wallet than the test lets it live.
    let doomed = system.worker(WorkerOptions {
        pay_timeout: Duration::from_secs(120),
        lease: Duration::from_secs(1),
        drain_timeout: Duration::from_millis(10),
    });

    let command = format!("/tip @reviewer {}", behaviour::PAYS_SILENTLY_SATS);
    assert_eq!(
        system.deliver("delivery-1", &command).await,
        StatusCode::ACCEPTED
    );
    let wallet = &system.wallet;
    system
        .wait_until("the wallet to pay", async || {
            !wallet.paid_hashes().is_empty()
        })
        .await;
    // Killed: the drain timeout aborts the handler before it records anything.
    doomed.stop().await;
    assert_eq!(system.attempts().await, ["pending"]);
    let state = sqlx::query_scalar!("SELECT state FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(state, "paying");

    let worker = system.worker(WorkerOptions {
        lease: Duration::from_secs(1),
        ..WorkerOptions::default()
    });
    system.wait_for_idle_queue().await;

    let tip = sqlx::query!("SELECT state, payment_hash FROM tips")
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(tip.state, "paid");
    assert_eq!(
        system.wallet.paid_hashes(),
        [tip.payment_hash.expect("payment hash")],
        "paid exactly once"
    );
    assert_eq!(system.attempts().await, ["succeeded"]);
    assert!(
        system
            .comments()
            .await
            .iter()
            .any(|body| body.contains("⚡ @maintainer tipped @reviewer **777 sats**")),
        "receipted"
    );

    worker.stop().await;
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn forged_delivery_changes_nothing(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    let worker = system.worker(WorkerOptions::default());
    let request = Request::post("/v1/webhooks/github")
        .header("x-github-event", "issue_comment")
        .header("x-github-delivery", "forged")
        .header("x-hub-signature-256", "sha256=deadbeef")
        .body(Body::from(
            FIXTURE.replace("{{BODY}}", "/tip @reviewer 100"),
        ))
        .unwrap();
    let status = system.api.clone().oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let tips = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM tips"#)
        .fetch_one(&system.pool)
        .await?;
    assert_eq!(tips, 0);
    assert!(
        system
            .github
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    worker.stop().await;
    Ok(())
}
