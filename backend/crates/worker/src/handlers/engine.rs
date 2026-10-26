use std::sync::Arc;

use async_trait::async_trait;
use mergetip_engine::{
    Engine, EngineError, ReconcileOutcome,
    jobs::{
        JobPayload, NotifyPledge, PayTip, PostReply, ProcessEvent, ReconcilePayment, ResolveHeld,
        ResumeSenderTips, SendDigest, VerifyTip,
    },
};
use mergetip_store::jobs::Job;

use crate::{JobContext, JobError, JobHandler, JobOutcome};

fn decode<P: JobPayload>(job: &Job) -> Result<P, JobError> {
    P::decode(&job.payload)
        .ok_or_else(|| JobError::permanent(format!("malformed {} payload", P::KIND)))
}

/// Retry what can succeed later (at GitHub's reset time for rate limits); dead-letter the rest.
fn job_error(ctx: &JobContext, err: EngineError) -> JobError {
    if !err.is_retryable() {
        return JobError::permanent(err);
    }
    match err.retry_after(ctx.clock.now()) {
        Some(after) => JobError::retry_after(err, after),
        None => JobError::retryable(err),
    }
}

macro_rules! engine_handler {
    ($(#[$doc:meta])* $name:ident, $payload:ty, |$engine:ident, $p:ident| $call:expr) => {
        $(#[$doc])*
        pub struct $name(pub Arc<Engine>);

        #[async_trait]
        impl JobHandler for $name {
            fn kind(&self) -> &'static str {
                <$payload>::KIND
            }

            async fn handle(&self, ctx: &JobContext, job: &Job) -> Result<JobOutcome, JobError> {
                let $p: $payload = decode(job)?;
                let $engine = self.0.as_ref();
                $call.await.map_err(|err| job_error(ctx, err))?;
                Ok(JobOutcome::Done)
            }
        }
    };
}

engine_handler!(
    /// Routes a stored webhook delivery.
    ProcessEventHandler,
    ProcessEvent,
    |engine, payload| engine.process_event(&payload.delivery_id)
);

engine_handler!(
    /// Checks the receiver's contribution and decides the tip's next state.
    VerifyTipHandler,
    VerifyTip,
    |engine, payload| engine.verify_tip(payload.tip_id)
);

engine_handler!(
    /// Posts a reply or reaction in a GitHub thread.
    PostReplyHandler,
    PostReply,
    |engine, payload| engine.post_reply(&payload)
);

engine_handler!(
    /// Pays or cancels a pull request's on-merge holds once it closes.
    ResolveHeldHandler,
    ResolveHeld,
    |engine, payload| engine.resolve_held(&payload)
);

engine_handler!(
    /// Moves a sender's waiting tips on once they connect a wallet.
    ResumeSenderTipsHandler,
    ResumeSenderTips,
    |engine, payload| engine.resume_sender_tips(payload.user_id)
);

engine_handler!(
    /// Finds an email for a new pledge's receiver and queues their digest.
    NotifyPledgeHandler,
    NotifyPledge,
    |engine, payload| engine.notify_pledge(payload.tip_id)
);

engine_handler!(
    /// Emails a receiver their due pledge notices, at most once a day.
    SendDigestHandler,
    SendDigest,
    |engine, payload| engine.send_digest(payload.user_id)
);

/// Pays a tip. On the job's last attempt the engine fails the tip rather than asking for
/// another retry, so a tip can't be left in `paying` by a dead-lettered job.
pub struct PayTipHandler(pub Arc<Engine>);

#[async_trait]
impl JobHandler for PayTipHandler {
    fn kind(&self) -> &'static str {
        PayTip::KIND
    }

    async fn handle(&self, ctx: &JobContext, job: &Job) -> Result<JobOutcome, JobError> {
        let payload: PayTip = decode(job)?;
        self.0
            .pay_tip(payload.tip_id, job.is_last_attempt())
            .await
            .map_err(|err| job_error(ctx, err))?;
        Ok(JobOutcome::Done)
    }
}

/// Settles payments whose outcome was unknown; re-runs itself until the outcome is certain.
pub struct ReconcilePaymentHandler(pub Arc<Engine>);

#[async_trait]
impl JobHandler for ReconcilePaymentHandler {
    fn kind(&self) -> &'static str {
        ReconcilePayment::KIND
    }

    async fn handle(&self, ctx: &JobContext, job: &Job) -> Result<JobOutcome, JobError> {
        let payload: ReconcilePayment = decode(job)?;
        match self
            .0
            .reconcile_payment(payload.tip_id, payload.attempt_id)
            .await
            .map_err(|err| job_error(ctx, err))?
        {
            ReconcileOutcome::Done => Ok(JobOutcome::Done),
            ReconcileOutcome::CheckAgainIn(delay) => Ok(JobOutcome::Reschedule(delay)),
        }
    }
}
