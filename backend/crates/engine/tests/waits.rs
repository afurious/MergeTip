//! Tips waiting for their sender to connect a wallet (task 4.2).

mod common;

use common::Harness;
use mergetip_engine::{
    ExpiryReport,
    jobs::{JobPayload, PayTip},
};
use sqlx::PgPool;
use time::Duration;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn connecting_a_wallet_resumes_waiting_tips(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    h.comment(1001, "/tip @reviewer 500\n/tip @author 300")
        .await;
    h.drain().await;
    assert_eq!(h.states().await, ["awaiting_sender", "awaiting_sender"]);
    assert!(h.github.posted_bodies()[0].starts_with("🔌 @maintainer, connect your wallet"));

    h.clock.advance(Duration::hours(23));
    h.connect_wallet(10, "maintainer").await;
    h.drain().await;
    assert_eq!(h.states().await, ["paying", "paying"]);
    assert_eq!(h.queued(PayTip::KIND).await, 2);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_tip_past_its_24_hours_is_cancelled_not_resumed(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    h.comment(1001, "/tip @reviewer 500").await;
    h.drain().await;

    h.clock.advance(Duration::hours(24) + Duration::minutes(1));
    // Connecting after the deadline doesn't revive the tip.
    h.connect_wallet(10, "maintainer").await;
    h.drain().await;
    assert_eq!(h.states().await, ["awaiting_sender"]);

    let report = h.engine.expire_tips().await.unwrap();
    assert_eq!(report.cancelled, 1);
    h.drain().await;
    assert_eq!(
        h.tips().await,
        [(
            "reviewer".into(),
            "cancelled".into(),
            Some("sender_timeout".into())
        )]
    );
    assert!(
        h.status_comment().contains(
            "⏳ @maintainer's **500 sats** tip to `reviewer` was cancelled: no wallet was \
             connected within 24 h"
        ),
        "the unverified receiver is not mentioned: {}",
        h.status_comment()
    );

    // A second run finds nothing to do.
    assert_eq!(
        h.engine.expire_tips().await.unwrap(),
        ExpiryReport::default()
    );
    assert_eq!(h.queued(PayTip::KIND).await, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_tip_inside_its_window_is_not_expired(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    h.comment(1001, "/tip @reviewer 500").await;
    h.drain().await;
    h.clock.advance(Duration::hours(23));
    assert_eq!(
        h.engine.expire_tips().await.unwrap(),
        ExpiryReport::default()
    );
    assert_eq!(h.states().await, ["awaiting_sender"]);
    Ok(())
}
