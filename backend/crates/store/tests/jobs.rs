use std::{collections::HashSet, time::Duration};

use mergetip_store::{
    StoreError,
    jobs::{self, Enqueued, NewJob},
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgListener};
use time::OffsetDateTime;

const LEASE: Duration = Duration::from_secs(300);

fn kinds(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

async fn enqueue(pool: &PgPool, job: NewJob) -> Enqueued {
    let mut conn = pool.acquire().await.unwrap();
    jobs::enqueue(&mut conn, &job).await.unwrap()
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn claimed_job_completes_and_disappears(pool: PgPool) -> sqlx::Result<()> {
    enqueue(&pool, NewJob::new("noop", json!({ "n": 1 }))).await;

    let job = jobs::claim(&pool, &kinds(&["noop"]), "w1/a", LEASE)
        .await
        .unwrap()
        .expect("a ready job");
    assert_eq!(job.payload, json!({ "n": 1 }));
    assert_eq!(job.attempts, 1);
    assert_eq!(job.lease, "w1/a");
    assert!(
        jobs::claim(&pool, &kinds(&["noop"]), "w2/b", LEASE)
            .await
            .unwrap()
            .is_none(),
        "a leased job must not be claimed twice"
    );

    jobs::complete(&pool, &job).await.unwrap();
    let left = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM jobs"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn only_ready_jobs_of_requested_kinds_are_claimed(pool: PgPool) -> sqlx::Result<()> {
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    enqueue(&pool, NewJob::new("pay_tip", json!({})).run_at(later)).await;
    enqueue(&pool, NewJob::new("send_email", json!({}))).await;

    let claimed = jobs::claim(&pool, &kinds(&["pay_tip"]), "w/1", LEASE)
        .await
        .unwrap();
    assert!(claimed.is_none());
    Ok(())
}

/// Two workers draining the same queue never receive the same job.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn concurrent_claimers_never_share_a_job(pool: PgPool) -> sqlx::Result<()> {
    for n in 0..100 {
        enqueue(&pool, NewJob::new("noop", json!({ "n": n }))).await;
    }

    let workers = (0..2).map(|w| {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut claimed = Vec::new();
            for attempt in 0..100 {
                let token = format!("w{w}/{attempt}");
                match jobs::claim(&pool, &kinds(&["noop"]), &token, LEASE)
                    .await
                    .unwrap()
                {
                    Some(job) => claimed.push(job.id),
                    None => break,
                }
            }
            claimed
        })
    });

    let mut all = Vec::new();
    for worker in workers {
        all.extend(worker.await.unwrap());
    }
    let unique: HashSet<_> = all.iter().copied().collect();
    assert_eq!(all.len(), 100, "every job claimed exactly once");
    assert_eq!(unique.len(), 100, "no job claimed twice");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn expired_lease_is_reclaimed_and_old_lease_is_void(pool: PgPool) -> sqlx::Result<()> {
    enqueue(&pool, NewJob::new("pay_tip", json!({}))).await;
    let first = jobs::claim(&pool, &kinds(&["pay_tip"]), "crashed/1", LEASE)
        .await
        .unwrap()
        .unwrap();

    // Simulate the worker dying: its lease ran out ten minutes ago.
    sqlx::query!(
        "UPDATE jobs SET locked_at = now() - interval '10 minutes' WHERE id = $1",
        first.id
    )
    .execute(&pool)
    .await?;

    let second = jobs::claim(&pool, &kinds(&["pay_tip"]), "healthy/1", LEASE)
        .await
        .unwrap()
        .expect("expired lease is reclaimable");
    assert_eq!(second.id, first.id);
    assert_eq!(second.attempts, 2);

    assert!(matches!(
        jobs::complete(&pool, &first).await,
        Err(StoreError::Conflict)
    ));
    jobs::complete(&pool, &second).await.unwrap();
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn dedupe_key_allows_one_live_job(pool: PgPool) -> sqlx::Result<()> {
    let job = || NewJob::new("send_email", json!({})).dedupe_key("digest:u1:2026-11-02");
    assert!(matches!(enqueue(&pool, job()).await, Enqueued::Created(_)));
    assert_eq!(enqueue(&pool, job()).await, Enqueued::Deduplicated);

    let claimed = jobs::claim(&pool, &kinds(&["send_email"]), "w/1", LEASE)
        .await
        .unwrap()
        .unwrap();
    jobs::fail_permanently(&pool, &claimed, "smtp down")
        .await
        .unwrap();

    assert!(
        matches!(enqueue(&pool, job()).await, Enqueued::Created(_)),
        "a dead job no longer blocks its dedupe key"
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn retry_reschedules_and_dead_jobs_are_never_claimed(pool: PgPool) -> sqlx::Result<()> {
    enqueue(&pool, NewJob::new("pay_tip", json!({})).max_attempts(2)).await;
    let ks = kinds(&["pay_tip"]);

    let first = jobs::claim(&pool, &ks, "w/1", LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(!first.is_last_attempt());
    jobs::retry(&pool, &first, "relay timeout", Duration::ZERO)
        .await
        .unwrap();

    let second = jobs::claim(&pool, &ks, "w/2", LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.last_error.as_deref(), Some("relay timeout"));
    assert!(second.is_last_attempt());
    jobs::fail_permanently(&pool, &second, "relay timeout")
        .await
        .unwrap();

    assert!(
        jobs::claim(&pool, &ks, "w/3", LEASE)
            .await
            .unwrap()
            .is_none()
    );
    let dead =
        sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM jobs WHERE dead_at IS NOT NULL"#)
            .fetch_one(&pool)
            .await?;
    assert_eq!(dead, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn reschedule_keeps_one_row_with_a_fresh_budget(pool: PgPool) -> sqlx::Result<()> {
    enqueue(
        &pool,
        NewJob::new("expire_tips", json!({})).dedupe_key("expire_tips"),
    )
    .await;
    let job = jobs::claim(&pool, &kinds(&["expire_tips"]), "w/1", LEASE)
        .await
        .unwrap()
        .unwrap();

    jobs::reschedule(&pool, &job, Duration::ZERO).await.unwrap();

    let again = jobs::claim(&pool, &kinds(&["expire_tips"]), "w/2", LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.id, job.id);
    assert_eq!(again.attempts, 1);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn enqueue_notifies_listeners_on_commit_only(pool: PgPool) -> sqlx::Result<()> {
    let mut listener = PgListener::connect_with(&pool).await?;
    listener.listen(jobs::NOTIFY_CHANNEL).await?;

    let mut tx = pool.begin().await?;
    jobs::enqueue(&mut tx, &NewJob::new("rolled_back", json!({})))
        .await
        .unwrap();
    tx.rollback().await?;

    let mut tx = pool.begin().await?;
    jobs::enqueue(&mut tx, &NewJob::new("verify_tip", json!({})))
        .await
        .unwrap();
    tx.commit().await?;

    let notification = tokio::time::timeout(Duration::from_secs(5), listener.recv())
        .await
        .expect("notification arrives")?;
    assert_eq!(notification.payload(), "verify_tip");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn retry_delay_is_measured_on_the_database_clock(pool: PgPool) -> sqlx::Result<()> {
    enqueue(&pool, NewJob::new("pay_tip", json!({}))).await;
    let ks = kinds(&["pay_tip"]);
    let job = jobs::claim(&pool, &ks, "w/1", LEASE)
        .await
        .unwrap()
        .unwrap();

    jobs::retry(&pool, &job, "rate limited", Duration::from_secs(3600))
        .await
        .unwrap();

    assert!(
        jobs::claim(&pool, &ks, "w/2", LEASE)
            .await
            .unwrap()
            .is_none()
    );
    let wait = sqlx::query_scalar!(
        r#"SELECT extract(epoch FROM run_at - now())::float8 AS "secs!" FROM jobs"#
    )
    .fetch_one(&pool)
    .await?;
    assert!((3590.0..=3600.0).contains(&wait), "run_at is {wait}s away");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn heartbeat_keeps_a_long_job_leased(pool: PgPool) -> sqlx::Result<()> {
    enqueue(&pool, NewJob::new("pay_tip", json!({}))).await;
    let ks = kinds(&["pay_tip"]);
    let job = jobs::claim(&pool, &ks, "slow/1", LEASE)
        .await
        .unwrap()
        .unwrap();

    // The job has been running longer than its lease...
    sqlx::query!(
        "UPDATE jobs SET locked_at = now() - interval '10 minutes' WHERE id = $1",
        job.id
    )
    .execute(&pool)
    .await?;
    // ...but its worker is alive and renews the lease.
    jobs::heartbeat(&pool, &job).await.unwrap();

    assert!(
        jobs::claim(&pool, &ks, "other/1", LEASE)
            .await
            .unwrap()
            .is_none()
    );
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn heartbeat_reports_a_lost_lease(pool: PgPool) -> sqlx::Result<()> {
    enqueue(&pool, NewJob::new("pay_tip", json!({}))).await;
    let ks = kinds(&["pay_tip"]);
    let stale = jobs::claim(&pool, &ks, "slow/1", LEASE)
        .await
        .unwrap()
        .unwrap();
    sqlx::query!(
        "UPDATE jobs SET locked_at = now() - interval '10 minutes' WHERE id = $1",
        stale.id
    )
    .execute(&pool)
    .await?;
    jobs::claim(&pool, &ks, "other/1", LEASE)
        .await
        .unwrap()
        .unwrap();

    assert!(matches!(
        jobs::heartbeat(&pool, &stale).await,
        Err(StoreError::Conflict)
    ));
    Ok(())
}
