//! `sessions`: web cookies and extension bearer tokens, stored by hash only.

use std::{fmt, str::FromStr};

use mergetip_core::UserId;
use sqlx::PgExecutor;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreResult, corrupt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// `mt_session` cookie for the web app.
    Web,
    /// `Authorization: Bearer` token for the browser extension.
    Extension,
}

impl SessionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Extension => "extension",
        }
    }
}

impl fmt::Display for SessionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "web" => Ok(Self::Web),
            "extension" => Ok(Self::Extension),
            other => Err(format!("unknown session kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: Uuid,
    pub user_id: UserId,
    pub kind: SessionKind,
    pub expires_at: OffsetDateTime,
    pub last_seen_at: OffsetDateTime,
    /// When the user signed in (or the token was minted): how fresh the authentication is.
    pub created_at: OffsetDateTime,
}

struct SessionRow {
    id: Uuid,
    user_id: Uuid,
    kind: String,
    expires_at: OffsetDateTime,
    last_seen_at: OffsetDateTime,
    created_at: OffsetDateTime,
}

impl TryFrom<SessionRow> for Session {
    type Error = crate::StoreError;

    fn try_from(row: SessionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            user_id: UserId::from_uuid(row.user_id),
            kind: row.kind.parse().map_err(corrupt)?,
            expires_at: row.expires_at,
            last_seen_at: row.last_seen_at,
            created_at: row.created_at,
        })
    }
}

pub async fn create(
    exec: impl PgExecutor<'_>,
    user_id: UserId,
    kind: SessionKind,
    token_hash: &str,
    expires_at: OffsetDateTime,
    user_agent: Option<&str>,
) -> StoreResult<Session> {
    sqlx::query_as!(
        SessionRow,
        r#"
        INSERT INTO sessions (user_id, kind, token_hash, expires_at, user_agent)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, user_id, kind, expires_at, last_seen_at, created_at
        "#,
        user_id.as_uuid(),
        kind.as_str(),
        token_hash,
        expires_at,
        user_agent,
    )
    .fetch_one(exec)
    .await?
    .try_into()
}

/// The unexpired session for a token hash, if its user still exists and isn't deleted.
pub async fn find_active(
    exec: impl PgExecutor<'_>,
    token_hash: &str,
    now: OffsetDateTime,
) -> StoreResult<Option<Session>> {
    let row = sqlx::query_as!(
        SessionRow,
        r#"
        SELECT s.id, s.user_id, s.kind, s.expires_at, s.last_seen_at, s.created_at
        FROM sessions s JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = $1 AND s.expires_at > $2 AND u.deleted_at IS NULL
        "#,
        token_hash,
        now,
    )
    .fetch_optional(exec)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Records activity and, for sliding sessions, pushes the expiry out.
pub async fn touch(
    exec: impl PgExecutor<'_>,
    id: Uuid,
    now: OffsetDateTime,
    expires_at: OffsetDateTime,
) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE sessions SET last_seen_at = $2, expires_at = $3 WHERE id = $1",
        id,
        now,
        expires_at,
    )
    .execute(exec)
    .await?;
    Ok(())
}

pub async fn delete_by_hash(exec: impl PgExecutor<'_>, token_hash: &str) -> StoreResult<()> {
    sqlx::query!("DELETE FROM sessions WHERE token_hash = $1", token_hash)
        .execute(exec)
        .await?;
    Ok(())
}

/// Signs the user out everywhere (account deletion).
pub async fn delete_for_user(exec: impl PgExecutor<'_>, user_id: UserId) -> StoreResult<()> {
    sqlx::query!("DELETE FROM sessions WHERE user_id = $1", user_id.as_uuid())
        .execute(exec)
        .await?;
    Ok(())
}
