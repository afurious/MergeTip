//! The job runner: a fixed number of slots, each claiming and running one job at a time.
//!
//! Slots wake on Postgres `NOTIFY` (enqueue sends one) and fall back to polling, so a missed
//! notification costs at most one poll interval. On shutdown, slots stop claiming, in-flight jobs
//! get `drain_timeout` to finish, and anything still running after that is abandoned; its lease
//! expires and another worker picks it up.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use mergetip_core::Clock;
use mergetip_crypto::{RandomError, random_bytes};
use mergetip_store::{
    PgPool, StoreError,
    jobs::{self, Job},
};
use sqlx::postgres::PgListener;
use tokio::{
    sync::Notify,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Jobs run concurrently by this process.
    pub concurrency: usize,
    /// How long a claim is exclusive without renewal. Running jobs renew it every `lease / 3`,
    /// so only a job whose worker stopped renewing (crashed, partitioned) is reclaimed.
    pub lease: Duration,
    pub poll_interval: Duration,
    pub drain_timeout: Duration,
    pub retry_base: Duration,
    pub retry_cap: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            concurrency: 8,
            lease: Duration::from_secs(300),
            poll_interval: Duration::from_secs(1),
            drain_timeout: Duration::from_secs(30),
            retry_base: Duration::from_secs(10),
            retry_cap: Duration::from_secs(600),
        }
    }
}

/// Shared dependencies handed to every handler.
#[derive(Clone)]
pub struct JobContext {
    pub pool: PgPool,
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for JobContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobContext").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    Done,
    /// Run this same job again after the given delay (recurring jobs).
    Reschedule(Duration),
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// Transient: retried with backoff, or after `after` when the cause dictates (rate limits).
    #[error("{reason}")]
    Retryable {
        reason: String,
        after: Option<Duration>,
    },
    /// Retrying cannot help; the job is dead-lettered.
    #[error("{reason}")]
    Permanent { reason: String },
}

impl JobError {
    pub fn retryable(reason: impl fmt::Display) -> Self {
        Self::Retryable {
            reason: reason.to_string(),
            after: None,
        }
    }

    pub fn retry_after(reason: impl fmt::Display, after: Duration) -> Self {
        Self::Retryable {
            reason: reason.to_string(),
            after: Some(after),
        }
    }

    pub fn permanent(reason: impl fmt::Display) -> Self {
        Self::Permanent {
            reason: reason.to_string(),
        }
    }
}

/// Database errors inside a handler are assumed transient (failover, pool exhaustion).
impl From<StoreError> for JobError {
    fn from(err: StoreError) -> Self {
        Self::retryable(err)
    }
}

#[async_trait]
pub trait JobHandler: Send + Sync {
    /// The `jobs.kind` value this handler runs.
    fn kind(&self) -> &'static str;

    async fn handle(&self, ctx: &JobContext, job: &Job) -> Result<JobOutcome, JobError>;
}

pub struct Runner {
    ctx: JobContext,
    config: RunnerConfig,
    handlers: HashMap<&'static str, Arc<dyn JobHandler>>,
    worker_id: String,
}

impl Runner {
    /// Fails if the OS random number generator does. Lease tokens are the only thing that
    /// stops one worker from finishing another's job, so they are never built from a fallback.
    pub fn new(
        pool: PgPool,
        clock: Arc<dyn Clock>,
        config: RunnerConfig,
    ) -> Result<Self, RandomError> {
        let suffix = hex::encode(random_bytes::<4>()?);
        Ok(Self {
            ctx: JobContext { pool, clock },
            config,
            handlers: HashMap::new(),
            worker_id: format!("worker-{}-{suffix}", std::process::id()),
        })
    }

    /// Adds a handler. Registering two handlers for one kind is a wiring bug, so it panics.
    #[must_use]
    pub fn register(mut self, handler: impl JobHandler + 'static) -> Self {
        let kind = handler.kind();
        let previous = self.handlers.insert(kind, Arc::new(handler));
        assert!(
            previous.is_none(),
            "two handlers registered for job kind {kind:?}"
        );
        self
    }

