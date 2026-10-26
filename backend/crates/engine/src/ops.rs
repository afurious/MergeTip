//! Operator actions, run from the worker binary (`mergetip-worker ops …`).

use mergetip_store::PgPool;

use crate::EngineError;

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error(transparent)]
    Engine(#[from] EngineError),
}

impl From<mergetip_store::StoreError> for OpsError {
    fn from(err: mergetip_store::StoreError) -> Self {
        Self::Engine(err.into())
    }
}

/// What a key rotation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RewrapReport {
    /// Values looked at.
    pub checked: usize,
    /// Values re-wrapped under the current master key.
    pub rewrapped: usize,
    /// Values that open with neither key (logged by ID; investigate before removing the old key).
    pub failed: usize,
}

/// Re-wraps every encrypted value whose data key is still under the previous master key, so
/// `MASTER_KEY_PREVIOUS` can be removed (runbook `key-rotation.md`). Only the small wrapped data
/// key changes; values are never decrypted to plaintext outside memory. Safe to run repeatedly
/// and while the service runs.
pub async fn rewrap(
    pool: &PgPool,
    keyring: &mergetip_crypto::Keyring,
    operator: &str,
) -> Result<RewrapReport, OpsError> {
    use mergetip_store::rewrap::{self as columns, Column};

    let mut report = RewrapReport::default();
    for column in Column::ALL {
        let mut after = uuid::Uuid::nil();
        loop {
            let page = columns::batch(pool, column, after, 500).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            after = *last;
            for (id, blob) in page {
                report.checked += 1;
                let aad = column.aad(id);
                // The header's 1-byte key ID can collide with a stray key's: prove a "current"
                // value actually opens before counting it as done.
                if keyring.is_current(&blob) {
                    if keyring.open(&blob, aad.as_bytes()).is_err() {
                        report.failed += 1;
                        tracing::error!(column = column.aad_prefix(), %id,
                            "value claims the current key but does not open");
                    }
                    continue;
                }
                match keyring.rewrap(&blob, aad.as_bytes()) {
                    Ok(fresh) => {
                        if columns::replace(pool, column, id, &blob, &fresh).await? {
                            report.rewrapped += 1;
                        }
                    }
                    Err(err) => {
                        report.failed += 1;
                        tracing::error!(column = column.aad_prefix(), %id, error = %err,
                            "value opens with neither master key");
                    }
                }
            }
        }
    }
    tracing::warn!(
        operator,
        checked = report.checked,
        rewrapped = report.rewrapped,
        failed = report.failed,
        "master key rewrap done"
    );
    Ok(report)
}
