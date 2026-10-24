mod common;

use mergetip_core::{TipId, TipState};
use mergetip_store::{
    StoreError,
    tips::{self, TipPatch},
};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn replayed_insert_creates_one_tip(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let new = common::new_tip(&installation, &sender, 5001, 0);

    let mut conn = pool.acquire().await?;
    let first = tips::insert_idempotent(&mut conn, &new).await.unwrap();
    let second = tips::insert_idempotent(&mut conn, &new).await.unwrap();

    let tip = first.expect("first insert creates the tip");
    assert!(second.is_none(), "replay must not create a second tip");
    assert_eq!(tip.state, TipState::Received);
    assert_eq!(tip.version, 0);
    assert_eq!(tip.thread.full_name(), "acme/widgets");
    assert_eq!(tip.amount.get(), 2_100);

    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM tips"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    let events = tips::events(&pool, tip.id).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].from_state, None);
    assert_eq!(events[0].to_state, TipState::Received);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn transition_updates_state_and_writes_audit(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let mut conn = pool.acquire().await?;
    let tip = tips::insert_idempotent(&mut conn, &common::new_tip(&installation, &sender, 1, 0))
        .await
        .unwrap()
        .unwrap();

    let moved = tips::transition(
        &mut conn,
        tip.id,
        TipState::Received,
        TipState::Verifying,
        &TipPatch::default(),
        json!({ "reason": "sender_has_wallet" }),
    )
    .await
    .unwrap();

    assert_eq!(moved.state, TipState::Verifying);
    assert_eq!(moved.version, 1);
    let events = tips::events(&pool, tip.id).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].from_state, Some(TipState::Received));
    assert_eq!(events[1].to_state, TipState::Verifying);
    assert_eq!(events[1].detail, json!({ "reason": "sender_has_wallet" }));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn stale_from_state_is_a_conflict(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let mut conn = pool.acquire().await?;
    let tip = tips::insert_idempotent(&mut conn, &common::new_tip(&installation, &sender, 1, 0))
        .await
        .unwrap()
        .unwrap();
    tips::transition(
        &mut conn,
        tip.id,
        TipState::Received,
        TipState::Verifying,
        &TipPatch::default(),
        json!({}),
    )
    .await
    .unwrap();

    let again = tips::transition(
        &mut conn,
        tip.id,
        TipState::Received,
        TipState::Verifying,
        &TipPatch::default(),
        json!({}),
    )
    .await;

    assert!(matches!(again, Err(StoreError::Conflict)), "{again:?}");
    assert_eq!(tips::events(&pool, tip.id).await.unwrap().len(), 2);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn illegal_transition_is_rejected_before_any_write(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let mut conn = pool.acquire().await?;
    let tip = tips::insert_idempotent(&mut conn, &common::new_tip(&installation, &sender, 1, 0))
        .await
        .unwrap()
        .unwrap();

    let result = tips::transition(
        &mut conn,
        tip.id,
        TipState::Received,
        TipState::Paid,
        &TipPatch::default(),
        json!({}),
    )
    .await;

    assert!(
        matches!(result, Err(StoreError::IllegalTransition(_))),
        "{result:?}"
    );
    let unchanged = tips::get(&pool, tip.id).await.unwrap();
    assert_eq!(unchanged.state, TipState::Received);
    assert_eq!(unchanged.version, 0);
    assert_eq!(tips::events(&pool, tip.id).await.unwrap().len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn unknown_tip_is_not_found(pool: PgPool) -> sqlx::Result<()> {
    let mut conn = pool.acquire().await?;
    let result = tips::transition(
        &mut conn,
        TipId::new_random(),
        TipState::Received,
        TipState::Verifying,
        &TipPatch::default(),
        json!({}),
    )
    .await;
    assert!(matches!(result, Err(StoreError::NotFound)), "{result:?}");
    assert!(matches!(
        tips::get(&pool, TipId::new_random()).await,
        Err(StoreError::NotFound)
    ));
    Ok(())
}

/// Two workers racing on the same tip: exactly one wins, the other sees a conflict.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn concurrent_transitions_have_one_winner(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let tip = {
        let mut conn = pool.acquire().await?;
        tips::insert_idempotent(&mut conn, &common::new_tip(&installation, &sender, 1, 0))
            .await
            .unwrap()
            .unwrap()
    };

    let attempts = (0..8).map(|_| {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut conn = pool.acquire().await.unwrap();
            tips::transition(
                &mut conn,
                tip.id,
                TipState::Received,
                TipState::Verifying,
                &TipPatch::default(),
                json!({}),
            )
            .await
        })
    });
    let mut wins = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Ok(_) => wins += 1,
            Err(StoreError::Conflict) => {}
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    assert_eq!(wins, 1);
    assert_eq!(tips::events(&pool, tip.id).await.unwrap().len(), 2);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn transition_rolls_back_with_the_callers_transaction(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let tip = {
        let mut conn = pool.acquire().await?;
        tips::insert_idempotent(&mut conn, &common::new_tip(&installation, &sender, 1, 0))
            .await
            .unwrap()
            .unwrap()
    };

    let mut tx = pool.begin().await?;
    tips::lock_for_update(&mut tx, tip.id).await.unwrap();
    tips::transition(
        &mut tx,
        tip.id,
        TipState::Received,
        TipState::Verifying,
        &TipPatch::default(),
        json!({}),
    )
    .await
    .unwrap();
    tx.rollback().await?;

    assert_eq!(
        tips::get(&pool, tip.id).await.unwrap().state,
        TipState::Received
    );
    assert_eq!(tips::events(&pool, tip.id).await.unwrap().len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn patch_is_applied_with_the_transition_and_never_erases(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let receiver = common::user(&pool, 11, "alice").await;
    let mut conn = pool.acquire().await?;
    let tip = tips::insert_idempotent(&mut conn, &common::new_tip(&installation, &sender, 1, 0))
        .await
        .unwrap()
        .unwrap();

    tips::transition(
        &mut conn,
        tip.id,
        TipState::Received,
        TipState::Verifying,
        &TipPatch::default(),
        json!({}),
    )
    .await
    .unwrap();
    let verified = tips::transition(
        &mut conn,
        tip.id,
        TipState::Verifying,
        TipState::Paying,
        &TipPatch {
            receiver_id: Some(receiver.id),
            verified_by: Some(json!({ "rule": "pr_review", "review_id": 5 })),
            ..TipPatch::default()
        },
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(verified.receiver_id, Some(receiver.id));

    let failed = tips::transition(
        &mut conn,
        tip.id,
        TipState::Paying,
        TipState::Failed,
        &TipPatch::error("insufficient_balance", Some("wallet said no".into())),
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(failed.error_code.as_deref(), Some("insufficient_balance"));
    assert_eq!(failed.error_detail.as_deref(), Some("wallet said no"));
    assert_eq!(
        failed.receiver_id,
        Some(receiver.id),
        "earlier facts are kept"
    );
    assert_eq!(
        failed.verified_by,
        Some(json!({ "rule": "pr_review", "review_id": 5 }))
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn limit_counters_and_rejected_tips(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 1).await;
    let sender = common::user(&pool, 10, "maintainer").await;
    let mut conn = pool.acquire().await?;
    let mut ids = Vec::new();
    for comment in 1..=3 {
        let tip = tips::insert_idempotent(
            &mut conn,
            &common::new_tip(&installation, &sender, comment, 0),
        )
        .await
        .unwrap()
        .unwrap();
        ids.push(tip.id);
    }
    tips::transition(
        &mut conn,
        ids[0],
        TipState::Received,
        TipState::Verifying,
        &TipPatch::default(),
        json!({}),
    )
    .await
    .unwrap();
    tips::transition(
        &mut conn,
        ids[0],
        TipState::Verifying,
        TipState::Rejected,
        &TipPatch::error("no_contribution", None),
        json!({}),
    )
    .await
    .unwrap();

    let an_hour_ago = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let thread = common::new_tip(&installation, &sender, 0, 0).thread;
    assert_eq!(
        tips::count_by_sender_since(&pool, sender.id, an_hour_ago, ids[2])
            .await
            .unwrap(),
        2,
        "the hourly count includes rejected tips, not the current one"
    );
    assert_eq!(
        tips::count_for_pair_in_thread(&pool, sender.id, "ALICE", &thread, ids[2])
            .await
            .unwrap(),
        1
    );
    Ok(())
}
