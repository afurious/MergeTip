//! Pledges: a verified tip to someone without a Lightning address (task 4.3).

mod common;

use common::Harness;
use mergetip_engine::{
    PLEDGE_WINDOW,
    jobs::{JobPayload, PayTip},
};
use sqlx::PgPool;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_tip_to_someone_without_an_address_becomes_a_claimable_pledge(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    h.wallet_for(10, "maintainer").await;
    h.comment(1001, "/tip @reviewer 5k for the review").await;
    h.drain().await;
    h.run_only(&[PayTip::KIND]).await;
    h.drain().await;
    assert_eq!(h.states().await, ["pledged"]);

    let tip = sqlx::query!("SELECT expires_at, claim_token_hash FROM tips")
        .fetch_one(&h.pool)
        .await?;
    // Postgres keeps microseconds.
    let drift = tip.expires_at.unwrap() - (h.clock_now() + PLEDGE_WINDOW);
    assert!(drift.abs() < time::Duration::milliseconds(1), "{drift}");
    let posted = h.github.posted_bodies();
    let reply = posted
        .iter()
        .find(|b| b.starts_with("⚡ @reviewer"))
        .expect("pledge reply");
    let prefix = "⚡ @reviewer, @maintainer sent you **5,000 sats** for the review. Claim within \
                  30 days → [Claim your sats](https://mergetip.dev/claim/";
    assert!(reply.starts_with(prefix), "{reply}");
    let token = reply[prefix.len()..].trim_end_matches(')');
    assert_eq!(
        tip.claim_token_hash.as_deref(),
        Some(mergetip_crypto::hash_token(token).as_str()),
        "the link's token is the one stored (hashed)"
    );
    Ok(())
}
