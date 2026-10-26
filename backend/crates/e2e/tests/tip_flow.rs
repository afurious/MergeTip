//! A `/tip` comment, end to end: sign in → connect a wallet → signed webhook → api → Postgres
//! queue → worker → a real NWC payment → GitHub replies.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{FIXTURE, System, WorkerOptions, claim_link};
use mergetip_payer::testing::behaviour;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;

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
