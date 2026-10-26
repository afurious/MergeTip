//! `users`: one row per GitHub account, created the first time we see it as sender or receiver.

use mergetip_core::{GithubUserId, Sats, UserId};
use sqlx::{Acquire, PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreResult, corrupt};

/// What GitHub tells us about an account. `avatar_url` and `created_at` are absent from some
/// webhook payloads, so `None` keeps the stored value instead of erasing it.
#[derive(Debug, Clone)]
pub struct GithubProfile {
    pub github_id: GithubUserId,
    pub login: String,
    pub avatar_url: Option<String>,
    pub created_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub github_id: GithubUserId,
    pub github_login: String,
    pub avatar_url: Option<String>,
    pub github_created_at: Option<OffsetDateTime>,
    pub lightning_address: Option<String>,
    pub per_tip_max: Sats,
    pub daily_max: Sats,
    pub confirm_threshold: Sats,
    pub confirm_otp_enabled: bool,
    pub email_opt_out: bool,
    pub created_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

struct UserRow {
    id: Uuid,
    github_id: i64,
    github_login: String,
    avatar_url: Option<String>,
    github_created_at: Option<OffsetDateTime>,
    lightning_address: Option<String>,
    per_tip_max_sats: i64,
    daily_max_sats: i64,
    confirm_threshold_sats: i64,
    confirm_otp_enabled: bool,
    email_opt_out: bool,
    created_at: OffsetDateTime,
    deleted_at: Option<OffsetDateTime>,
}

impl TryFrom<UserRow> for User {
    type Error = crate::StoreError;

