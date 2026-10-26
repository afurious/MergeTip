//! The PRD's non-happy journeys, end to end (task 4.9): an on-merge pledge claimed by a
//! newcomer (journey B), a sender who never connects a wallet (journey C, expired), and a
//! high-value tip confirmed in the web app with an emailed code.

mod common;

use axum::http::StatusCode;
use common::{System, WorkerOptions, claim_link};
use serde_json::json;
use sqlx::PgPool;
use time::Duration;

/// Journey B: `/tip @author 5000 on-merge` is held, the PR merges, the author has no Lightning
/// address so it becomes a pledge, and they claim it by signing in and pasting one.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_on_merge_pledge_is_claimed_by_a_newcomer(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.connect_wallet().await;
    let worker = system.worker(WorkerOptions::default());

    let status = system
        .deliver("delivery-1", "/tip @author 5000 on-merge for shipping it")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    system.wait_for_idle_queue().await;
    assert_eq!(system.tip_state().await, "held");
    assert!(system.wallet.paid_hashes().is_empty());

    assert_eq!(system.merge_pr("delivery-2").await, StatusCode::ACCEPTED);
    system.wait_for_idle_queue().await;
    assert_eq!(system.tip_state().await, "pledged");
    assert!(
        system.wallet.paid_hashes().is_empty(),
        "nothing charged yet"
    );
    let token = claim_link(&system.comments().await);

    // The claim page works before signing in.
    let (status, body) = system
        .call(
            axum::http::Request::get(format!("/v1/claims/{token}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tip"]["kind"], "on_merge");

    let author = system.sign_in("author-code").await;
    let address = format!("author@{}", system.lnurl.address());
    let (status, body) = system
        .write(
            "POST",
            &format!("/v1/claims/{token}"),
            &author,
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
    assert!(
        system
            .comments()
            .await
            .iter()
            .any(|b| b.contains("⚡ @maintainer tipped @author **5,000 sats** for shipping it")),
        "receipt posted"
    );
    worker.stop().await;
    Ok(())
}

/// Journey C, expired: the sender never connects a wallet within 24 h, so the tip is cancelled,
/// and connecting later doesn't bring it back.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_tip_whose_sender_never_connects_is_cancelled(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.link_address("reviewer").await;
    let worker = system.worker(WorkerOptions::default());

    system
        .deliver("delivery-1", "/tip @reviewer 2100 for the review")
        .await;
    system.wait_for_idle_queue().await;
    assert_eq!(system.tip_state().await, "awaiting_sender");

    system
        .clock
        .advance(Duration::hours(24) + Duration::minutes(1));
    let report = worker.engine.expire_tips().await.unwrap();
    assert_eq!(report.cancelled, 1);
    system.wait_for_idle_queue().await;
    assert_eq!(system.tip_state().await, "cancelled");
    assert!(
        system
            .comments()
            .await
            .iter()
            .any(|b| b.contains("was cancelled: no wallet was connected within 24 h")),
        "status line posted"
    );

    system.connect_wallet().await;
    system.wait_for_idle_queue().await;
    assert_eq!(system.tip_state().await, "cancelled");
    assert!(system.wallet.paid_hashes().is_empty());
    worker.stop().await;
    Ok(())
}

/// A tip over the sender's threshold waits for confirmation in the web app, with the code
/// emailed to them, and only then is paid.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_high_value_tip_is_paid_only_after_web_confirmation(pool: PgPool) -> sqlx::Result<()> {
    let system = System::start(pool).await;
    system.link_address("reviewer").await;
    let maintainer = system.connect_wallet().await;
    let (status, body) = system
        .write(
            "PATCH",
            "/v1/me/settings",
            &maintainer,
            json!({
                "per_tip_max_sats": 100_000, "daily_max_sats": 200_000,
                "confirm_otp_enabled": true
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let worker = system.worker(WorkerOptions::default());

    system.deliver("delivery-1", "/tip @reviewer 60000").await;
    system.wait_for_idle_queue().await;
    assert_eq!(system.tip_state().await, "awaiting_confirmation");
    assert!(
        system.wallet.paid_hashes().is_empty(),
        "not paid unconfirmed"
    );
    let tip_id = sqlx::query_scalar!("SELECT id FROM tips")
        .fetch_one(&system.pool)
        .await?;
    let confirm_link = format!("https://mergetip.dev/confirm/{tip_id}");
    assert!(
        system
            .comments()
            .await
            .iter()
            .any(|b| b.starts_with("🔐 @maintainer") && b.contains(&confirm_link)),
        "confirmation request posted"
    );

    // Without the code: refused.
    let confirm = format!("/v1/confirmations/{tip_id}");
    let (status, body) = system.write("POST", &confirm, &maintainer, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = system
        .write("POST", &format!("{confirm}/otp"), &maintainer, json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let email = system
        .api_mail
        .sent()
        .last()
        .cloned()
        .expect("a code email");
    assert_eq!(email.to, "maintainer@example.com");
    let code = email
        .text
        .split(|c: char| !c.is_ascii_digit())
        .find(|w| w.len() == 6)
        .expect("a 6-digit code")
        .to_owned();

    let (status, body) = system
        .write("POST", &confirm, &maintainer, json!({ "otp": code }))
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