    /// Runs until `shutdown` is cancelled, then drains in-flight jobs.
    pub async fn run(self, shutdown: CancellationToken) {
        let runner = Arc::new(self);
        let wake = Arc::new(Notify::new());
        let kinds: Arc<[String]> = runner.handlers.keys().map(|k| (*k).to_owned()).collect();
        tracing::info!(
            worker_id = %runner.worker_id,
            concurrency = runner.config.concurrency,
            kinds = ?kinds,
            "job runner started"
        );

        let listener = tokio::spawn(listen(
            runner.ctx.pool.clone(),
            wake.clone(),
            shutdown.clone(),
        ));

        let mut slots = JoinSet::new();
        for slot in 0..runner.config.concurrency {
            slots.spawn(
                runner
                    .clone()
                    .slot(kinds.clone(), wake.clone(), shutdown.clone())
                    .instrument(tracing::info_span!("slot", slot)),
            );
        }

        shutdown.cancelled().await;
        tracing::info!("draining in-flight jobs");
        let drained = tokio::time::timeout(runner.config.drain_timeout, async {
            while slots.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                remaining = slots.len(),
                "drain timed out; abandoned jobs will be reclaimed after their lease expires"
            );
            slots.abort_all();
        }
        listener.abort();
        tracing::info!("job runner stopped");
    }

    async fn slot(
        self: Arc<Self>,
        kinds: Arc<[String]>,
        wake: Arc<Notify>,
        shutdown: CancellationToken,
    ) {
        while !shutdown.is_cancelled() {
            // Register for wake-ups before claiming, so an enqueue that lands between an empty
            // claim and the wait is not missed.
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let lease_token = match random_bytes::<12>() {
                Ok(nonce) => format!("{}/{}", self.worker_id, hex::encode(nonce)),
                Err(err) => {
                    tracing::error!(error = %err, "cannot create a lease token; not claiming");
                    tokio::select! {
                        () = tokio::time::sleep(self.config.poll_interval) => {}
                        () = shutdown.cancelled() => {}
                    }
                    continue;
                }
            };
            match jobs::claim(&self.ctx.pool, &kinds, &lease_token, self.config.lease).await {
                Ok(Some(job)) => self.execute(job).await,
                Ok(None) => {
                    tokio::select! {
                        () = &mut notified => {}
                        () = tokio::time::sleep(self.config.poll_interval) => {}
                        () = shutdown.cancelled() => {}
                    }
                }
                Err(err) => {
                    tracing::error!(error = %err, "claiming a job failed");
                    tokio::select! {
                        () = tokio::time::sleep(self.config.poll_interval) => {}
                        () = shutdown.cancelled() => {}
                    }
                }
            }
        }
    }

    async fn execute(&self, job: Job) {
        let span = tracing::info_span!(
            "job",
            job_id = job.id,
            kind = %job.kind,
            attempt = job.attempts
        );
        async {
            if job.is_exhausted() {
                tracing::error!("job was abandoned on its final attempt; dead-lettering");
                self.finish(jobs::fail_permanently(
                    &self.ctx.pool,
                    &job,
                    "lease expired during the final attempt",
                ))
                .await;
                return;
            }

            let Some(handler) = self.handlers.get(job.kind.as_str()).cloned() else {
                // Unreachable while claims are filtered by registered kinds; fail loudly if not.
                self.finish(jobs::fail_permanently(
                    &self.ctx.pool,
                    &job,
                    "no handler registered",
                ))
                .await;
                return;
            };

            // A separate task, so a panicking handler fails its job instead of the slot.
            let ctx = self.ctx.clone();
            let task_job = job.clone();
            let mut handler_task = AbortOnDrop(tokio::spawn(
                async move { handler.handle(&ctx, &task_job).await }.in_current_span(),
            ));
            let Some(result) = self.supervise(&job, &mut handler_task.0).await else {
                return;
            };

            match result {
                Ok(JobOutcome::Done) => {
                    tracing::debug!("job done");
                    self.finish(jobs::complete(&self.ctx.pool, &job)).await;
                }
                Ok(JobOutcome::Reschedule(delay)) => {
                    self.finish(jobs::reschedule(&self.ctx.pool, &job, delay))
                        .await;
                }
                Err(JobError::Retryable { reason, after }) if !job.is_last_attempt() => {
                    let delay = after.unwrap_or_else(|| self.backoff(job.attempts));
                    tracing::warn!(error = %reason, retry_in = ?delay, "job failed; will retry");
                    self.finish(jobs::retry(&self.ctx.pool, &job, &reason, delay))
                        .await;
                }
                Err(JobError::Retryable { reason, .. } | JobError::Permanent { reason }) => {
                    tracing::error!(error = %reason, "job failed permanently");
                    self.finish(jobs::fail_permanently(&self.ctx.pool, &job, &reason))
                        .await;
                }
            }
        }
        .instrument(span)
        .await;
    }

