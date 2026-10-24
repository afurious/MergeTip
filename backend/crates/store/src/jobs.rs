//! Postgres job queue.
//!
//! Jobs are enqueued in the same transaction as the state change that needs them, so a job
//! exists exactly when its tip state says it should. Workers claim with `FOR UPDATE SKIP LOCKED`
//! and a lease; a job whose worker died is reclaimed once the lease expires.
//!
//! Every claim gets a unique lease token stored in `locked_by`. Completing, retrying or failing a
//! job requires that token, so a worker whose lease expired cannot finish a job that another
//! worker has since claimed.

use std::time::Duration;

use serde_json::Value;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;

use crate::error::{StoreError, StoreResult};

/// The Postgres `NOTIFY` channel; the payload is the job kind.
pub const NOTIFY_CHANNEL: &str = "jobs";
pub const DEFAULT_MAX_ATTEMPTS: i32 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct NewJob {
    pub kind: String,
    pub payload: Value,
    /// `None` runs as soon as a worker is free.
    pub run_at: Option<OffsetDateTime>,
    /// At most one live (not dead) job per `(kind, dedupe_key)`; later enqueues are dropped.
    pub dedupe_key: Option<String>,
    pub max_attempts: i32,
}

impl NewJob {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
            run_at: None,
            dedupe_key: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }

    pub fn run_at(mut self, at: OffsetDateTime) -> Self {
        self.run_at = Some(at);
        self
    }

    pub fn dedupe_key(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    pub fn max_attempts(mut self, attempts: i32) -> Self {
        self.max_attempts = attempts;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    Created(i64),
    /// A live job with the same kind and dedupe key already exists.
    Deduplicated,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub payload: Value,
    pub run_at: OffsetDateTime,
    /// Runs started so far, including the current one (incremented on claim).
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
    /// Token proving this worker holds the job; required to complete, retry or fail it.
    pub lease: String,
}

impl Job {
    /// True when the job was reclaimed after its final attempt crashed. The runner dead-letters
    /// such jobs instead of running them again.
    pub fn is_exhausted(&self) -> bool {
        self.attempts > self.max_attempts
    }

    /// True when a failure now should be final.
    pub fn is_last_attempt(&self) -> bool {
        self.attempts >= self.max_attempts
    }
}

pub async fn enqueue(conn: &mut PgConnection, job: &NewJob) -> StoreResult<Enqueued> {
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO jobs (kind, payload, run_at, dedupe_key, max_attempts)
        VALUES ($1, $2, COALESCE($3, now()), $4, $5)
        ON CONFLICT (kind, dedupe_key) WHERE dedupe_key IS NOT NULL AND dead_at IS NULL
        DO NOTHING
        RETURNING id
        "#,
        job.kind,
        job.payload,
        job.run_at,
        job.dedupe_key,
        job.max_attempts,
    )
    .fetch_optional(&mut *conn)
    .await?;

    let Some(id) = id else {
        return Ok(Enqueued::Deduplicated);
    };
    // Delivered when the surrounding transaction commits, never for a rolled-back enqueue.
    sqlx::query!("SELECT pg_notify($1, $2)", NOTIFY_CHANNEL, job.kind)
        .execute(&mut *conn)
        .await?;
    Ok(Enqueued::Created(id))
}

