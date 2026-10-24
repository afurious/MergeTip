use async_trait::async_trait;
use mergetip_store::jobs::Job;

use crate::{JobContext, JobError, JobHandler, JobOutcome};

/// Does nothing. Lets operators check end to end that workers are claiming jobs.
#[derive(Debug, Default)]
pub struct NoopHandler;

impl NoopHandler {
    pub const KIND: &'static str = "noop";
}

#[async_trait]
impl JobHandler for NoopHandler {
    fn kind(&self) -> &'static str {
        Self::KIND
    }

    async fn handle(&self, _ctx: &JobContext, job: &Job) -> Result<JobOutcome, JobError> {
        tracing::info!(job_id = job.id, payload = %job.payload, "noop job ran");
        Ok(JobOutcome::Done)
    }
}
