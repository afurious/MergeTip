//! `users`: one row per GitHub account, created the first time we see it as sender or receiver.

use mergetip_core::{GithubUserId, Sats, UserId};
use sqlx::PgExecutor;
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
pub async fn upsert_by_github_id(
    exec: impl PgExecutor<'_>,
    profile: &GithubProfile,
) -> StoreResult<User> {
    let row = sqlx::query_as!(
        UserRow,
        r#"
        INSERT INTO users (github_id, github_login, avatar_url, github_created_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (github_id) DO UPDATE SET
            github_login = EXCLUDED.github_login,
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
    .fetch_one(exec)
    .await?;
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

/// Case-insensitive lookup of a live (not deleted) account by its current login.
pub async fn get_by_login(exec: impl PgExecutor<'_>, login: &str) -> StoreResult<User> {
    sqlx::query_as!(
        UserRow,
        r#"
        SELECT id, github_id, github_login, avatar_url, github_created_at, lightning_address,
               per_tip_max_sats, daily_max_sats, confirm_threshold_sats, confirm_otp_enabled,
               email_opt_out, created_at, deleted_at
        FROM users
        WHERE lower(github_login) = lower($1) AND deleted_at IS NULL
        ORDER BY updated_at DESC
        LIMIT 1
        "#,
        login,
    )
    .fetch_one(exec)
    .await?
    .try_into()
}
