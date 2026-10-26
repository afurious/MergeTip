//! The end-to-end system: the real api router and worker runner on Postgres, with the outside
//! world faked. GitHub is a `wiremock` server driven through the production octocrab and OAuth
//! clients, the receiver's Lightning address is a `wiremock` LNURL server minting real signed
//! invoices, and the sender's wallet is the NIP-47 mock wallet on an in-process relay, reached
//! through the production NWC client.

#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use mergetip_api::{ApiConfig, AppState, Services, router};
use mergetip_core::{FixedClock, Msats, SystemClock};
use mergetip_crypto::Keyring;
use mergetip_engine::{Engine, EngineSettings, Payments};
use mergetip_github::{OctocrabGitHub, oauth::OAuthClient};
use mergetip_notify::testing::RecordingMailer;
use mergetip_payer::{
    LnurlClient, Network,
    nwc::NwcConnector,
    testing::{MintParams, MockWallet, mint_invoice},
};
use mergetip_worker::{
    Runner, RunnerConfig,
    handlers::{
        NotifyPledgeHandler, PayTipHandler, PostReplyHandler, ProcessEventHandler,
        ReconcilePaymentHandler, ResolveHeldHandler, ResumeSenderTipsHandler, SendDigestHandler,
        VerifyTipHandler,
    },
};
use nostr_sdk::prelude::MockRelay;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate,
    matchers::{body_string_contains, header as has_header, method, path, path_regex, query_param},
};

pub const WEB: &str = "https://mergetip.dev";
pub const WEBHOOK_SECRET: &str = "e2e-secret";
pub const MASTER_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
pub const SEED: [u8; 32] = [9; 32];
pub const METADATA: &str = r#"[["text/plain","Sats for reviewer"]]"#;
pub const FIXTURE: &str = include_str!("../fixtures/issue_comment_created.json");

/// LNURL-pay for any user: the callback mints a real invoice for the requested amount, payable
/// by the mock wallet because both share [`SEED`]. `wrongamount` asks for 1 sat more.
pub struct Lnurl {
    pub base: String,
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
pub struct WorkerOptions {
    pub pay_timeout: Duration,
    pub lease: Duration,
    pub drain_timeout: Duration,
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

pub struct Worker {
    pub shutdown: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
    /// The worker's engine, for running a use case directly (the recurring expiry job never
    /// leaves the queue, so tests call it instead of scheduling it).
    pub engine: Arc<Engine>,
}

impl Worker {
    pub async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.unwrap();
    }
}

pub struct System {
    pub pool: PgPool,
    pub api: Router,
    pub github: MockServer,
    pub lnurl: MockServer,
    pub wallet: MockWallet,
    pub relay: MockRelay,
    /// The engine's and api's clock, so tests can move time; the job queue keeps database time.
    pub clock: Arc<FixedClock>,
    /// What the api emailed (confirmation codes).
    pub api_mail: Arc<RecordingMailer>,
    /// What the worker emailed (pledge notices).
    pub worker_mail: Arc<RecordingMailer>,
}

impl System {
    pub async fn start(pool: PgPool) -> Self {
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
        let clock = Arc::new(FixedClock::new(time::OffsetDateTime::now_utc()));
        let api_mail = Arc::new(RecordingMailer::new());
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
            mailer: api_mail.clone(),
        };
        let api = router(AppState::new(pool.clone(), config, clock.clone(), services));

        Self {
            pool,
            api,
            github,
            lnurl,
            wallet,
            relay,
            clock,
            api_mail,
            worker_mail: Arc::new(RecordingMailer::new()),
        }
    }

    /// A worker process: the production runner and handlers around a real-payments engine.
    pub fn worker(&self, options: WorkerOptions) -> Worker {
        let client = OctocrabGitHub::with_token("installation-token", &self.github.uri()).unwrap();
        let mut settings = EngineSettings::new(WEB, false);
        settings.pay_timeout = options.pay_timeout;
        settings.api_base_url = "https://api.mergetip.dev".into();
        let engine = Arc::new(
            Engine::new(
                self.pool.clone(),
                Arc::new(client),
                self.clock.clone(),
                settings,
                Payments {
                    receivers: Arc::new(
                        LnurlClient::new(Network::Regtest, true, Arc::new(SystemClock)).unwrap(),
                    ),
                    wallets: Arc::new(NwcConnector),
                    keyring: Arc::new(Keyring::from_hex(MASTER_KEY, None).unwrap()),
                },
            )
            .with_mailer(self.worker_mail.clone()),
        );
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
        .register(NotifyPledgeHandler(engine.clone()))
        .register(SendDigestHandler(engine.clone()))
        .register(ReconcilePaymentHandler(engine.clone()));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(runner.run(shutdown.clone()));
        Worker {
            shutdown,
            task,
            engine,
        }
    }

