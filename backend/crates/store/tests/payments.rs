mod common;

use mergetip_core::{Msats, TipState};
use mergetip_store::{
    StoreError,
    payment_attempts::{self, AttemptStatus, NewAttempt},
    sessions::{self, SessionKind},
    tips::{self, Tip, TipPatch},
};
use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

async fn paying_tip(pool: &PgPool, comment: i64) -> Tip {
    let installation = common::installation(pool, 1).await;
    let sender = common::user(pool, 10, "maintainer").await;
    let mut conn = pool.acquire().await.unwrap();
    let tip = tips::insert_idempotent(
        &mut conn,
        &common::new_tip(&installation, &sender, comment, 0),
    )
    .await
    .unwrap()
    .unwrap();
    for (from, to) in [
        (TipState::Received, TipState::Verifying),
        (TipState::Verifying, TipState::Paying),
    ] {
        tips::transition(&mut conn, tip.id, from, to, &TipPatch::default(), json!({}))
            .await
            .unwrap();
    }
    tips::get(pool, tip.id).await.unwrap()
}

fn attempt<'a>(tip: &Tip, hash: &'a str) -> NewAttempt<'a> {
    NewAttempt {
        tip_id: tip.id,
        payment_hash: hash,
        bolt11: "lnbcrt1...",
        amount: Msats::new(2_100_000),
        invoice_expires_at: OffsetDateTime::now_utc() + Duration::minutes(10),
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn only_one_open_attempt_per_tip(pool: PgPool) -> sqlx::Result<()> {
    let tip = paying_tip(&pool, 1).await;
    let first = payment_attempts::insert_pending(&pool, &attempt(&tip, "h1"))
        .await
        .unwrap();

    assert!(matches!(
        payment_attempts::insert_pending(&pool, &attempt(&tip, "h2")).await,
        Err(StoreError::Conflict)
    ));
    let now = OffsetDateTime::now_utc();
    payment_attempts::set_status(&pool, first.id, AttemptStatus::Unknown, None, now)
        .await
        .unwrap();
    assert!(
        matches!(
            payment_attempts::insert_pending(&pool, &attempt(&tip, "h2")).await,
            Err(StoreError::Conflict)
        ),
        "an attempt with an unknown outcome still blocks a new one"
    );

    payment_attempts::set_status(
        &pool,
        first.id,
        AttemptStatus::Failed,
        Some("PAYMENT_FAILED"),
        now,
    )
    .await
    .unwrap();
    let second = payment_attempts::insert_pending(&pool, &attempt(&tip, "h2"))
        .await
        .unwrap();
    assert_eq!(
        payment_attempts::open_for_tip(&pool, tip.id)
            .await
            .unwrap()
            .map(|a| a.id),
        Some(second.id)
    );
    assert_eq!(
        payment_attempts::count_for_tip(&pool, tip.id)
            .await
            .unwrap(),
        2
    );
    let failed = payment_attempts::get(&pool, first.id).await.unwrap();
    assert_eq!(failed.nwc_error_code.as_deref(), Some("PAYMENT_FAILED"));
    assert!(failed.finished_at.is_some());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_tip_can_succeed_only_once(pool: PgPool) -> sqlx::Result<()> {
    let tip = paying_tip(&pool, 1).await;
    let now = OffsetDateTime::now_utc();
    let first = payment_attempts::insert_pending(&pool, &attempt(&tip, "h1"))
        .await
        .unwrap();
    payment_attempts::set_status(&pool, first.id, AttemptStatus::Succeeded, None, now)
        .await
        .unwrap();

    // Forced past the open-attempt guard, a second success is still refused.
    sqlx::query!(
        "INSERT INTO payment_attempts (tip_id, payment_hash, bolt11, amount_msat, invoice_expires_at, status)
         VALUES ($1, 'h2', 'x', 1, now(), 'failed')",
        tip.id.as_uuid()
    )
    .execute(&pool)
    .await?;
    let second = sqlx::query_scalar!("SELECT id FROM payment_attempts WHERE payment_hash = 'h2'")
        .fetch_one(&pool)
        .await?;
    assert!(matches!(
        payment_attempts::set_status(&pool, second, AttemptStatus::Succeeded, None, now).await,
        Err(StoreError::Conflict)
    ));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn reserved_amount_counts_in_flight_and_settled_only(pool: PgPool) -> sqlx::Result<()> {
    let now = OffsetDateTime::now_utc();
    let statuses = [
        AttemptStatus::Pending,
        AttemptStatus::Unknown,
        AttemptStatus::Succeeded,
        AttemptStatus::Failed,
    ];
    for (i, status) in statuses.into_iter().enumerate() {
        let tip = paying_tip(&pool, 100 + i64::try_from(i).unwrap()).await;
        let hash = format!("r{i}");
        let a = payment_attempts::insert_pending(&pool, &attempt(&tip, &hash))
            .await
            .unwrap();
        payment_attempts::set_status(&pool, a.id, status, None, now)
            .await
            .unwrap();
    }
    let sender = common::user(&pool, 10, "maintainer").await;
    let reserved = payment_attempts::reserved_since(&pool, sender.id, now - Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(reserved, Msats::new(3 * 2_100_000));
    let none = payment_attempts::reserved_since(&pool, sender.id, now + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(none, Msats::new(0));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn sessions_expire_and_die_with_their_user(pool: PgPool) -> sqlx::Result<()> {
    let user = common::user(&pool, 10, "maintainer").await;
    let now = OffsetDateTime::now_utc();
    let session = sessions::create(
        &pool,
        user.id,
        SessionKind::Web,
        "hash-1",
        now + Duration::days(30),
        Some("ua"),
    )
    .await
    .unwrap();
    sessions::create(
        &pool,
        user.id,
        SessionKind::Extension,
        "hash-2",
        now - Duration::seconds(1),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        sessions::find_active(&pool, "hash-1", now)
            .await
            .unwrap()
            .map(|s| s.id),
        Some(session.id)
    );
    assert!(
        sessions::find_active(&pool, "hash-2", now)
            .await
            .unwrap()
            .is_none(),
        "expired"
    );
    assert!(
        sessions::find_active(&pool, "nope", now)
            .await
            .unwrap()
            .is_none()
    );

    sessions::touch(&pool, session.id, now, now + Duration::days(31))
        .await
        .unwrap();
    let touched = sessions::find_active(&pool, "hash-1", now)
        .await
        .unwrap()
        .unwrap();
    assert!(touched.expires_at > session.expires_at);

    sqlx::query!(
        "UPDATE users SET deleted_at = now() WHERE id = $1",
        user.id.as_uuid()
    )
    .execute(&pool)
    .await?;
    assert!(
        sessions::find_active(&pool, "hash-1", now)
            .await
            .unwrap()
            .is_none(),
        "deleted user"
    );

    sessions::delete_for_user(&pool, user.id).await.unwrap();
    let left = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM sessions"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}
