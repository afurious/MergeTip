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
            preimage = COALESCE($11, preimage)
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
    row.try_into()
}

/// Live tips the sender created since `since`, other than `exclude`. Rejected tips don't count,
/// so a burst of invalid commands can't lock a sender out.
pub async fn count_by_sender_since(
    exec: impl PgExecutor<'_>,
    sender: UserId,
    since: OffsetDateTime,
    exclude: TipId,
) -> StoreResult<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!" FROM tips
        WHERE sender_id = $1 AND created_at >= $2 AND id <> $3 AND state <> 'rejected'
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