    fn try_from(row: UserRow) -> Result<Self, Self::Error> {
        let sats = |v: i64| Sats::try_from(v).map_err(|_| corrupt(format!("negative cap {v}")));
        Ok(Self {
            id: UserId::from_uuid(row.id),
            github_id: GithubUserId::new(row.github_id),
            github_login: row.github_login,
            avatar_url: row.avatar_url,
            github_created_at: row.github_created_at,
            lightning_address: row.lightning_address,
            per_tip_max: sats(row.per_tip_max_sats)?,
            daily_max: sats(row.daily_max_sats)?,
            confirm_threshold: sats(row.confirm_threshold_sats)?,
            confirm_otp_enabled: row.confirm_otp_enabled,
            email_opt_out: row.email_opt_out,
            created_at: row.created_at,
            deleted_at: row.deleted_at,
        })
    }
}

/// Inserts the account or refreshes its login and avatar. Keyed on `github_id` because logins
/// can be renamed and later reused by someone else.
///
/// Seeing this account with a login proves no other account holds that login any more, so in the
/// same transaction every other row with it is marked stale. [`get_by_login`] ignores stale rows,
/// and a unique index guarantees one current holder per login.
pub async fn upsert_by_github_id(
    conn: &mut PgConnection,
    profile: &GithubProfile,
) -> StoreResult<User> {
    let mut tx = conn.begin().await?;
    sqlx::query!(
        r#"
        UPDATE users SET login_stale = true
        WHERE lower(github_login) = lower($1) AND github_id <> $2 AND NOT login_stale
        "#,
        profile.login,
        profile.github_id.get(),
    )
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query_as!(
        UserRow,
        r#"
        INSERT INTO users (github_id, github_login, avatar_url, github_created_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (github_id) DO UPDATE SET
            github_login = EXCLUDED.github_login,
            login_stale = false,
            avatar_url = COALESCE(EXCLUDED.avatar_url, users.avatar_url),
            github_created_at = COALESCE(EXCLUDED.github_created_at, users.github_created_at)
        RETURNING id, github_id, github_login, avatar_url, github_created_at, lightning_address,
                  per_tip_max_sats, daily_max_sats, confirm_threshold_sats, confirm_otp_enabled,
                  email_opt_out, created_at, deleted_at
        "#,
        profile.github_id.get(),
        profile.login,
        profile.avatar_url,
        profile.created_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    row.try_into()
}

pub async fn get(exec: impl PgExecutor<'_>, id: UserId) -> StoreResult<User> {
    sqlx::query_as!(
        UserRow,
        r#"
        SELECT id, github_id, github_login, avatar_url, github_created_at, lightning_address,
               per_tip_max_sats, daily_max_sats, confirm_threshold_sats, confirm_otp_enabled,
               email_opt_out, created_at, deleted_at
        FROM users WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_one(exec)
    .await?
    .try_into()
}

/// Case-insensitive lookup of the account that currently holds `login`: not deleted, and not a
/// row whose login has since been taken by another account.
///
/// Our copy of a login can lag GitHub (we only learn of a rename when we next see the account),
/// so anything that moves money must still resolve the receiver through the GitHub API by ID.
pub async fn get_by_login(exec: impl PgExecutor<'_>, login: &str) -> StoreResult<User> {
    sqlx::query_as!(
        UserRow,
        r#"
        SELECT id, github_id, github_login, avatar_url, github_created_at, lightning_address,
               per_tip_max_sats, daily_max_sats, confirm_threshold_sats, confirm_otp_enabled,
               email_opt_out, created_at, deleted_at
        FROM users
        WHERE lower(github_login) = lower($1) AND NOT login_stale AND deleted_at IS NULL
        "#,
        login,
    )
    .fetch_one(exec)
    .await?
    .try_into()
}

/// Holds the user's row lock until the caller's transaction ends. Used to serialise per-user
/// limit checks (count, then insert) so concurrent requests can't both pass on a stale count.
pub async fn lock(conn: &mut PgConnection, id: UserId) -> StoreResult<()> {
    sqlx::query!(
        "SELECT id FROM users WHERE id = $1 FOR UPDATE",
        id.as_uuid()
    )
    .fetch_one(conn)
    .await?;
    Ok(())
}

/// Encrypted values stored for a user. Decrypt with the AAD named on each field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSecrets {
    /// AAD `users.github_token:{id}`.
    pub github_token_ciphertext: Option<Vec<u8>>,
    /// AAD `users.github_refresh_token:{id}`.
    pub github_refresh_token_ciphertext: Option<Vec<u8>>,
    pub github_token_expires_at: Option<OffsetDateTime>,
    /// AAD `users.email:{id}`.
    pub email_ciphertext: Option<Vec<u8>>,
    pub email_source: Option<String>,
}

pub async fn secrets(exec: impl PgExecutor<'_>, id: UserId) -> StoreResult<UserSecrets> {
    let row = sqlx::query_as!(
        UserSecrets,
        r#"
        SELECT github_token_ciphertext, github_refresh_token_ciphertext, github_token_expires_at,
               email_ciphertext, email_source
        FROM users WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_one(exec)
    .await?;
    Ok(row)
}

/// Stores the user's GitHub tokens (already encrypted). A `None` refresh token keeps the old one.
pub async fn set_github_tokens(
    exec: impl PgExecutor<'_>,
    id: UserId,
    access_ciphertext: &[u8],
    refresh_ciphertext: Option<&[u8]>,
    expires_at: Option<OffsetDateTime>,
) -> StoreResult<()> {
    sqlx::query!(
        r#"
        UPDATE users SET
            github_token_ciphertext = $2,
            github_refresh_token_ciphertext = COALESCE($3, github_refresh_token_ciphertext),
            github_token_expires_at = $4
        WHERE id = $1
        "#,
        id.as_uuid(),
        access_ciphertext,
        refresh_ciphertext,
        expires_at,
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// Where an email came from, best first (FR-11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailSource {
    Verified,
    PublicProfile,
    Commit,
}

impl EmailSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::PublicProfile => "public_profile",
            Self::Commit => "commit",
        }
    }
}

/// Stores an (encrypted) email. A verified email always wins; a weaker source never replaces a
/// stronger one.
pub async fn set_email(
    exec: impl PgExecutor<'_>,
    id: UserId,
    ciphertext: &[u8],
    source: EmailSource,
) -> StoreResult<()> {
    sqlx::query!(
        r#"
        UPDATE users SET email_ciphertext = $2, email_source = $3
        WHERE id = $1 AND (
            email_source IS NULL
            OR $3 = 'verified'
            OR (email_source = 'commit' AND $3 = 'public_profile')
            OR email_source = $3
        )
        "#,
        id.as_uuid(),
        ciphertext,
        source.as_str(),
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// Changes to a user's limits and preferences; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsUpdate {
    pub per_tip_max: Option<Sats>,
    pub daily_max: Option<Sats>,
    pub confirm_threshold: Option<Sats>,
    pub confirm_otp_enabled: Option<bool>,
    pub email_opt_out: Option<bool>,
}

pub async fn update_settings(
    exec: impl PgExecutor<'_>,
    id: UserId,
    update: &SettingsUpdate,
) -> StoreResult<()> {
    let sats = |v: Option<Sats>| v.map(i64::try_from).transpose().map_err(corrupt);
    sqlx::query!(
        r#"
        UPDATE users SET
            per_tip_max_sats = COALESCE($2, per_tip_max_sats),
            daily_max_sats = COALESCE($3, daily_max_sats),
            confirm_threshold_sats = COALESCE($4, confirm_threshold_sats),
            confirm_otp_enabled = COALESCE($5, confirm_otp_enabled),
            email_opt_out = COALESCE($6, email_opt_out)
        WHERE id = $1
        "#,
        id.as_uuid(),
        sats(update.per_tip_max)?,
        sats(update.daily_max)?,
        sats(update.confirm_threshold)?,
        update.confirm_otp_enabled,
        update.email_opt_out,
    )
    .execute(exec)
    .await?;
    Ok(())
}

pub async fn set_lightning_address(
    exec: impl PgExecutor<'_>,
    id: UserId,
    address: Option<&str>,
) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE users SET lightning_address = $2 WHERE id = $1",
        id.as_uuid(),
        address,
    )
    .execute(exec)
    .await?;
    Ok(())
}
