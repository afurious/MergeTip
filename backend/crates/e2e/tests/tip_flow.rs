//! A `/tip` comment, end to end: signed webhook → api → Postgres queue → worker → GitHub replies.
//!
//! Everything is real except GitHub, which is a `wiremock` server driven through the production
//! octocrab client.

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use mergetip_api::{ApiConfig, AppState, Services, router};
use mergetip_core::SystemClock;
use mergetip_engine::{Engine, EngineSettings};
use mergetip_github::{OctocrabGitHub, oauth::OAuthClient};
use mergetip_worker::{
    Runner, RunnerConfig,
    handlers::{PayTipStubHandler, PostReplyHandler, ProcessEventHandler, VerifyTipHandler},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, Request as MockRequest, ResponseTemplate,
    matchers::{method, path, query_param},
};

const WEBHOOK_SECRET: &str = "e2e-secret";
const FIXTURE: &str = include_str!("fixtures/issue_comment_created.json");

struct System {
    pool: PgPool,
    api: Router,
    github: MockServer,
    shutdown: CancellationToken,
    worker: tokio::task::JoinHandle<()>,
}

impl System {
    async fn start(pool: PgPool) -> Self {
        let github = MockServer::start().await;
        mock_github(&github).await;

        let env = HashMap::from([
            (
                "MASTER_KEY",
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            ),
            ("DATABASE_URL", "postgres://unused/db"),
            ("PUBLIC_BASE_URL", "https://mergetip.dev"),
            ("GITHUB_WEBHOOK_SECRET", WEBHOOK_SECRET),
            ("API_PUBLIC_URL", "https://api.mergetip.dev"),
            ("GITHUB_CLIENT_ID", "Iv1.e2e"),
            ("GITHUB_CLIENT_SECRET", "e2e-client-secret"),
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
        };
        let api = router(AppState::new(
            pool.clone(),
            config,
            Arc::new(SystemClock),
            services,
        ));

        let client = OctocrabGitHub::with_token("installation-token", &github.uri()).unwrap();
        let engine = Arc::new(Engine::new(
            pool.clone(),
            Arc::new(client),
            Arc::new(SystemClock),
            EngineSettings {
                web_base_url: "https://mergetip.dev".into(),
                stub_payer: true,
            },
        ));
        let runner = Runner::new(
            pool.clone(),
            Arc::new(SystemClock),
            RunnerConfig {
                concurrency: 4,
                poll_interval: Duration::from_millis(50),
                retry_base: Duration::from_millis(50),
                retry_cap: Duration::from_millis(200),
                ..RunnerConfig::default()
            },
        )
        .unwrap()
        .register(ProcessEventHandler(engine.clone()))
        .register(VerifyTipHandler(engine.clone()))
        .register(PostReplyHandler(engine.clone()))
        .register(PayTipStubHandler(engine));
        let shutdown = CancellationToken::new();
        let worker = tokio::spawn(runner.run(shutdown.clone()));

        Self {
            pool,
            api,
            github,
            shutdown,
            worker,
        }
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

    async fn wait_for_idle_queue(&self) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let pending = sqlx::query_scalar!(
                    r#"SELECT count(*) AS "n!" FROM jobs WHERE dead_at IS NULL"#
                )
                .fetch_one(&self.pool)
                .await
                .unwrap();
                if pending == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("every job finishes");
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

    async fn stop(self) {
        self.shutdown.cancel();
        self.worker.await.unwrap();
    }
}

async fn mock_github(server: &MockServer) {
    let user = |id: i64, login: &str| {
        json!({
            "id": id, "login": login, "type": "User",
            "avatar_url": format!("https://avatars.githubusercontent.com/u/{id}"),
            "created_at": "2019-05-01T00:00:00Z"
        })
    };
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

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn reviewer_is_tipped_and_receipted(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;

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

    system.stop().await;
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn redeliveries_pay_once(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;

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
    let comments = system
        .requests("POST", "/repos/acme/widgets/issues/42/comments")
        .await;
    assert_eq!(comments.len(), 1);

    system.stop().await;
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn forged_delivery_changes_nothing(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
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
    system.stop().await;
    Ok(())
}
