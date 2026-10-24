//! Typed job payloads. Each payload type knows its `jobs.kind`, so producers and handlers cannot
//! disagree on a kind string or payload shape.

use mergetip_core::TipId;
use mergetip_store::{
    StoreResult,
    jobs::{self, Enqueued, NewJob},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::PgConnection;

pub trait JobPayload: Serialize + DeserializeOwned {
    const KIND: &'static str;

    fn job(&self) -> NewJob {
        NewJob::new(
            Self::KIND,
            serde_json::to_value(self).expect("job payloads serialise to JSON"),
        )
    }

    /// Decodes a claimed job's payload. `None` means the payload is malformed: retrying cannot
    /// fix it, so the handler should fail the job permanently.
    fn decode(payload: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(payload.clone()).ok()
    }
}

/// Enqueues `payload` inside the caller's transaction.
pub async fn enqueue<P: JobPayload>(conn: &mut PgConnection, payload: &P) -> StoreResult<Enqueued> {
    jobs::enqueue(conn, &payload.job()).await
}

/// Process one stored GitHub webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEvent {
    pub delivery_id: String,
}

impl JobPayload for ProcessEvent {
    const KIND: &'static str = "process_event";
}

/// Check the receiver's contribution for a tip in `verifying`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyTip {
    pub tip_id: TipId,
}

impl JobPayload for VerifyTip {
    const KIND: &'static str = "verify_tip";
}

/// Pay a tip in `paying`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayTip {
    pub tip_id: TipId,
}

impl JobPayload for PayTip {
    const KIND: &'static str = "pay_tip";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_round_trip_through_jobs() {
        let payload = VerifyTip {
            tip_id: TipId::new_random(),
        };
        let job = payload.job();
        assert_eq!(job.kind, "verify_tip");
        assert_eq!(VerifyTip::decode(&job.payload), Some(payload));
        assert_eq!(PayTip::decode(&serde_json::json!({ "nope": 1 })), None);
    }
}
