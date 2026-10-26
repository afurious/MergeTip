//! High-value tips: nothing in GitHub can release them (task 4.4).

mod common;

use common::Harness;
use mergetip_core::{TipId, TipState};
use mergetip_engine::{
    confirm,
    jobs::{JobPayload, PayTip},
};
use mergetip_store::tips;
use sqlx::PgPool;

async fn high_value(h: &mut Harness, body: &str) -> TipId {
    h.wallet_for(10, "maintainer").await;
    sqlx::query!("UPDATE users SET per_tip_max_sats = 1000000 WHERE github_id = 10")
        .execute(&h.pool)
        .await
        .unwrap();
    h.comment(1001, body).await;
    h.drain().await;
    assert_eq!(h.states().await, ["awaiting_confirmation"]);
    let id = sqlx::query_scalar!("SELECT id FROM tips")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    TipId::from_uuid(id)
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn no_comment_can_confirm_a_high_value_tip(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    high_value(&mut h, "/tip @reviewer 80k").await;
    let posted = h.github.posted_bodies();
    assert!(
        posted
            .iter()
            .any(|b| b.starts_with("🔐 @maintainer, this **80,000 sats**"))
    );

    // Someone with the sender's GitHub session tries every comment-side lever.
    for (id, body) in [
        (1002, "/confirm"),
        (1003, "/confirm 80k"),
        (1004, "/tip confirm"),
        (1001, "/tip @reviewer 80k"),
    ] {
        h.comment(id, body).await;
    }
    h.drain().await;
    assert_eq!(h.states().await[0], "awaiting_confirmation");
    assert_eq!(h.queued(PayTip::KIND).await, 0);
    Ok(())
}

/// Confirmed after the PR already merged: the hold resolves straight to paying.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_confirmed_on_merge_tip_pays_if_the_pr_already_merged(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    let id = high_value(&mut h, "/tip @author 80k on-merge").await;
    h.set_pull("closed", true);

    let mut tx = h.pool.begin().await?;
    let tip = tips::lock_for_update(&mut tx, id).await.unwrap();
    let held = confirm::confirm(&mut tx, &tip, h.clock_now())
        .await
        .unwrap();
    assert_eq!(held.state, TipState::Held);
    tx.commit().await?;

    h.drain().await;
    assert_eq!(h.states().await, ["paying"]);
    assert_eq!(h.queued(PayTip::KIND).await, 1);
    Ok(())
}
