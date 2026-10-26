//! `tips` and `tip_events`.
//!
//! [`transition`] is the only code that writes `tips.state`. It validates the change against
//! `core::transition`, applies it as a compare-and-set, bumps `version` and appends the audit row,
//! all in one transaction.

use mergetip_core::{
    GithubInstallationId, InstallationId, Sats, ThreadRef, TipId, TipKind, TipState, UserId,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult, corrupt};

/// `hex(sha256("{installation}:{comment}:{index}"))`. Derived from the comment rather than the
/// delivery, because GitHub may redeliver one comment under several delivery IDs.
pub fn idempotency_key(
    github_installation_id: GithubInstallationId,
    comment_id: i64,
    command_index: u16,
) -> String {
    let input = format!("{github_installation_id}:{comment_id}:{command_index}");
    hex::encode(Sha256::digest(input.as_bytes()))
}

#[derive(Debug, Clone)]
pub struct NewTip {
    pub idempotency_key: String,
    pub installation_id: InstallationId,
    pub sender_id: UserId,
    pub receiver_login: String,
    pub thread: ThreadRef,
    pub comment_id: i64,
    pub command_index: u16,
    pub kind: TipKind,
    pub amount: Sats,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tip {
    pub id: TipId,
    pub idempotency_key: String,
    pub installation_id: InstallationId,
    pub sender_id: UserId,
    pub receiver_id: Option<UserId>,
    pub receiver_login: String,
    pub thread: ThreadRef,
    pub comment_id: i64,
    pub command_index: u16,
    pub kind: TipKind,
    pub amount: Sats,
    pub note: Option<String>,
    pub state: TipState,
    /// Bumped by every transition; lets readers detect that a tip moved under them.
    pub version: i32,
    pub verified_by: Option<Value>,
    pub payment_hash: Option<String>,
    pub preimage: Option<String>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub confirm_expires_at: Option<OffsetDateTime>,
    pub confirmed_at: Option<OffsetDateTime>,
    pub expires_at: Option<OffsetDateTime>,
    pub email_status: Option<String>,
    pub last_notified_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One row of the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct TipEvent {
    pub from_state: Option<TipState>,
    pub to_state: TipState,
    pub detail: Value,
    pub at: OffsetDateTime,
}

/// Mirrors the table; converted to [`Tip`] so the rest of the code sees domain types only.
struct TipRow {
    id: Uuid,
    idempotency_key: String,
    installation_id: Uuid,
    sender_id: Uuid,
    receiver_id: Option<Uuid>,
    receiver_login: String,
    repo: String,
    thread_number: i32,
    is_pr: bool,
    comment_id: i64,
    command_index: i16,
    kind: String,
    amount_sats: i64,
    note: Option<String>,
    state: String,
    version: i32,
    verified_by: Option<Value>,
    payment_hash: Option<String>,
    preimage: Option<String>,
    error_code: Option<String>,
    error_detail: Option<String>,
    // Selected by `*` but deliberately not exposed: claim tokens are looked up by hash only.
    #[allow(dead_code)]
    claim_token_hash: Option<String>,
    // Likewise: read only through [`claim_token_ciphertext`], so `Tip` never carries a secret.
    #[allow(dead_code)]
    claim_token_ciphertext: Option<Vec<u8>>,
    confirm_expires_at: Option<OffsetDateTime>,
    confirmed_at: Option<OffsetDateTime>,
    expires_at: Option<OffsetDateTime>,
    email_status: Option<String>,
    last_notified_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<TipRow> for Tip {
    type Error = StoreError;

    fn try_from(row: TipRow) -> Result<Self, Self::Error> {
        let (owner, repo) = row
            .repo
            .split_once('/')
            .ok_or_else(|| corrupt(format!("repo {:?} is not owner/name", row.repo)))?;
        Ok(Self {
            id: TipId::from_uuid(row.id),
            idempotency_key: row.idempotency_key,
            installation_id: InstallationId::from_uuid(row.installation_id),
            sender_id: UserId::from_uuid(row.sender_id),
            receiver_id: row.receiver_id.map(UserId::from_uuid),
            receiver_login: row.receiver_login,
            thread: ThreadRef {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                number: u32::try_from(row.thread_number).map_err(corrupt)?,
                is_pr: row.is_pr,
            },
            comment_id: row.comment_id,
            command_index: u16::try_from(row.command_index).map_err(corrupt)?,
            kind: row.kind.parse().map_err(corrupt)?,
            amount: Sats::try_from(row.amount_sats).map_err(corrupt)?,
            note: row.note,
            state: row.state.parse().map_err(corrupt)?,
            version: row.version,
            verified_by: row.verified_by,
            payment_hash: row.payment_hash,
            preimage: row.preimage,
            error_code: row.error_code,
            error_detail: row.error_detail,
            confirm_expires_at: row.confirm_expires_at,
            confirmed_at: row.confirmed_at,
            expires_at: row.expires_at,
            email_status: row.email_status,
            last_notified_at: row.last_notified_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// Inserts a tip in state `received` together with its first audit row.
///
/// Returns `None` when a tip with the same idempotency key already exists: a replayed or
/// redelivered webhook then does nothing.
pub async fn insert_idempotent(conn: &mut PgConnection, new: &NewTip) -> StoreResult<Option<Tip>> {
    let thread_number = i32::try_from(new.thread.number).map_err(corrupt)?;
    let command_index = i16::try_from(new.command_index).map_err(corrupt)?;
    let amount_sats = i64::try_from(new.amount).map_err(corrupt)?;

    let mut tx = conn.begin().await?;
    let row = sqlx::query_as!(
        TipRow,
        r#"
        INSERT INTO tips (idempotency_key, installation_id, sender_id, receiver_login, repo,
                          thread_number, is_pr, comment_id, command_index, kind, amount_sats,
                          note, state)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (idempotency_key) DO NOTHING
        RETURNING *
        "#,
        new.idempotency_key,
        new.installation_id.as_uuid(),
        new.sender_id.as_uuid(),
        new.receiver_login,
        new.thread.full_name(),
        thread_number,
        new.thread.is_pr,
        new.comment_id,
        command_index,
        new.kind.as_str(),
        amount_sats,
        new.note,
        TipState::Received.as_str(),
    )
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    sqlx::query!(
        "INSERT INTO tip_events (tip_id, from_state, to_state) VALUES ($1, NULL, $2)",
        row.id,
        TipState::Received.as_str(),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    row.try_into().map(Some)
}

pub async fn get(exec: impl PgExecutor<'_>, id: TipId) -> StoreResult<Tip> {
    sqlx::query_as!(TipRow, "SELECT * FROM tips WHERE id = $1", id.as_uuid())
        .fetch_one(exec)
        .await?
        .try_into()
}

/// Reads the tip and holds its row lock until the caller's transaction ends.
pub async fn lock_for_update(conn: &mut PgConnection, id: TipId) -> StoreResult<Tip> {
    sqlx::query_as!(
        TipRow,
        "SELECT * FROM tips WHERE id = $1 FOR UPDATE",
        id.as_uuid()
    )
    .fetch_one(conn)
    .await?
    .try_into()
}

/// Columns written together with a state change. `None` leaves a column as it is, so a
/// transition can only ever add facts (who the receiver is, why it was rejected, the payment
/// proof), never erase them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TipPatch {
    pub receiver_id: Option<UserId>,
    pub verified_by: Option<Value>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub confirm_expires_at: Option<OffsetDateTime>,
    pub payment_hash: Option<String>,
    pub preimage: Option<String>,
    /// SHA-256 of a new claim token, and the token itself sealed with AAD
    /// `tips.claim_token:{id}`. Cleared only by [`clear_claim_token`].
    pub claim_token_hash: Option<String>,
    pub claim_token_ciphertext: Option<Vec<u8>>,
    /// When the sender confirmed a high-value tip in the web app.
    pub confirmed_at: Option<OffsetDateTime>,
}

impl TipPatch {
    /// The patch for a rejection or failure: a stable `snake_case` code plus optional detail.
    pub fn error(code: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            error_code: Some(code.into()),
            error_detail: detail,
            ..Self::default()
        }
    }
}

/// Moves a tip from `from` to `to`, applies `patch` in the same statement, and records why in
/// `tip_events.detail`.
///
/// Fails with [`StoreError::IllegalTransition`] before touching the database if the state
/// machine forbids the change, and with [`StoreError::Conflict`] if the tip is no longer in
/// `from` (another worker moved it first). Pass the caller's transaction when the transition
/// must commit together with other writes, such as enqueuing the next job.
pub async fn transition(
    conn: &mut PgConnection,
    id: TipId,
    from: TipState,
    to: TipState,
    patch: &TipPatch,
    detail: Value,
) -> StoreResult<Tip> {
    mergetip_core::transition(from, to)?;

    let mut tx = conn.begin().await?;
    let row = sqlx::query_as!(
        TipRow,
        r#"
        UPDATE tips SET
            state = $3,
            version = version + 1,
            receiver_id = COALESCE($4, receiver_id),
            verified_by = COALESCE($5, verified_by),
            error_code = COALESCE($6, error_code),
            error_detail = COALESCE($7, error_detail),
            expires_at = COALESCE($8, expires_at),
            confirm_expires_at = COALESCE($9, confirm_expires_at),
            payment_hash = COALESCE($10, payment_hash),
            preimage = COALESCE($11, preimage),
            claim_token_hash = COALESCE($12, claim_token_hash),
            claim_token_ciphertext = COALESCE($13, claim_token_ciphertext),
            confirmed_at = COALESCE($14, confirmed_at)
        WHERE id = $1 AND state = $2
        RETURNING *
        "#,
        id.as_uuid(),
        from.as_str(),
        to.as_str(),
        patch.receiver_id.map(|id| id.as_uuid()),
        patch.verified_by,
        patch.error_code,
        patch.error_detail,
        patch.expires_at,
        patch.confirm_expires_at,
        patch.payment_hash,
        patch.preimage,
        patch.claim_token_hash,
        patch.claim_token_ciphertext,
        patch.confirmed_at,
    )
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = row else {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM tips WHERE id = $1) AS "exists!""#,
            id.as_uuid()
        )
        .fetch_one(&mut *tx)
        .await?;
        return Err(if exists {
            StoreError::Conflict
        } else {
            StoreError::NotFound
        });
    };

    sqlx::query!(
        "INSERT INTO tip_events (tip_id, from_state, to_state, detail) VALUES ($1, $2, $3, $4)",
        id.as_uuid(),
        from.as_str(),
        to.as_str(),
        detail,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::debug!(tip_id = %id, %from, %to, "tip transitioned");
    if to.is_final() {
        metrics::counter!("tips_final_total", "state" => to.as_str()).increment(1);
    }
    if to == TipState::Paid {
        let seconds = (OffsetDateTime::now_utc() - row.created_at).as_seconds_f64();
        metrics::histogram!("tip_e2e_seconds").record(seconds.max(0.0));
    }
    row.try_into()
}

/// Tips the sender created since `since`, other than `exclude`, rejected ones included. Every
/// tip can make the bot post in a thread, so every tip counts toward the hourly limit; otherwise
/// a stream of rejected tips would be unlimited bot output.
pub async fn count_by_sender_since(
    exec: impl PgExecutor<'_>,
    sender: UserId,
    since: OffsetDateTime,
    exclude: TipId,
) -> StoreResult<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!" FROM tips
        WHERE sender_id = $1 AND created_at >= $2 AND id <> $3
        "#,
        sender.as_uuid(),
        since,
        exclude.as_uuid(),
    )
    .fetch_one(exec)
    .await?;
    Ok(count)
}

/// Live tips from `sender` to `receiver_login` in one thread, other than `exclude`.
pub async fn count_for_pair_in_thread(
    exec: impl PgExecutor<'_>,
    sender: UserId,
    receiver_login: &str,
    thread: &ThreadRef,
    exclude: TipId,
) -> StoreResult<i64> {
    let thread_number = i32::try_from(thread.number).map_err(corrupt)?;
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!" FROM tips
        WHERE sender_id = $1 AND lower(receiver_login) = lower($2)
          AND repo = $3 AND thread_number = $4 AND id <> $5 AND state <> 'rejected'
        "#,
        sender.as_uuid(),
        receiver_login,
        thread.full_name(),
        thread_number,
        exclude.as_uuid(),
    )
    .fetch_one(exec)
    .await?;
    Ok(count)
}

/// Tips in `state` on one thread, oldest first (e.g. every `held` tip when the PR closes).
pub async fn list_in_thread(
    exec: impl PgExecutor<'_>,
    repo: &str,
    thread_number: u32,
    state: TipState,
) -> StoreResult<Vec<Tip>> {
    let thread_number = i32::try_from(thread_number).map_err(corrupt)?;
    sqlx::query_as!(
        TipRow,
        r#"
        SELECT * FROM tips
        WHERE repo = $1 AND thread_number = $2 AND state = $3
        ORDER BY created_at, command_index
        "#,
        repo,
        thread_number,
        state.as_str(),
    )
    .fetch_all(exec)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// The sender's tips still waiting for them to connect a wallet, deadline not yet passed.
pub async fn list_awaiting_sender(
    exec: impl PgExecutor<'_>,
    sender: UserId,
    now: OffsetDateTime,
) -> StoreResult<Vec<Tip>> {
    sqlx::query_as!(
        TipRow,
        r#"
        SELECT * FROM tips
        WHERE sender_id = $1 AND state = 'awaiting_sender' AND expires_at > $2
        ORDER BY created_at, command_index
        "#,
        sender.as_uuid(),
        now,
    )
    .fetch_all(exec)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Up to `limit` tips in `state` whose deadline is before `now`, locked until the caller's
/// transaction ends. `SKIP LOCKED` lets several expiry workers split the work without waiting on
/// or double-handling each other's rows. The deadline is `confirm_expires_at` for
/// `awaiting_confirmation` and `expires_at` otherwise.
pub async fn lock_expired(
    conn: &mut PgConnection,
    state: TipState,
    now: OffsetDateTime,
    limit: i64,
) -> StoreResult<Vec<Tip>> {
    sqlx::query_as!(
        TipRow,
        r#"
        SELECT * FROM tips
        WHERE state = $1
          AND (CASE WHEN $1 = 'awaiting_confirmation' THEN confirm_expires_at
                    ELSE expires_at END) < $2
        ORDER BY id
        LIMIT $3
        FOR UPDATE SKIP LOCKED
        "#,
        state.as_str(),
        now,
        limit,
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// The pledge whose claim token hashes to `token_hash`, if any. Claimed and expired-and-claimed
/// tips have no token, so a used link finds nothing.
pub async fn get_by_claim_hash(
    exec: impl PgExecutor<'_>,
    token_hash: &str,
) -> StoreResult<Option<Tip>> {
    sqlx::query_as!(
        TipRow,
        "SELECT * FROM tips WHERE claim_token_hash = $1",
        token_hash
    )
    .fetch_optional(exec)
    .await?
    .map(TryInto::try_into)
    .transpose()
}

/// [`get_by_claim_hash`], holding the row lock until the caller's transaction ends.
pub async fn lock_by_claim_hash(
    conn: &mut PgConnection,
    token_hash: &str,
) -> StoreResult<Option<Tip>> {
    sqlx::query_as!(
        TipRow,
        "SELECT * FROM tips WHERE claim_token_hash = $1 FOR UPDATE",
        token_hash
    )
    .fetch_optional(conn)
    .await?
    .map(TryInto::try_into)
    .transpose()
}

/// The sealed claim token, for rendering the claim link into replies and emails.
pub async fn claim_token_ciphertext(
    exec: impl PgExecutor<'_>,
    id: TipId,
) -> StoreResult<Option<Vec<u8>>> {
    let blob = sqlx::query_scalar!(
        "SELECT claim_token_ciphertext FROM tips WHERE id = $1",
        id.as_uuid()
    )
    .fetch_one(exec)
    .await?;
    Ok(blob)
}

/// Makes the claim link single-use: forgets both the hash and the sealed token.
pub async fn clear_claim_token(exec: impl PgExecutor<'_>, id: TipId) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE tips SET claim_token_hash = NULL, claim_token_ciphertext = NULL WHERE id = $1",
        id.as_uuid()
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// The receiver's pledges an email could mention: every open one, plus expired ones whose final
/// notice hasn't gone out. Oldest first.
pub async fn notice_candidates(
    exec: impl PgExecutor<'_>,
    receiver: UserId,
) -> StoreResult<Vec<Tip>> {
    sqlx::query_as!(
        TipRow,
        r#"
        SELECT * FROM tips
        WHERE receiver_id = $1
          AND (state = 'pledged'
               OR (state = 'expired' AND expires_at IS NOT NULL
                   AND (last_notified_at IS NULL OR last_notified_at < expires_at)))
        ORDER BY created_at, command_index
        "#,
        receiver.as_uuid(),
    )
    .fetch_all(exec)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Receivers with a pledge whose 7-day reminder is due (`now ≥ expires_at − reminder`) and not
/// yet sent: no notice since the reminder point.
pub async fn receivers_due_a_reminder(
    exec: impl PgExecutor<'_>,
    now: OffsetDateTime,
    reminder: time::Duration,
) -> StoreResult<Vec<UserId>> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT receiver_id AS "receiver_id!" FROM tips
        WHERE state = 'pledged' AND receiver_id IS NOT NULL AND expires_at IS NOT NULL
          AND expires_at - $2::interval <= $1
          AND (last_notified_at IS NULL OR last_notified_at < expires_at - $2::interval)
        "#,
        now,
        reminder as _,
    )
    .fetch_all(exec)
    .await?;
    Ok(ids.into_iter().map(UserId::from_uuid).collect())
}

/// Receivers with a pledge that found an address (`email_status = pending`) but was never
/// covered by a notice, and has waited since before `cutoff`.
pub async fn receivers_with_unsent_notices(
    exec: impl PgExecutor<'_>,
    cutoff: OffsetDateTime,
) -> StoreResult<Vec<UserId>> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT receiver_id AS "receiver_id!" FROM tips
        WHERE state = 'pledged' AND receiver_id IS NOT NULL AND last_notified_at IS NULL
          AND email_status = 'pending' AND updated_at < $1
        "#,
        cutoff,
    )
    .fetch_all(exec)
    .await?;
    Ok(ids.into_iter().map(UserId::from_uuid).collect())
}

/// Records that these tips were covered by a notice at `at` (sent or deliberately skipped), and
/// sets their email status.
pub async fn mark_notified(
    exec: impl PgExecutor<'_>,
    ids: &[TipId],
    at: OffsetDateTime,
    email_status: &str,
) -> StoreResult<()> {
    let ids: Vec<Uuid> = ids.iter().map(TipId::as_uuid).collect();
    sqlx::query!(
        "UPDATE tips SET last_notified_at = $2, email_status = $3 WHERE id = ANY($1)",
        &ids,
        at,
        email_status,
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// Sets the email status of a tip no notice has covered yet (e.g. `pending` once an address is
/// found). Returns `false`, changing nothing, if a digest already covered it.
pub async fn set_email_status_before_notice(
    exec: impl PgExecutor<'_>,
    id: TipId,
    email_status: &str,
) -> StoreResult<bool> {
    let result = sqlx::query!(
        "UPDATE tips SET email_status = $2 WHERE id = $1 AND last_notified_at IS NULL",
        id.as_uuid(),
        email_status,
    )
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Applies a delivery event from the email provider. Only moves forward
/// (`pending < sent < delivered < opened`); `bounced` and `failed` are final, so late or
/// replayed webhooks can't undo a bounce. Returns how many tips changed.
pub async fn advance_email_status(
    exec: impl PgExecutor<'_>,
    ids: &[Uuid],
    status: &str,
) -> StoreResult<u64> {
    let result = sqlx::query!(
        r#"
        UPDATE tips SET email_status = $2
        WHERE id = ANY($1) AND (
            email_status IS NULL OR email_status IN ('none', 'pending')
            OR ($2 = 'delivered' AND email_status = 'sent')
            OR ($2 = 'opened' AND email_status IN ('sent', 'delivered'))
            OR ($2 IN ('bounced', 'failed') AND email_status NOT IN ('bounced', 'failed'))
        )
        "#,
        ids,
        status,
    )
    .execute(exec)
    .await?;
    Ok(result.rows_affected())
}

/// Which of a user's tips to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Sent,
    Received,
    All,
}

impl Direction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Received => "received",
            Self::All => "all",
        }
    }
}

/// One page of the user's tips, newest first, strictly after the `(created_at, id)` keyset
/// cursor when given. Keyset (not offset) pagination keeps pages stable while new tips arrive:
/// a row is never repeated or skipped because others were inserted ahead of it.
pub async fn list_for_user(
    exec: impl PgExecutor<'_>,
    user: UserId,
    direction: Direction,
    state: Option<TipState>,
    after: Option<(OffsetDateTime, TipId)>,
    limit: i64,
) -> StoreResult<Vec<Tip>> {
    let (after_at, after_id) = match after {
        Some((at, id)) => (Some(at), Some(id.as_uuid())),
        None => (None, None),
    };
    sqlx::query_as!(
        TipRow,
        r#"
        SELECT * FROM tips
        WHERE (($2 = 'sent' AND sender_id = $1)
               OR ($2 = 'received' AND receiver_id = $1)
               OR ($2 = 'all' AND (sender_id = $1 OR receiver_id = $1)))
          AND ($3::text IS NULL OR state = $3)
          AND ($4::timestamptz IS NULL OR (created_at, id) < ($4, $5::uuid))
        ORDER BY created_at DESC, id DESC
        LIMIT $6
        "#,
        user.as_uuid(),
        direction.as_str(),
        state.map(TipState::as_str),
        after_at,
        after_id,
        limit,
    )
    .fetch_all(exec)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// The sender's tips that are still waiting on something (no money in flight), locked until
/// the caller's transaction ends. Used when an account is deleted.
pub async fn lock_open_by_sender(conn: &mut PgConnection, sender: UserId) -> StoreResult<Vec<Tip>> {
    sqlx::query_as!(
        TipRow,
        r#"
        SELECT * FROM tips
        WHERE sender_id = $1
          AND state IN ('awaiting_sender', 'verifying', 'awaiting_confirmation', 'held', 'pledged')
        ORDER BY created_at
        FOR UPDATE
        "#,
        sender.as_uuid(),
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Tips per state, for the `tips_in_state` gauge.
pub async fn count_by_state(exec: impl PgExecutor<'_>) -> StoreResult<Vec<(String, i64)>> {
    let rows = sqlx::query!(r#"SELECT state, count(*) AS "n!" FROM tips GROUP BY state"#)
        .fetch_all(exec)
        .await?;
    Ok(rows.into_iter().map(|r| (r.state, r.n)).collect())
}

/// Records the payment hash of the tip's current attempt (each attempt has a fresh invoice).
/// Not a state change, so it doesn't go through [`transition`].
pub async fn set_payment_hash(
    exec: impl PgExecutor<'_>,
    id: TipId,
    payment_hash: &str,
) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE tips SET payment_hash = $2 WHERE id = $1",
        id.as_uuid(),
        payment_hash,
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// The audit trail for one tip, oldest first.
pub async fn events(exec: impl PgExecutor<'_>, id: TipId) -> StoreResult<Vec<TipEvent>> {
    let rows = sqlx::query!(
        r#"
        SELECT from_state, to_state, detail, at FROM tip_events
        WHERE tip_id = $1 ORDER BY id
        "#,
        id.as_uuid()
    )
    .fetch_all(exec)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(TipEvent {
                from_state: row
                    .from_state
                    .map(|s| s.parse())
                    .transpose()
                    .map_err(corrupt)?,
                to_state: row.to_state.parse().map_err(corrupt)?,
                detail: row.detail,
                at: row.at,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    #[test]
    fn idempotency_key_is_stable_per_command() {
        let key = idempotency_key(GithubInstallationId::new(7), 1001, 0);
        assert_eq!(key, idempotency_key(GithubInstallationId::new(7), 1001, 0));
        assert_eq!(key, hex::encode(Sha256::digest(b"7:1001:0")));
        assert_ne!(key, idempotency_key(GithubInstallationId::new(7), 1001, 1));
        assert_ne!(key, idempotency_key(GithubInstallationId::new(8), 1001, 0));
    }

    /// Guards the single-writer rule: only this module may assign `tips.state`.
    #[test]
    fn only_this_module_writes_tip_state() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let this_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tips.rs");
        let mut offenders = Vec::new();
        visit(&crates, &mut |path| {
            if path.extension().is_some_and(|ext| ext == "rs")
                && fs::canonicalize(path).ok() != fs::canonicalize(&this_file).ok()
                && writes_tip_state(&fs::read_to_string(path).unwrap_or_default())
            {
                offenders.push(path.display().to_string());
            }
        });
        assert!(
            offenders.is_empty(),
            "tips.state written outside store::tips: {offenders:?}"
        );
    }

    #[test]
    fn detector_flags_every_way_of_writing_state() {
        for sql in [
            "UPDATE tips SET state = 'paid' WHERE id = $1",
            "UPDATE tips SET version = version + 1, state = 'paid' WHERE id = $1",
            "update tips\n   set error_code = $2,state=$3\n where id = $1",
            "UPDATE tips AS t SET \"state\" = $1 RETURNING *",
            "UPDATE tips SET state = $1",
            "INSERT INTO tips (id, state, amount_sats) VALUES ($1, $2, $3)",
        ] {
            assert!(writes_tip_state(sql), "missed: {sql}");
        }
    }

    #[test]
    fn detector_ignores_reads_and_other_tables() {
        for sql in [
            "UPDATE tips SET email_status = $2 WHERE state = 'paid'",
            "UPDATE tips SET error_code = $1 WHERE id = $2 AND state = $3 RETURNING state",
            "UPDATE tip_events SET detail = $1 WHERE to_state = 'paid'",
            "UPDATE tips_archive SET state = 'x'",
            "SELECT state FROM tips WHERE state = $1",
            "INSERT INTO tip_events (tip_id, from_state, to_state) VALUES ($1, $2, $3)",
            "INSERT INTO tips (id, prestate_note) VALUES ($1, $2)",
            "// reset state after the test",
        ] {
            assert!(!writes_tip_state(sql), "false positive: {sql}");
        }
    }

    /// Whether `source` contains SQL that assigns `tips.state`: `state` among the assignments of
    /// an `UPDATE tips … SET` (before `WHERE`/`RETURNING`), or in the column list of an
    /// `INSERT INTO tips (…)`. Column order and spacing don't matter; mentions elsewhere do not
    /// count.
    fn writes_tip_state(source: &str) -> bool {
        let text = source.to_lowercase().replace('"', "");
        statements(&text, "update tips").any(|rest| {
            let Some(set_at) = find_word(rest, "set") else {
                return false;
            };
            let assignments = &rest[set_at + 3..];
            let end = ["where", "returning", ";"]
                .iter()
                .filter_map(|stop| find_word(assignments, stop))
                .min()
                .unwrap_or(assignments.len());
            assigns_state(&assignments[..end])
        }) || statements(&text, "insert into tips").any(|rest| {
            let rest = rest.trim_start();
            rest.strip_prefix('(')
                .and_then(|cols| cols.split_once(')'))
                .is_some_and(|(cols, _)| cols.split(',').any(|col| col.trim() == "state"))
        })
    }

    /// The text after each occurrence of `prefix` that ends at a word boundary (so
    /// `update tips` does not match `update tips_archive`).
    fn statements<'a>(text: &'a str, prefix: &'a str) -> impl Iterator<Item = &'a str> {
        let collapsed_prefix: Vec<&str> = prefix.split(' ').collect();
        text.match_indices(collapsed_prefix[0])
            .filter_map(move |(at, _)| {
                let mut rest = &text[at..];
                for word in &collapsed_prefix {
                    rest = rest.trim_start().strip_prefix(word)?;
                    if rest.starts_with(is_ident_char) {
                        return None;
                    }
                }
                Some(rest)
            })
    }

    fn assigns_state(assignments: &str) -> bool {
        let mut search = assignments;
        while let Some(at) = find_word(search, "state") {
            if search[at + 5..].trim_start().starts_with('=') {
                return true;
            }
            search = &search[at + 5..];
        }
        false
    }

    /// Byte offset of `word` in `text` where it is not part of a longer identifier.
    fn find_word(text: &str, word: &str) -> Option<usize> {
        text.match_indices(word).map(|(at, _)| at).find(|&at| {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].chars().next();
            !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
        })
    }

    fn is_ident_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    fn visit(dir: &Path, f: &mut dyn FnMut(&Path)) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "target") {
                    visit(&path, f);
                }
            } else {
                f(&path);
            }
        }
    }
}
