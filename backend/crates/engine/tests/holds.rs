//! On-merge holds resolved by `pull_request.closed` (task 4.1).

mod common;

use common::{Harness, REPO};
use mergetip_engine::jobs::{JobPayload, PayTip};
use mergetip_github::testing::FakeGitHub;
use sqlx::PgPool;

async fn two_holds(pool: PgPool) -> Harness {
    let mut h = Harness::new(pool);
    h.wallet_for(10, "maintainer").await;
    h.comment(1001, "/tip @author 5k on-merge for shipping it")
        .await;
    h.comment(1002, "/tip @author 1000 on-merge").await;
    h.drain().await;
    assert_eq!(h.states().await, ["held", "held"]);
    h
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_merge_pays_every_held_tip_once(pool: PgPool) -> sqlx::Result<()> {
    let mut h = two_holds(pool).await;
    h.set_pull("closed", true);

    h.pull_closed(true).await;
    h.drain().await;
    assert_eq!(h.states().await, ["paying", "paying"]);
    assert_eq!(h.queued(PayTip::KIND).await, 2);

    // GitHub redelivers the event, and a second delivery of it arrives.
    h.pull_closed(true).await;
    h.pull_closed(true).await;
    h.drain().await;
    assert_eq!(h.states().await, ["paying", "paying"]);
    assert_eq!(h.queued(PayTip::KIND).await, 2, "nothing queued twice");
    let verified = sqlx::query_scalar!("SELECT verified_by FROM tips ORDER BY created_at")
        .fetch_all(&h.pool)
        .await?;
    assert!(
        verified
            .iter()
            .all(|v| v.as_ref().unwrap()["rule"] == "pr_author")
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn closing_without_merge_cancels_the_holds(pool: PgPool) -> sqlx::Result<()> {
    let mut h = two_holds(pool).await;
    h.set_pull("closed", false);

    h.pull_closed(false).await;
    h.drain().await;
    let tips = h.tips().await;
    assert!(
        tips.iter()
            .all(|t| t.1 == "cancelled" && t.2.as_deref() == Some("pr_closed")),
        "{tips:?}"
    );
    let status = h.status_comment();
    assert!(
        status.contains(
            "⏳ @maintainer's **5,000 sats** tip to @author was cancelled: PR closed without \
             merge. Nothing was charged."
        ),
        "{status}"
    );

    // Redelivery: nothing moves and no line is added twice.
    h.pull_closed(false).await;
    h.drain().await;
    assert_eq!(h.status_comment(), status);
    assert_eq!(h.queued(PayTip::KIND).await, 0);
    Ok(())
}

/// The payload is only a hint: GitHub says the PR is still open, so nothing is paid.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_payload_claiming_a_merge_is_not_trusted(pool: PgPool) -> sqlx::Result<()> {
    let mut h = two_holds(pool).await;
    h.pull_closed(true).await;
    h.drain().await;
    assert_eq!(h.states().await, ["held", "held"]);
    assert_eq!(h.queued(PayTip::KIND).await, 0);
    Ok(())
}

/// The receiver authored the PR when the tip was made, but not when it merged.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_receiver_no_longer_on_the_pr_is_not_paid(pool: PgPool) -> sqlx::Result<()> {
    let mut h = two_holds(pool).await;
    h.set_pull("closed", true);
    h.github
        .state()
        .pulls
        .get_mut(&(REPO.into(), 42))
        .unwrap()
        .user = Some(FakeGitHub::account(13, "someone-else"));

    h.pull_closed(true).await;
    h.drain().await;
    let tips = h.tips().await;
    assert!(
        tips.iter()
            .all(|t| t.1 == "cancelled" && t.2.as_deref() == Some("no_contribution")),
        "{tips:?}"
    );
    assert_eq!(h.queued(PayTip::KIND).await, 0);
    Ok(())
}

/// The same repo name and PR number under another installation (a transferred or reinstalled
/// repo) must not release holds made under this one.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn another_installations_merge_leaves_the_holds_alone(pool: PgPool) -> sqlx::Result<()> {
    let mut h = two_holds(pool).await;
    h.set_pull("closed", true);
    mergetip_store::installations::upsert(
        &h.pool,
        mergetip_core::GithubInstallationId::new(88),
        "acme",
    )
    .await
    .unwrap();

    let payload = serde_json::json!({
        "action": "closed",
        "number": 42,
        "pull_request": { "number": 42, "merged": true },
        "repository": {
            "id": 5, "name": "widgets", "full_name": REPO,
            "owner": { "id": 3, "login": "acme", "type": "Organization" }
        },
        "installation": { "id": 88 }
    });
    h.deliver("pull_request", "closed", payload).await;
    h.drain().await;
    assert_eq!(h.states().await, ["held", "held"]);
    assert_eq!(h.queued(PayTip::KIND).await, 0);

    // This installation's own event still releases them.
    h.pull_closed(true).await;
    h.drain().await;
    assert_eq!(h.states().await, ["paying", "paying"]);
    Ok(())
}