    /// Waits for the handler while renewing the job's lease, so a run longer than `lease` is
    /// not mistaken for a dead worker and run a second time elsewhere.
    ///
    /// Returns `None` if the lease was lost anyway (e.g. the database was unreachable for longer
    /// than the lease). The handler is then aborted and nothing is recorded: the job belongs to
    /// whichever worker reclaimed it.
    async fn supervise(
        &self,
        job: &Job,
        handler_task: &mut JoinHandle<Result<JobOutcome, JobError>>,
    ) -> Option<Result<JobOutcome, JobError>> {
        let period = (self.config.lease / 3).max(Duration::from_millis(10));
        let mut renew = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                joined = &mut *handler_task => {
                    return Some(joined.unwrap_or_else(|join_err| {
                        Err(JobError::retryable(format!("handler panicked: {join_err}")))
                    }));
                }
                _ = renew.tick() => match jobs::heartbeat(&self.ctx.pool, job).await {
                    Ok(()) => {}
                    Err(StoreError::Conflict) => {
                        tracing::error!("lease lost while the job was running; abandoning it");
                        handler_task.abort();
                        return None;
                    }
                    // Transient: try again next tick; the lease has slack for a missed renewal.
                    Err(err) => tracing::warn!(error = %err, "lease renewal failed"),
                },
            }
        }
    }

    fn backoff(&self, attempt: i32) -> Duration {
        // Jitter only spreads retries out; the midpoint is a safe value if the RNG fails.
        let jitter = f64::from(getrandom::u32().unwrap_or(u32::MAX / 2)) / f64::from(u32::MAX);
        jobs::backoff(
            attempt,
            self.config.retry_base,
            self.config.retry_cap,
            jitter,
        )
    }

    async fn finish(&self, update: impl Future<Output = Result<(), StoreError>>) {
        match update.await {
            Ok(()) => {}
            Err(StoreError::Conflict) => {
                tracing::warn!("lease was lost before the job finished; another worker owns it");
            }
            Err(err) => tracing::error!(error = %err, "recording the job result failed"),
        }
    }
}

/// Owns a handler task and aborts it when dropped.
///
/// When the drain timeout aborts a slot, the slot's future is dropped mid-job. Without this guard
/// the spawned handler would keep running detached, possibly finishing its side effect while its
/// result is never recorded, and the job would then run again once its lease expired.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Wakes one idle slot per enqueued job. Waking every slot would turn each enqueue into
/// `concurrency` claim queries, all but one coming back empty. If no slot is idle, `notify_one`
/// stores a permit, so the next slot to finish claims straight away. `PgListener` reconnects on
/// its own; errors are logged and polling covers the gap.
async fn listen(pool: PgPool, wake: Arc<Notify>, shutdown: CancellationToken) {
    let mut listener = loop {
        match PgListener::connect_with(&pool).await {
            Ok(mut listener) => match listener.listen(jobs::NOTIFY_CHANNEL).await {
                Ok(()) => break listener,
                Err(err) => tracing::warn!(error = %err, "LISTEN failed; polling only"),
            },
            Err(err) => tracing::warn!(error = %err, "listener connect failed; polling only"),
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
            () = shutdown.cancelled() => return,
        }
    };
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            message = listener.recv() => match message {
                Ok(_) => wake.notify_one(),
                Err(err) => {
                    tracing::warn!(error = %err, "job notification stream failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            },
        }
    }
}
