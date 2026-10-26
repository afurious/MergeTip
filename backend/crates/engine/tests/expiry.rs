//! The expiry job (task 4.7): every window, reminders, and concurrent runs.

mod common;

use common::Harness;
use futures::future::join;
use mergetip_core::{GithubInstallationId, Sats, ThreadRef, TipKind, TipState};
use mergetip_engine::{
    ExpiryReport,
    jobs::{self, ExpireTips, JobPayload, PayTip},
};
use mergetip_github::testing::FakeGitHub;
use mergetip_store::{
    installations,
    tips::{self, NewTip, TipPatch},
    users,
};
use serde_json::json;
use sqlx::PgPool;
use time::Duration;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_unconfirmed_tip_is_cancelled_after_an_hour(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    h.wallet_for(10, "maintainer").await;
    sqlx::query!("UPDATE users SET per_tip_max_sats = 1000000 WHERE github_id = 10")
        .execute(&h.pool)
        .await?;
    h.comment(1001, "/tip @reviewer 80k").await;
    h.drain().await;

    h.clock.advance(Duration::minutes(59));
    assert_eq!(h.engine.expire_tips().await.unwrap().cancelled, 0);
    h.clock.advance(Duration::minutes(2));
    assert_eq!(h.engine.expire_tips().await.unwrap().cancelled, 1);
    h.drain().await;
    assert_eq!(
        h.tips().await,
        [(
            "reviewer".into(),
            "cancelled".into(),
            Some("confirmation_timeout".into())
        )]
    );
    assert!(
        h.status_comment()
            .contains("was cancelled: it wasn't confirmed within 1 hour"),
        "{}",
        h.status_comment()
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn an_unclaimed_pledge_expires_after_30_days(pool: PgPool) -> sqlx::Result<()> {
    let mut h = Harness::new(pool);
    h.wallet_for(10, "maintainer").await;
    let mut reviewer = FakeGitHub::account(11, "reviewer");
    reviewer.email = Some("reviewer@example.com".into());
    h.github.add_user(reviewer);
    h.comment(1001, "/tip @reviewer 5k").await;
    h.drain().await;
    h.run_only(&[PayTip::KIND]).await;
    h.drain().await;
    assert_eq!(h.states().await, ["pledged"]);
    assert_eq!(h.mailer.sent().len(), 1, "the pledge email");

    h.clock.advance(Duration::days(30) - Duration::minutes(1));
    let report = h.engine.expire_tips().await.unwrap();
    assert_eq!(report.expired, 0);
    assert_eq!(report.reminders, 1, "day 23's reminder was still due");

    h.clock.advance(Duration::minutes(2));
    assert_eq!(h.engine.expire_tips().await.unwrap().expired, 1);
    h.release_jobs_due_by_clock().await;
    h.drain().await;
    assert_eq!(h.states().await, ["expired"]);
    let told_sender = h
        .github
        .posted_bodies()
        .into_iter()
        .find(|b| b.starts_with("⌛"))
        .expect("the sender is told in the thread");
    assert_eq!(
        told_sender,
        "⌛ @maintainer, @reviewer didn't claim your **5,000 sats** tip within 30 days, so it \
         expired. Nothing was charged."
    );
    // The reminder queued at the last minute folded into the final notice: one email, not two.
    let sent = h.mailer.sent().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].tag, "expiry");
    assert!(
        sent[1].text.contains("wasn't claimed within 30 days"),
        "{}",
        sent[1].text
    );
    Ok(())
}

/// Many tips past their deadline, two workers expiring at once: each tip moves exactly once.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn concurrent_expiry_runs_never_move_a_tip_twice(pool: PgPool) -> sqlx::Result<()> {
    let h = Harness::new(pool);
    let installation = installations::upsert(&h.pool, GithubInstallationId::new(77), "acme")
        .await
        .unwrap();
    let mut conn = h.pool.acquire().await?;
    let sender = users::upsert_by_github_id(
        &mut conn,
        &users::GithubProfile {
            github_id: mergetip_core::GithubUserId::new(10),
            login: "maintainer".into(),
            avatar_url: None,
            created_at: None,
        },
    )
    .await
    .unwrap();
    for i in 0..60 {
        let tip = tips::insert_idempotent(
            &mut conn,
            &NewTip {
                idempotency_key: format!("k{i}"),
                installation_id: installation.id,
                sender_id: sender.id,
                receiver_login: format!("user{i}"),
                thread: ThreadRef {
                    owner: "acme".into(),
                    repo: "widgets".into(),
                    number: 42,
                    is_pr: true,
                },
                comment_id: i,
                command_index: 0,
                kind: TipKind::Now,
                amount: Sats::new(100),
                note: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        tips::transition(
            &mut conn,
            tip.id,
            TipState::Received,
            TipState::AwaitingSender,
            &TipPatch {
                expires_at: Some(h.clock_now() - Duration::minutes(1)),
                ..TipPatch::default()
            },
            json!({}),
        )
        .await
        .unwrap();
    }
    drop(conn);

    let (a, b) = join(h.engine.expire_tips(), h.engine.expire_tips()).await;
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.cancelled + b.cancelled, 60);
    let cancellations = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM tip_events WHERE to_state = 'cancelled'"#
    )
    .fetch_one(&h.pool)
    .await?;
    assert_eq!(cancellations, 60, "one transition per tip");
    let replies = h.queued("post_reply").await;
    assert_eq!(replies, 60, "one status line per tip");
    assert_eq!(
        h.engine.expire_tips().await.unwrap(),
        ExpiryReport::default()
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn there_is_only_ever_one_expiry_job(pool: PgPool) -> sqlx::Result<()> {
    let h = Harness::new(pool);
    for _ in 0..3 {
        let mut conn = h.pool.acquire().await?;
        jobs::ensure_expiry_job(&mut conn).await.unwrap();
    }
    assert_eq!(h.queued(ExpireTips::KIND).await, 1);
    Ok(())
}
