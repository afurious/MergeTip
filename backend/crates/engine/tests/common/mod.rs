//! A harness for the M4 edge-flow tests: Postgres, an in-memory GitHub, a movable clock, and a
//! job runner that dispatches by kind like the worker. Payments use the real engine path with a
//! wallet row that can't pay, so tests stop at `paying` unless they say otherwise.

#![allow(dead_code)]

use std::{sync::Arc, time::Duration};

use mergetip_core::{FixedClock, SystemClock};
use mergetip_crypto::Keyring;
use mergetip_engine::{
    Engine, EngineSettings, Payments,
    jobs::{JobPayload, PayTip, PostReply, ProcessEvent, ResolveHeld, ResumeSenderTips, VerifyTip},
};
use mergetip_github::{PullRequest, Review, testing::FakeGitHub};
use mergetip_payer::{LnurlClient, Network, nwc::NwcConnector};
use mergetip_store::{github_events, jobs};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;

pub const REPO: &str = "acme/widgets";
pub const KEY: &str = "0707070707070707070707070707070707070707070707070707070707070707";

pub struct Harness {
    pub pool: PgPool,
    pub github: Arc<FakeGitHub>,
    pub clock: Arc<FixedClock>,
    pub engine: Engine,
    deliveries: u32,
}

impl Harness {
    pub fn new(pool: PgPool) -> Self {
        let github = Arc::new(FakeGitHub::new());
        github.add_user(FakeGitHub::account(10, "maintainer"));
        github.add_user(FakeGitHub::account(11, "reviewer"));
        github.add_user(FakeGitHub::account(12, "author"));
        {
            let mut state = github.state();
            state.pulls.insert(
                (REPO.into(), 42),
                PullRequest {
                    number: 42,
                    user: Some(FakeGitHub::account(12, "author")),
                    state: "open".into(),
                    merged: Some(false),
                    merged_at: None,
                },
            );
            state.reviews.insert(
                (REPO.into(), 42),
                vec![Review {
                    id: 501,
                    user: Some(FakeGitHub::account(11, "reviewer")),
                    state: "APPROVED".into(),
                }],
            );
        }
        let clock = Arc::new(FixedClock::new(OffsetDateTime::now_utc()));
        let engine = Engine::new(
            pool.clone(),
            github.clone(),
            clock.clone(),
            EngineSettings::new("https://mergetip.dev", false),
            Payments {
                receivers: Arc::new(
                    LnurlClient::new(Network::Regtest, true, Arc::new(SystemClock)).unwrap(),
                ),
                wallets: Arc::new(NwcConnector),
                keyring: Arc::new(Keyring::from_hex(KEY, None).unwrap()),
            },
        );
        Self {
            pool,
            github,
            clock,
            engine,
            deliveries: 0,
        }
    }

