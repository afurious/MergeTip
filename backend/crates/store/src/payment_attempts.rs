//! `payment_attempts`: each try at paying a tip's invoice, recorded before the wallet is asked.
//!
//! Partial unique indexes enforce at most one open (`pending`/`unknown`) and at most one
//! `succeeded` attempt per tip, so a double payment is refused by the database itself.

use std::{fmt, str::FromStr};

use mergetip_core::{Msats, TipId, UserId};
use sqlx::PgExecutor;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult, corrupt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStatus {
    /// Reserved and about to be (or being) sent to the wallet.
    Pending,
    Succeeded,
    Failed,
    /// Sent, but the outcome is unknown (timeout); reconciliation decides.
    Unknown,
}

impl AttemptStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for AttemptStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AttemptStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!("unknown attempt status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub tip_id: TipId,
    pub payment_hash: String,
    pub bolt11: String,
    pub amount: Msats,
    pub invoice_expires_at: OffsetDateTime,
    pub status: AttemptStatus,
    pub nwc_error_code: Option<String>,
    pub started_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
    /// The wallet connection that was asked to pay; `None` for attempts made before it was
    /// recorded.
    pub wallet_connection_id: Option<Uuid>,
}

struct AttemptRow {
    id: Uuid,
    tip_id: Uuid,
    payment_hash: String,
    bolt11: String,
    amount_msat: i64,
    invoice_expires_at: OffsetDateTime,
    status: String,
    nwc_error_code: Option<String>,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    wallet_connection_id: Option<Uuid>,
}

impl TryFrom<AttemptRow> for PaymentAttempt {
    type Error = StoreError;

    fn try_from(row: AttemptRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            tip_id: TipId::from_uuid(row.tip_id),
            payment_hash: row.payment_hash,
            bolt11: row.bolt11,
            amount: Msats::try_from(row.amount_msat).map_err(corrupt)?,
            invoice_expires_at: row.invoice_expires_at,
            status: row.status.parse().map_err(corrupt)?,
            nwc_error_code: row.nwc_error_code,
            started_at: row.started_at,
            finished_at: row.finished_at,
            wallet_connection_id: row.wallet_connection_id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewAttempt<'a> {
    pub tip_id: TipId,
    pub payment_hash: &'a str,
    pub bolt11: &'a str,
    pub amount: Msats,
    pub invoice_expires_at: OffsetDateTime,
    /// The connection about to be asked to pay (always set by the engine).
    pub wallet_connection_id: Option<Uuid>,
}

/// Records a `pending` attempt. Fails with [`StoreError::Conflict`] if the tip already has an
/// open attempt: a second payment must never start while the first may still be in flight.
pub async fn insert_pending(
    exec: impl PgExecutor<'_>,
    new: &NewAttempt<'_>,
) -> StoreResult<PaymentAttempt> {
    let amount = i64::try_from(new.amount).map_err(corrupt)?;
    let row = sqlx::query_as!(
        AttemptRow,
        r#"
        INSERT INTO payment_attempts (tip_id, payment_hash, bolt11, amount_msat,
                                      invoice_expires_at, status, wallet_connection_id)
        VALUES ($1, $2, $3, $4, $5, 'pending', $6)
        RETURNING id, tip_id, payment_hash, bolt11, amount_msat, invoice_expires_at, status,
                  nwc_error_code, started_at, finished_at, wallet_connection_id
        "#,
        new.tip_id.as_uuid(),
        new.payment_hash,
        new.bolt11,
        amount,
        new.invoice_expires_at,
        new.wallet_connection_id,
    )
    .fetch_one(exec)
    .await
    .map_err(unique_as_conflict)?;
    row.try_into()
}

/// Moves an attempt to `status`. `finished_at` is set for final statuses.
pub async fn set_status(
    exec: impl PgExecutor<'_>,
    id: Uuid,
    status: AttemptStatus,
    nwc_error_code: Option<&str>,
    now: OffsetDateTime,
) -> StoreResult<PaymentAttempt> {
    let finished = matches!(status, AttemptStatus::Succeeded | AttemptStatus::Failed);
    let row = sqlx::query_as!(
        AttemptRow,
        r#"
        UPDATE payment_attempts
        SET status = $2, nwc_error_code = COALESCE($3, nwc_error_code),
            finished_at = CASE WHEN $4 THEN $5 ELSE finished_at END
        WHERE id = $1
        RETURNING id, tip_id, payment_hash, bolt11, amount_msat, invoice_expires_at, status,
                  nwc_error_code, started_at, finished_at, wallet_connection_id
        "#,
        id,
        status.as_str(),
        nwc_error_code,
        finished,
        now,
    )
    .fetch_one(exec)
    .await
    .map_err(unique_as_conflict)?;
    row.try_into()
}

/// The tip's open attempt (`pending` or `unknown`), if any.
pub async fn open_for_tip(
    exec: impl PgExecutor<'_>,
    tip_id: TipId,
) -> StoreResult<Option<PaymentAttempt>> {
    let row = sqlx::query_as!(
        AttemptRow,
        r#"
        SELECT id, tip_id, payment_hash, bolt11, amount_msat, invoice_expires_at, status,
               nwc_error_code, started_at, finished_at, wallet_connection_id
        FROM payment_attempts
        WHERE tip_id = $1 AND status IN ('pending', 'unknown')
        "#,
        tip_id.as_uuid(),
    )
    .fetch_optional(exec)
    .await?;
    row.map(TryInto::try_into).transpose()
}

pub async fn get(exec: impl PgExecutor<'_>, id: Uuid) -> StoreResult<PaymentAttempt> {
    sqlx::query_as!(
        AttemptRow,
        r#"
        SELECT id, tip_id, payment_hash, bolt11, amount_msat, invoice_expires_at, status,
               nwc_error_code, started_at, finished_at, wallet_connection_id
        FROM payment_attempts WHERE id = $1
        "#,
        id,
    )
    .fetch_one(exec)
    .await?
    .try_into()
}

/// How many attempts the tip has had (any status).
pub async fn count_for_tip(exec: impl PgExecutor<'_>, tip_id: TipId) -> StoreResult<i64> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM payment_attempts WHERE tip_id = $1"#,
        tip_id.as_uuid(),
    )
    .fetch_one(exec)
    .await?;
    Ok(count)
}

/// Sats committed by a sender since `since`: attempts that succeeded or may still succeed.
/// The daily cap is checked against this, so in-flight payments count before they settle.
pub async fn reserved_since(
    exec: impl PgExecutor<'_>,
    sender: UserId,
    since: OffsetDateTime,
) -> StoreResult<Msats> {
    let total = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(sum(a.amount_msat), 0)::bigint AS "total!"
        FROM payment_attempts a JOIN tips t ON t.id = a.tip_id
        WHERE t.sender_id = $1 AND a.started_at >= $2
          AND a.status IN ('pending', 'unknown', 'succeeded')
        "#,
        sender.as_uuid(),
        since,
    )
    .fetch_one(exec)
    .await?;
    Msats::try_from(total).map_err(corrupt)
}

fn unique_as_conflict(err: sqlx::Error) -> StoreError {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => StoreError::Conflict,
        _ => err.into(),
    }
}
