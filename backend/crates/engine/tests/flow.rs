//! Engine flows from a stored webhook to replies, against Postgres and an in-memory GitHub.

use std::{sync::Arc, time::Duration};

use mergetip_core::SystemClock;
use mergetip_engine::{
    Engine, EngineSettings,
    jobs::{JobPayload, PayTip, PostReply, ProcessEvent, VerifyTip},
};
use mergetip_github::{Issue, PullRequest, Reaction, Review, testing::FakeGitHub};
use mergetip_store::{github_events, jobs};
use serde_json::{Value, json};
use sqlx::PgPool;

const REPO: &str = "acme/widgets";

struct Harness {
    pool: PgPool,
    github: Arc<FakeGitHub>,
    engine: Engine,
    deliveries: u32,
}

impl Harness {
    fn new(pool: PgPool, stub_payer: bool) -> Self {
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
            state.issues.insert(
                (REPO.into(), 7),
                Issue {
                    number: 7,
                    user: Some(FakeGitHub::account(12, "author")),
                    pull_request: None,
                },
            );
        }
        let engine = Engine::new(
            pool.clone(),
            github.clone(),
            Arc::new(SystemClock),
            EngineSettings {
                web_base_url: "https://mergetip.dev".into(),
                stub_payer,
            },
        );
        Self {
            pool,
            github,
            engine,
            deliveries: 0,
        }
    }

    /// Stores a delivery like the webhook endpoint does, then processes everything it causes.
    async fn comment(&mut self, comment_id: i64, author: (i64, &str), body: &str, number: u32) {
        self.comment_event("created", comment_id, author, body, number)
            .await;
    }

    async fn comment_event(
        &mut self,
        action: &str,
        comment_id: i64,
        (author_id, author_login): (i64, &str),
        body: &str,
        number: u32,
    ) {
        let is_pr = number == 42;
        let author_type = if author_login.ends_with("[bot]") {
            "Bot"
        } else {
            "User"
        };
        let payload = json!({
            "action": action,
            "comment": {
                "id": comment_id, "body": body,
                "html_url": format!("https://github.com/{REPO}/pull/{number}#issuecomment-{comment_id}"),
                "user": { "id": author_id, "login": author_login, "type": author_type }
            },
            "issue": {
                "number": number,
                "user": { "id": 12, "login": "author", "type": "User" },
                "pull_request": if is_pr { json!({ "url": "x" }) } else { Value::Null }
            },
            "repository": {
                "id": 5, "name": "widgets", "full_name": REPO,
                "owner": { "id": 3, "login": "acme", "type": "Organization" }
            },
            "installation": { "id": 77 }
        });
        self.deliveries += 1;
        let delivery_id = format!("delivery-{}", self.deliveries);
        let mut tx = self.pool.begin().await.unwrap();
        github_events::insert_if_new(
            &mut *tx,
            &delivery_id,
            "issue_comment",
            Some(action),
            &payload,
        )
        .await
        .unwrap();
        mergetip_engine::jobs::enqueue(&mut tx, &ProcessEvent { delivery_id })
            .await
            .unwrap();
        tx.commit().await.unwrap();
        self.drain().await;
    }

    /// Runs queued jobs until none are left, dispatching by kind like the worker does. As in the
    /// worker, `pay_tip` only has a handler when the stub payer is on; otherwise paying tips wait.
    async fn drain(&self) {
        let mut kinds: Vec<String> = [ProcessEvent::KIND, VerifyTip::KIND, PostReply::KIND]
            .map(String::from)
            .to_vec();
        if self.engine.settings().stub_payer {
            kinds.push(PayTip::KIND.into());
        }
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
                        .pay_tip_stub(PayTip::decode(&job.payload).unwrap().tip_id)
                        .await
                }
                PostReply::KIND => {
                    self.engine
                        .post_reply(&PostReply::decode(&job.payload).unwrap())
                        .await
                }
                other => panic!("unexpected job kind {other}"),
            };
            result.unwrap_or_else(|err| panic!("{} failed: {err}", job.kind));
            jobs::complete(&self.pool, &job).await.unwrap();
        }
    }

    async fn tips(&self) -> Vec<(String, String, Option<String>)> {
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

    fn reactions(&self) -> Vec<(i64, Reaction)> {
        self.github.state().reactions.clone()
    }
}