/// Claims the next ready job of one of `kinds`, or a job whose previous lease has expired.
pub async fn claim(
    exec: impl PgExecutor<'_>,
    kinds: &[String],
    lease_token: &str,
    lease: Duration,
) -> StoreResult<Option<Job>> {
    let job = sqlx::query_as!(
        Job,
        r#"
        UPDATE jobs
        SET locked_at = now(), locked_by = $2, attempts = attempts + 1
        WHERE id = (
            SELECT id FROM jobs
            WHERE dead_at IS NULL
              AND run_at <= now()
              AND kind = ANY($1)
              AND (locked_at IS NULL OR locked_at < now() - make_interval(secs => $3))
            ORDER BY run_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        RETURNING id, kind, payload, run_at, attempts, max_attempts, last_error,
                  locked_by AS "lease!"
        "#,
        kinds,
        lease_token,
        lease.as_secs_f64(),
    )
    .fetch_optional(exec)
    .await?;
    Ok(job)
}

/// Finished successfully: the row is deleted.
pub async fn complete(exec: impl PgExecutor<'_>, job: &Job) -> StoreResult<()> {
    let result = sqlx::query!(
        "DELETE FROM jobs WHERE id = $1 AND locked_by = $2",
        job.id,
        job.lease,
    )
    .execute(exec)
    .await?;
    lease_held(result.rows_affected())
}

/// Failed but worth retrying at `run_at`.
pub async fn retry(
    exec: impl PgExecutor<'_>,
    job: &Job,
    error: &str,
    run_at: OffsetDateTime,
) -> StoreResult<()> {
    let result = sqlx::query!(
        r#"
        UPDATE jobs SET run_at = $3, last_error = $4, locked_at = NULL, locked_by = NULL
        WHERE id = $1 AND locked_by = $2
        "#,
        job.id,
        job.lease,
        run_at,
        error,
    )
    .execute(exec)
    .await?;
    lease_held(result.rows_affected())
}

/// Gave up: the row stays with `dead_at` set, for inspection and manual replay.
pub async fn fail_permanently(
    exec: impl PgExecutor<'_>,
    job: &Job,
    error: &str,
) -> StoreResult<()> {
    let result = sqlx::query!(
        r#"
        UPDATE jobs SET dead_at = now(), last_error = $3, locked_at = NULL, locked_by = NULL
        WHERE id = $1 AND locked_by = $2
        "#,
        job.id,
        job.lease,
        error,
    )
    .execute(exec)
    .await?;
    lease_held(result.rows_affected())
}

/// Runs the same job again at `run_at` with a fresh attempt budget. Recurring jobs use this
/// instead of enqueueing a successor, which their own dedupe key would otherwise drop.
pub async fn reschedule(
    exec: impl PgExecutor<'_>,
    job: &Job,
    run_at: OffsetDateTime,
) -> StoreResult<()> {
    let result = sqlx::query!(
        r#"
        UPDATE jobs SET run_at = $3, attempts = 0, last_error = NULL,
                        locked_at = NULL, locked_by = NULL
        WHERE id = $1 AND locked_by = $2
        "#,
        job.id,
        job.lease,
        run_at,
    )
    .execute(exec)
    .await?;
    lease_held(result.rows_affected())
}

fn lease_held(rows: u64) -> StoreResult<()> {
    if rows == 1 {
        Ok(())
    } else {
        Err(StoreError::Conflict)
    }
}

/// Exponential backoff: `base * 2^(attempt - 1)`, capped, then spread by ±20% so jobs that failed
/// together (e.g. during a relay outage) don't all retry in the same instant.
///
/// `jitter` is a uniform sample in `[0, 1)`; taking it as an argument keeps this function pure.
pub fn backoff(attempt: i32, base: Duration, cap: Duration, jitter: f64) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1))
        .unwrap_or(0)
        .min(30);
    let delay = base.saturating_mul(1 << exponent).min(cap);
    let factor = 0.8 + 0.4 * jitter.clamp(0.0, 1.0);
    delay.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_secs(10);
    const CAP: Duration = Duration::from_secs(600);

    #[test]
    fn backoff_doubles_per_attempt() {
        assert_eq!(backoff(1, BASE, CAP, 0.5), Duration::from_secs(10));
        assert_eq!(backoff(2, BASE, CAP, 0.5), Duration::from_secs(20));
        assert_eq!(backoff(3, BASE, CAP, 0.5), Duration::from_secs(40));
    }

    #[test]
    fn backoff_is_capped_even_for_huge_attempt_counts() {
        assert_eq!(backoff(50, BASE, CAP, 0.5), CAP);
        assert_eq!(backoff(i32::MAX, BASE, CAP, 0.5), CAP);
    }

    #[test]
    fn backoff_jitter_stays_within_twenty_percent() {
        assert_eq!(backoff(1, BASE, CAP, 0.0), Duration::from_secs(8));
        assert!(backoff(1, BASE, CAP, 0.999_999) < Duration::from_secs(12));
        assert_eq!(backoff(1, BASE, CAP, 7.0), Duration::from_secs(12));
        assert_eq!(backoff(0, BASE, CAP, 0.5), Duration::from_secs(10));
    }
}