    /// Gives `github_id` a wallet connection (its URI can't be decrypted, so it never pays).
    pub async fn wallet_for(&self, github_id: i64, login: &str) {
        sqlx::query!(
            r#"INSERT INTO users (github_id, github_login) VALUES ($1, $2)
               ON CONFLICT (github_id) DO NOTHING"#,
            github_id,
            login,
        )
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query!(
            r#"INSERT INTO wallet_connections
                   (user_id, nwc_uri_ciphertext, wallet_pubkey, relays, methods, encryption)
               SELECT id, '\x00', 'pk', '{wss://relay}', '{pay_invoice}', 'nip44'
               FROM users WHERE github_id = $1"#,
            github_id,
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Connects a wallet the way `PUT /me/wallet` does: the connection plus the resume job.
    pub async fn connect_wallet(&self, github_id: i64, login: &str) {
        self.wallet_for(github_id, login).await;
        let user_id = sqlx::query_scalar!("SELECT id FROM users WHERE github_id = $1", github_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        let mut tx = self.pool.begin().await.unwrap();
        mergetip_engine::jobs::enqueue(
            &mut tx,
            &ResumeSenderTips {
                user_id: mergetip_core::UserId::from_uuid(user_id),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    /// Stores and queues a delivery like the webhook endpoint does.
    pub async fn deliver(&mut self, event: &str, action: &str, payload: Value) {
        self.deliveries += 1;
        let delivery_id = format!("delivery-{}", self.deliveries);
        let mut tx = self.pool.begin().await.unwrap();
        github_events::insert_if_new(&mut *tx, &delivery_id, event, Some(action), &payload)
            .await
            .unwrap();
        mergetip_engine::jobs::enqueue(&mut tx, &ProcessEvent { delivery_id })
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// A `/tip` comment by the maintainer on PR #42.
    pub async fn comment(&mut self, comment_id: i64, body: &str) {
        let payload = json!({
            "action": "created",
            "comment": {
                "id": comment_id, "body": body,
                "html_url": format!("https://github.com/{REPO}/pull/42#issuecomment-{comment_id}"),
                "user": { "id": 10, "login": "maintainer", "type": "User" }
            },
            "issue": {
                "number": 42,
                "user": { "id": 12, "login": "author", "type": "User" },
                "pull_request": { "url": "x" }
            },
            "repository": {
                "id": 5, "name": "widgets", "full_name": REPO,
                "owner": { "id": 3, "login": "acme", "type": "Organization" }
            },
            "installation": { "id": 77 }
        });
        self.deliver("issue_comment", "created", payload).await;
    }

    /// `pull_request.closed` for PR #42. `claims_merged` is what the payload says; GitHub's own
    /// answer is whatever the fake's PR currently holds.
    pub async fn pull_closed(&mut self, claims_merged: bool) {
        let payload = json!({
            "action": "closed",
            "number": 42,
            "pull_request": { "number": 42, "merged": claims_merged },
            "repository": {
                "id": 5, "name": "widgets", "full_name": REPO,
                "owner": { "id": 3, "login": "acme", "type": "Organization" }
            },
            "installation": { "id": 77 }
        });
        self.deliver("pull_request", "closed", payload).await;
    }

    /// Sets PR #42's state on the fake GitHub.
    pub fn set_pull(&self, state: &str, merged: bool) {
        let mut github = self.github.state();
        let pull = github.pulls.get_mut(&(REPO.into(), 42)).unwrap();
        pull.state = state.into();
        pull.merged = Some(merged);
        pull.merged_at = merged.then(OffsetDateTime::now_utc);
    }

    /// Runs every ready job except `pay_tip` (left queued, so tests can count it).
    pub async fn drain(&self) {
        self.run_only(&[
            ProcessEvent::KIND,
            VerifyTip::KIND,
            PostReply::KIND,
            ResolveHeld::KIND,
            ResumeSenderTips::KIND,
        ])
        .await;
    }

    pub async fn run_only(&self, kinds: &[&str]) {
        let kinds: Vec<String> = kinds.iter().map(|k| (*k).to_owned()).collect();
        while let Some(job) = jobs::claim(&self.pool, &kinds, "test/1", Duration::from_secs(60))
            .await
            .unwrap()
        {
            let result = match job.kind.as_str() {
                ProcessEvent::KIND => {
                    let p = ProcessEvent::decode(&job.payload).unwrap();
                    self.engine.process_event(&p.delivery_id).await
                }
                VerifyTip::KIND => {
                    self.engine
                        .verify_tip(VerifyTip::decode(&job.payload).unwrap().tip_id)
                        .await
                }
                PayTip::KIND => {
                    self.engine
                        .pay_tip(PayTip::decode(&job.payload).unwrap().tip_id, false)
                        .await
                }
                PostReply::KIND => {
                    self.engine
                        .post_reply(&PostReply::decode(&job.payload).unwrap())
                        .await
                }
                ResolveHeld::KIND => {
                    self.engine
                        .resolve_held(&ResolveHeld::decode(&job.payload).unwrap())
                        .await
                }
                ResumeSenderTips::KIND => {
                    self.engine
                        .resume_sender_tips(ResumeSenderTips::decode(&job.payload).unwrap().user_id)
                        .await
                }
                other => panic!("unexpected job kind {other}"),
            };
            result.unwrap_or_else(|err| panic!("{} failed: {err}", job.kind));
            jobs::complete(&self.pool, &job).await.unwrap();
        }
    }

    /// `(receiver, state, error_code)` per tip, oldest first.
    pub async fn tips(&self) -> Vec<(String, String, Option<String>)> {
        sqlx::query!(
            "SELECT receiver_login, state, error_code FROM tips ORDER BY created_at, command_index"
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.receiver_login, r.state, r.error_code))
        .collect()
    }

    pub async fn states(&self) -> Vec<String> {
        self.tips().await.into_iter().map(|t| t.1).collect()
    }

    pub async fn queued(&self, kind: &str) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM jobs WHERE kind = $1 AND dead_at IS NULL"#,
            kind
        )
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// The thread's collapsed status comment as last written.
    pub fn status_comment(&self) -> String {
        self.github
            .posted_bodies()
            .into_iter()
            .find(|b| b.starts_with("### ⚡ MergeTip"))
            .unwrap_or_default()
    }
}
