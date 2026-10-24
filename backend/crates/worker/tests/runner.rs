use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use mergetip_core::SystemClock;
use mergetip_store::jobs::{self, Job, NewJob};
use mergetip_worker::{JobContext, JobError, JobHandler, JobOutcome, Runner, RunnerConfig};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn fast_config() -> RunnerConfig {
    RunnerConfig {
        concurrency: 4,
        poll_interval: Duration::from_millis(50),
        drain_timeout: Duration::from_secs(5),
        retry_base: Duration::ZERO,
        retry_cap: Duration::ZERO,
        ..RunnerConfig::default()
    }
}

/// Reports every payload it handles; fails the first `fail_first` runs with `error`.
struct Recorder {
    seen: mpsc::UnboundedSender<serde_json::Value>,
    runs: AtomicUsize,
    fail_first: usize,
    error: fn() -> JobError,
    delay: Duration,
}

impl Recorder {
    fn new(seen: mpsc::UnboundedSender<serde_json::Value>) -> Self {
        Self {
            seen,
            runs: AtomicUsize::new(0),
            fail_first: 0,
            error: || JobError::retryable("flaky"),
            delay: Duration::ZERO,
        }
    }
}

#[async_trait]
impl JobHandler for Recorder {
    fn kind(&self) -> &'static str {
        "record"
    }

    async fn handle(&self, _ctx: &JobContext, job: &Job) -> Result<JobOutcome, JobError> {
        tokio::time::sleep(self.delay).await;
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        if run < self.fail_first {
            return Err((self.error)());
        }
        self.seen.send(job.payload.clone()).ok();
        Ok(JobOutcome::Done)
    }
}

async fn enqueue(pool: &PgPool, job: NewJob) {
    let mut conn = pool.acquire().await.unwrap();
    jobs::enqueue(&mut conn, &job).await.unwrap();
}

async fn job_rows(pool: &PgPool) -> (i64, i64) {
    let row = sqlx::query!(r#"SELECT count(*) AS "total!", count(dead_at) AS "dead!" FROM jobs"#)
        .fetch_one(pool)
        .await
        .unwrap();
    (row.total, row.dead)
}

fn start(pool: &PgPool, handler: Recorder) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let shutdown = CancellationToken::new();
    let runner = Runner::new(pool.clone(), Arc::new(SystemClock), fast_config()).register(handler);
    let handle = tokio::spawn(runner.run(shutdown.clone()));
    (shutdown, handle)
}

async fn recv(rx: &mut mpsc::UnboundedReceiver<serde_json::Value>) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("job handled in time")
        .expect("handler alive")
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn runs_enqueued_jobs_and_deletes_them(pool: PgPool) -> sqlx::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (shutdown, handle) = start(&pool, Recorder::new(tx));

    for n in 0..5 {
        enqueue(&pool, NewJob::new("record", json!({ "n": n }))).await;
    }
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(recv(&mut rx).await["n"].as_i64().unwrap());
    }
    seen.sort_unstable();
    assert_eq!(seen, [0, 1, 2, 3, 4]);

    shutdown.cancel();
    handle.await.unwrap();
    assert_eq!(job_rows(&pool).await, (0, 0));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn retryable_failures_are_retried(pool: PgPool) -> sqlx::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handler = Recorder {
        fail_first: 2,
        ..Recorder::new(tx)
    };
    let (shutdown, handle) = start(&pool, handler);

    enqueue(&pool, NewJob::new("record", json!({ "n": 1 }))).await;
    assert_eq!(recv(&mut rx).await, json!({ "n": 1 }));

    shutdown.cancel();
    handle.await.unwrap();
    assert_eq!(job_rows(&pool).await, (0, 0));
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn exhausted_retries_dead_letter(pool: PgPool) -> sqlx::Result<()> {
    let (tx, _rx) = mpsc::unbounded_channel();
    let handler = Recorder {
        fail_first: usize::MAX,
        ..Recorder::new(tx)
    };
    let (shutdown, handle) = start(&pool, handler);

    enqueue(&pool, NewJob::new("record", json!({})).max_attempts(3)).await;
    wait_until(|| async { job_rows(&pool).await == (1, 1) }).await;

    let error = sqlx::query_scalar!("SELECT last_error FROM jobs")
        .fetch_one(&pool)
        .await?;
    assert_eq!(error.as_deref(), Some("flaky"));
    let attempts = sqlx::query_scalar!("SELECT attempts FROM jobs")
        .fetch_one(&pool)
        .await?;
    assert_eq!(attempts, 3);

    shutdown.cancel();
    handle.await.unwrap();
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn permanent_error_is_not_retried(pool: PgPool) -> sqlx::Result<()> {
    let (tx, _rx) = mpsc::unbounded_channel();
    let handler = Recorder {
        fail_first: usize::MAX,
        error: || JobError::permanent("invalid payload"),
        ..Recorder::new(tx)
    };
    let (shutdown, handle) = start(&pool, handler);

    enqueue(&pool, NewJob::new("record", json!({}))).await;
    wait_until(|| async { job_rows(&pool).await == (1, 1) }).await;
    let attempts = sqlx::query_scalar!("SELECT attempts FROM jobs")
        .fetch_one(&pool)
        .await?;
    assert_eq!(attempts, 1);

    shutdown.cancel();
    handle.await.unwrap();
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn shutdown_drains_the_job_in_flight(pool: PgPool) -> sqlx::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handler = Recorder {
        delay: Duration::from_millis(500),
        ..Recorder::new(tx)
    };
    let (shutdown, handle) = start(&pool, handler);

    enqueue(&pool, NewJob::new("record", json!({ "slow": true }))).await;
    wait_until(|| async {
        sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM jobs WHERE locked_at IS NOT NULL"#)
            .fetch_one(&pool)
            .await
            .unwrap()
            == 1
    })
    .await;

    shutdown.cancel();
    handle.await.unwrap();
    assert_eq!(recv(&mut rx).await, json!({ "slow": true }));
    assert_eq!(
        job_rows(&pool).await,
        (0, 0),
        "in-flight job finished before exit"
    );
    Ok(())
}

async fn wait_until<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition met in time");
}