fn states(tips: &[(String, String, Option<String>)]) -> Vec<&str> {
    tips.iter().map(|t| t.1.as_str()).collect()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn stubbed_tip_is_verified_paid_and_receipted(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    h.comment(
        1001,
        (10, "maintainer"),
        "Thanks!\n/tip @reviewer 2100 for the review",
        42,
    )
    .await;

    let tips = h.tips().await;
    assert_eq!(states(&tips), ["paid"]);
    let tip = sqlx::query!("SELECT payment_hash, preimage, verified_by, receiver_id FROM tips")
        .fetch_one(&h.pool)
        .await?;
    assert_eq!(
        tip.verified_by,
        Some(json!({ "rule": "pr_review", "review_id": 501 }))
    );
    assert!(tip.receiver_id.is_some());
    assert_eq!(tip.payment_hash.as_ref().map(String::len), Some(64));

    let posted = h.github.posted_bodies();
    assert_eq!(posted.len(), 1, "one collapsed status comment: {posted:?}");
    assert!(posted[0].starts_with("### ⚡ MergeTip"));
    assert!(posted[0].contains("⚡ @maintainer tipped @reviewer **2,100 sats** for the review"));
    assert_eq!(
        h.reactions(),
        [(1001, Reaction::Eyes), (1001, Reaction::Rocket)]
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn redelivered_comment_creates_one_tip(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    for _ in 0..3 {
        h.comment(1001, (10, "maintainer"), "/tip @reviewer 100", 42)
            .await;
    }
    assert_eq!(states(&h.tips().await), ["paid"]);
    assert_eq!(h.github.posted_bodies().len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn edits_and_bot_comments_create_nothing(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    h.comment_event("edited", 1001, (10, "maintainer"), "/tip @reviewer 100", 42)
        .await;
    h.comment(1002, (99, "mergetip[bot]"), "/tip @reviewer 100", 42)
        .await;
    assert!(h.tips().await.is_empty());
    assert!(h.github.posted_bodies().is_empty());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn malformed_and_excess_commands_get_one_reply(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    let body = "/tip @reviewer 1\n/tip @reviewer 2\n/tip @reviewer 3\n/tip @author 4\n/tip @author nope\n/tip @author 6";
    h.comment(1001, (10, "maintainer"), body, 42).await;

    let tips = h.tips().await;
    assert_eq!(tips.len(), 4, "{tips:?}");
    let posted = h.github.posted_bodies();
    let errors: Vec<_> = posted
        .iter()
        .filter(|b| b.contains("couldn't read"))
        .collect();
    assert_eq!(errors.len(), 1, "{posted:?}");
    assert!(errors[0].contains("`/tip @author nope`"));
    assert!(errors[0].contains("Only 5 tips are allowed per comment"));
    assert!(errors[0].contains("Usage: `/tip @username"));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn sender_without_wallet_waits_for_one(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, false);
    h.comment(1001, (10, "maintainer"), "/tip @reviewer 500", 42)
        .await;

    assert_eq!(states(&h.tips().await), ["awaiting_sender"]);
    let expires = sqlx::query_scalar!(
        r#"SELECT extract(epoch FROM expires_at - now())::float8 AS "secs!" FROM tips"#
    )
    .fetch_one(&h.pool)
    .await?;
    assert!(
        (86_000.0..=86_400.0).contains(&expires),
        "expires in {expires}s"
    );
    let posted = h.github.posted_bodies();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with("🔌 @maintainer, connect your wallet"));
    assert!(posted[0].contains("https://mergetip.dev/settings/wallet"));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn sender_with_wallet_is_verified_without_the_stub(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, false);
    h.comment(1000, (10, "maintainer"), "hello", 42).await; // creates nothing
    sqlx::query!(
        r#"INSERT INTO users (github_id, github_login) VALUES (10, 'maintainer')
           ON CONFLICT (github_id) DO NOTHING"#
    )
    .execute(&h.pool)
    .await?;
    sqlx::query!(
        r#"INSERT INTO wallet_connections (user_id, nwc_uri_ciphertext, wallet_pubkey, relays, methods, encryption)
           SELECT id, '\x00', 'pk', '{wss://relay}', '{pay_invoice}', 'nip44' FROM users WHERE github_id = 10"#
    )
    .execute(&h.pool)
    .await?;

    h.comment(1001, (10, "maintainer"), "/tip @reviewer 500", 42)
        .await;
    // Verified, waiting for a payer: no real payer exists before M3.
    assert_eq!(states(&h.tips().await), ["paying"]);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn unverifiable_tips_are_rejected_with_a_reply(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    h.github.add_user(FakeGitHub::account(13, "bystander"));
    h.comment(
        1001,
        (10, "maintainer"),
        "/tip @bystander 100\n/tip @maintainer 100\n/tip @ghost 1",
        42,
    )
    .await;

    let tips = h.tips().await;
    let codes: Vec<_> = tips.iter().map(|t| t.2.as_deref().unwrap_or("")).collect();
    assert_eq!(states(&tips), ["rejected", "rejected", "rejected"]);
    assert_eq!(codes, ["no_contribution", "self_tip", "unknown_user"]);
    let posted = h.github.posted_bodies().join("\n");
    assert!(posted.contains("@bystander hasn't contributed to this thread yet"));
    assert!(posted.contains("you can't tip yourself"));
    assert!(posted.contains("there is no GitHub user `ghost`"));
    assert_eq!(
        h.reactions(),
        [(1001, Reaction::Eyes), (1001, Reaction::Confused)]
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn limits_reject_without_asking_github(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    h.comment(1001, (10, "maintainer"), "/tip @author 5 on-merge", 7)
        .await;
    for i in 0..4 {
        h.comment(2000 + i, (10, "maintainer"), "/tip @reviewer 1", 42)
            .await;
    }

    let tips = h.tips().await;
    assert_eq!(tips[0].2.as_deref(), Some("on_merge_not_pr"));
    assert_eq!(states(&tips[1..]), ["paid", "paid", "paid", "rejected"]);
    assert_eq!(tips[4].2.as_deref(), Some("pair_limit"));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn the_twenty_first_tip_in_an_hour_is_rejected(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, false); // no wallet: tips stay awaiting_sender, still counted
    for i in 0..7 {
        let body: String = (0..3).map(|j| format!("/tip @user{i}x{j} 1\n")).collect();
        h.comment(3000 + i, (10, "maintainer"), &body, 42).await;
    }
    let tips = h.tips().await;
    assert_eq!(tips.len(), 21);
    assert!(tips[..20].iter().all(|t| t.1 == "awaiting_sender"));
    assert_eq!(tips[20].2.as_deref(), Some("rate_limited_sender"));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn high_value_and_on_merge_tips_wait(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    sqlx::query!("INSERT INTO users (github_id, github_login, per_tip_max_sats) VALUES (10, 'maintainer', 1000000)")
        .execute(&h.pool)
        .await?;
    h.comment(
        1001,
        (10, "maintainer"),
        "/tip @reviewer 80k\n/tip @author 5k on-merge",
        42,
    )
    .await;

    assert_eq!(states(&h.tips().await), ["awaiting_confirmation", "held"]);
    let posted = h.github.posted_bodies();
    let confirm = posted
        .iter()
        .find(|b| b.starts_with("🔐"))
        .expect("confirmation request");
    let tip_id = sqlx::query_scalar!("SELECT id FROM tips WHERE state = 'awaiting_confirmation'")
        .fetch_one(&h.pool)
        .await?;
    assert!(confirm.contains(&format!("https://mergetip.dev/confirm/{tip_id}")));
    assert!(confirm.contains("**80,000 sats**"));
    let status = posted
        .iter()
        .find(|b| b.starts_with("### ⚡ MergeTip"))
        .unwrap();
    assert!(
        status.contains("⏳ @maintainer will tip @author **5,000 sats** when this PR is merged")
    );
    Ok(())
}

/// Two status updates for the same thread at once must both end up in the comment.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn concurrent_status_updates_both_survive(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool, true);
    h.comment(1001, (10, "maintainer"), "/tip @reviewer 10", 42)
        .await;
    let tip_id = sqlx::query_scalar!("SELECT id FROM tips")
        .fetch_one(&h.pool)
        .await?;
    let reply = |n| PostReply {
        installation: mergetip_core::GithubInstallationId::new(77),
        repo: REPO.into(),
        thread_number: 42,
        reply: mergetip_engine::jobs::Reply::Tip {
            tip_id: mergetip_core::TipId::from_uuid(tip_id),
            message: if n == 0 {
                mergetip_engine::jobs::TipMessage::Held
            } else {
                mergetip_engine::jobs::TipMessage::Paid
            },
        },
    };
    let (a, b) = (reply(0), reply(1));
    let (ra, rb) = tokio::join!(h.engine.post_reply(&a), h.engine.post_reply(&b));
    ra.unwrap();
    rb.unwrap();

    let posted = h.github.posted_bodies();
    assert_eq!(posted.len(), 1, "{posted:?}");
    let lines = posted[0].lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(
        lines, 3,
        "receipt from the flow plus both concurrent lines:\n{}",
        posted[0]
    );
    Ok(())
}