    pub async fn call(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response: Response = self.api.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Signs in through GitHub OAuth, as the browser would, and returns the session cookie.
    pub async fn sign_in(&self, code: &str) -> String {
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
    pub async fn write(
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

    pub async fn me(&self, session: &str) -> Value {
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
    pub async fn onboard(&self, address_user: &str) -> String {
        self.link_address(address_user).await;
        self.connect_wallet().await
    }

    /// The maintainer signs in, connects the mock wallet and sets caps. Returns their session.
    pub async fn connect_wallet(&self) -> String {
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
    pub async fn link_address(&self, address_user: &str) -> String {
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
    pub async fn deliver(&self, delivery_id: &str, body: &str) -> StatusCode {
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

    /// Sends any signed event, as GitHub would.
    pub async fn deliver_event(
        &self,
        event: &str,
        delivery_id: &str,
        payload: &Value,
    ) -> StatusCode {
        let payload = payload.to_string();
        let signature = format!(
            "sha256={}",
            hex::encode(mergetip_crypto::hmac_sha256(
                WEBHOOK_SECRET.as_bytes(),
                payload.as_bytes()
            ))
        );
        let request = Request::post("/v1/webhooks/github")
            .header("content-type", "application/json")
            .header("x-github-event", event)
            .header("x-github-delivery", delivery_id)
            .header("x-hub-signature-256", signature)
            .body(Body::from(payload))
            .unwrap();
        self.api.clone().oneshot(request).await.unwrap().status()
    }

    /// PR #42 merges: GitHub now reports it merged, and sends `pull_request.closed`.
    pub async fn merge_pr(&self, delivery_id: &str) -> StatusCode {
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 42, "state": "closed", "merged": true,
                "merged_at": "2026-11-03T10:00:00Z",
                "user": { "id": 12, "login": "author", "type": "User",
                          "created_at": "2019-05-01T00:00:00Z" }
            })))
            .with_priority(1)
            .mount(&self.github)
            .await;
        let payload = json!({
            "action": "closed",
            "number": 42,
            "pull_request": { "number": 42, "merged": true },
            "repository": {
                "id": 5, "name": "widgets", "full_name": "acme/widgets",
                "owner": { "id": 3, "login": "acme", "type": "Organization" }
            },
            "installation": { "id": 77 }
        });
        self.deliver_event("pull_request", delivery_id, &payload)
            .await
    }

    /// The single tip's state.
    pub async fn tip_state(&self) -> String {
        sqlx::query_scalar!("SELECT state FROM tips")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub async fn wait_until(&self, what: &str, mut done: impl AsyncFnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !done().await {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    pub async fn wait_for_idle_queue(&self) {
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

    pub async fn requests(&self, verb: &str, url_path: &str) -> Vec<Value> {
        self.github
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r: &MockRequest| r.method.as_str() == verb && r.url.path() == url_path)
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }

    pub async fn comments(&self) -> Vec<String> {
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

    pub async fn attempts(&self) -> Vec<String> {
        sqlx::query_scalar!("SELECT status FROM payment_attempts ORDER BY started_at")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }
}

pub fn cookie(response: &Response, name: &str) -> String {
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

pub async fn mock_github(server: &MockServer) {
    let user = |id: i64, login: &str| {
        json!({
            "id": id, "login": login, "type": "User",
            "avatar_url": format!("https://avatars.githubusercontent.com/u/{id}"),
            "created_at": "2019-05-01T00:00:00Z"
        })
    };
    for (id, login) in [(10, "maintainer"), (11, "reviewer"), (12, "author")] {
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
    // Only the maintainer has a verified email (confirmation codes go there).
    Mock::given(method("GET"))
        .and(path("/user/emails"))
        .and(has_header("authorization", "Bearer ghu_maintainer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "email": "maintainer@example.com", "primary": true, "verified": true }
        ])))
        .mount(server)
        .await;
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
        .and(path("/users/author"))
        .respond_with(ResponseTemplate::new(200).set_body_json(user(12, "author")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/42/commits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
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

/// The claim token from the pledge reply's link.
pub fn claim_link(comments: &[String]) -> String {
    let marker = "https://mergetip.dev/claim/";
    let body = comments
        .iter()
        .find(|b| b.contains(marker))
        .expect("a pledge reply with a claim link");
    let start = body.find(marker).unwrap() + marker.len();
    body[start..].split(')').next().unwrap().to_owned()
}
