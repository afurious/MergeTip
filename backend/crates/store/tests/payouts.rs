//! Fiat payout schema (task 5.1).

mod common;

use std::str::FromStr;

use mergetip_store::{
    bitnob_events,
    payout_methods::{self, DestinationType, NewPayoutMethod},
    tips::{self, FiatDetails},
};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

fn method(user: mergetip_core::UserId, country: &str, is_default: bool) -> NewPayoutMethod<'_> {
    NewPayoutMethod {
        id: Uuid::new_v4(),
        user_id: user,
        country,
        currency: "NGN",
        destination_type: DestinationType::Bank,
        beneficiary_ciphertext: b"sealed",
        provider_customer_id: None,
        verified_at: OffsetDateTime::now_utc(),
        is_default,
    }
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_user_has_at_most_one_default_payout_method(pool: PgPool) -> sqlx::Result<()> {
    let alice = common::user(&pool, 1, "alice").await;
    let mut conn = pool.acquire().await?;
    let first = payout_methods::insert(&mut conn, &method(alice.id, "NG", true))
        .await
        .unwrap();
    let second = payout_methods::insert(&mut conn, &method(alice.id, "GH", true))
        .await
        .unwrap();

    let methods = payout_methods::list_for_user(&pool, alice.id)
        .await
        .unwrap();
    assert_eq!(methods.len(), 2);
    assert_eq!(
        payout_methods::default_for_user(&pool, alice.id)
            .await
            .unwrap()
            .map(|m| m.id),
        Some(second.id),
        "adding a new default replaces the old one"
    );

    assert!(
        payout_methods::set_default(&mut conn, alice.id, first.id, true)
            .await
            .unwrap()
    );
    assert_eq!(
        payout_methods::default_for_user(&pool, alice.id)
            .await
            .unwrap()
            .map(|m| m.id),
        Some(first.id)
    );
    // The database itself refuses two defaults.
    let both = sqlx::query!(
        "UPDATE payout_methods SET is_default = true WHERE user_id = $1",
        alice.id.as_uuid()
    )
    .execute(&pool)
    .await;
    assert!(both.is_err());

    // Someone else's method can't be touched.
    let bob = common::user(&pool, 2, "bob").await;
    assert!(
        !payout_methods::delete(&pool, bob.id, first.id)
            .await
            .unwrap()
    );
    assert!(
        payout_methods::delete(&pool, alice.id, first.id)
            .await
            .unwrap()
    );
    assert_eq!(
        payout_methods::default_for_user(&pool, alice.id)
            .await
            .unwrap(),
        None
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn a_fiat_method_needs_its_destination(pool: PgPool) -> sqlx::Result<()> {
    let alice = common::user(&pool, 1, "alice").await;
    let incomplete = sqlx::query!(
        "INSERT INTO payout_methods (user_id, kind, country) VALUES ($1, 'bitnob_fiat', 'NG')",
        alice.id.as_uuid()
    )
    .execute(&pool)
    .await;
    assert!(incomplete.is_err());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn fiat_amounts_round_trip_exactly(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 7).await;
    let sender = common::user(&pool, 1, "sender").await;
    let mut conn = pool.acquire().await?;
    let tip = tips::insert_idempotent(&mut conn, &common::new_tip(&installation, &sender, 1, 0))
        .await
        .unwrap()
        .unwrap();
    let fiat = FiatDetails {
        currency: Some("NGN".into()),
        amount: Some(Decimal::from_str("3456.78").unwrap()),
        fx_rate: Some(Decimal::from_str("164608093.123456789").unwrap()),
        fees: Some(Decimal::from_str("0.10").unwrap()),
        quote_id: Some("QT2_1".into()),
        payout_id: Some("po_1".into()),
        provider_status: Some("initiated".into()),
    };
    tips::set_fiat_details(&pool, tip.id, &fiat).await.unwrap();
    assert_eq!(tips::get(&pool, tip.id).await.unwrap().fiat, fiat);

    let found = tips::lock_by_payout_id(&mut conn, "po_1").await.unwrap();
    assert_eq!(found.map(|t| t.id), Some(tip.id));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn bitnob_events_are_deduplicated(pool: PgPool) -> sqlx::Result<()> {
    let payload = json!({ "event": "payouts.processing", "event_id": "ev_1" });
    assert!(
        bitnob_events::insert_if_new(&pool, "ev_1", &payload)
            .await
            .unwrap()
    );
    assert!(
        !bitnob_events::insert_if_new(&pool, "ev_1", &payload)
            .await
            .unwrap()
    );
    let (stored, processed) = bitnob_events::get(&pool, "ev_1").await.unwrap();
    assert_eq!(stored, payload);
    assert!(processed.is_none());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn queue_health_counts_ready_and_dead_jobs(pool: PgPool) -> sqlx::Result<()> {
    use mergetip_store::{jobs, observability};
    let empty = observability::queue_health(&pool).await.unwrap();
    assert_eq!((empty.ready, empty.dead), (0, 0));
    assert_eq!(empty.oldest_age_seconds, 0.0);

    let mut conn = pool.acquire().await?;
    let past = OffsetDateTime::now_utc() - time::Duration::minutes(10);
    jobs::enqueue(&mut conn, &jobs::NewJob::new("a", json!({})).run_at(past))
        .await
        .unwrap();
    jobs::enqueue(&mut conn, &jobs::NewJob::new("b", json!({})))
        .await
        .unwrap();
    jobs::enqueue(
        &mut conn,
        &jobs::NewJob::new("later", json!({}))
            .run_at(OffsetDateTime::now_utc() + time::Duration::hours(1)),
    )
    .await
    .unwrap();
    sqlx::query!("INSERT INTO jobs (kind, dead_at) VALUES ('dead', now())")
        .execute(&pool)
        .await?;
    let health = observability::queue_health(&pool).await.unwrap();
    assert_eq!(health.ready, 2, "due and unclaimed only");
    assert_eq!(health.dead, 1);
    assert!(
        health.oldest_age_seconds >= 590.0,
        "{}",
        health.oldest_age_seconds
    );
    assert_eq!(
        observability::multiply_settled_tips(&pool).await.unwrap(),
        0
    );
    Ok(())
}
