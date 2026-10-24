use std::sync::Arc;

use async_trait::async_trait;
use mergetip_engine::{
    Engine, EngineError,
    jobs::{JobPayload, PayTip, PostReply, ProcessEvent, VerifyTip},
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
    /// Staging only: settles tips with a fake proof. Registered only when `STUB_PAYER=true`, so
    /// without it `pay_tip` jobs wait for the real payer.
    PayTipStubHandler,
    PayTip,
    |engine, payload| engine.pay_tip_stub(payload.tip_id)
);
