//! `installations` and `installation_repos`: where the GitHub App is installed.

use mergetip_core::{GithubInstallationId, InstallationId};
use sqlx::{Acquire, PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub id: InstallationId,
    pub github_installation_id: GithubInstallationId,
    pub account_login: String,
    pub suspended_at: Option<OffsetDateTime>,
}

/// A repository the installation can access, as listed in installation webhooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationRepo {
    pub github_repo_id: i64,
    pub full_name: String,
}

struct InstallationRow {
    id: Uuid,
    github_installation_id: i64,
    account_login: String,
    suspended_at: Option<OffsetDateTime>,
}

impl From<InstallationRow> for Installation {
    fn from(row: InstallationRow) -> Self {
        Self {
            id: InstallationId::from_uuid(row.id),
            github_installation_id: GithubInstallationId::new(row.github_installation_id),
            account_login: row.account_login,
            suspended_at: row.suspended_at,
        }
    }
}

/// Records an installation, or reactivates it after a reinstall.
pub async fn upsert(
    exec: impl PgExecutor<'_>,
    github_installation_id: GithubInstallationId,
    account_login: &str,
) -> StoreResult<Installation> {
    let row = sqlx::query_as!(
        InstallationRow,
        r#"
        INSERT INTO installations (github_installation_id, account_login)
        VALUES ($1, $2)
        ON CONFLICT (github_installation_id) DO UPDATE SET
            account_login = EXCLUDED.account_login,
            suspended_at = NULL
        RETURNING id, github_installation_id, account_login, suspended_at
        "#,
        github_installation_id.get(),
        account_login,
    )
    .fetch_one(exec)
    .await?;
    Ok(row.into())
}

pub async fn get_by_github_id(
    exec: impl PgExecutor<'_>,
    github_installation_id: GithubInstallationId,
) -> StoreResult<Installation> {
    let row = sqlx::query_as!(
        InstallationRow,
        r#"
        SELECT id, github_installation_id, account_login, suspended_at
        FROM installations WHERE github_installation_id = $1
        "#,
        github_installation_id.get(),
    )
    .fetch_one(exec)
    .await?;
    Ok(row.into())
}

/// Sets or clears the suspension time (`installation.suspend` / `unsuspend` events).
pub async fn set_suspended(
    exec: impl PgExecutor<'_>,
    github_installation_id: GithubInstallationId,
    suspended_at: Option<OffsetDateTime>,
) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE installations SET suspended_at = $2 WHERE github_installation_id = $1",
        github_installation_id.get(),
        suspended_at,
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// Handles `installation.deleted`. The row is kept, marked suspended, because tips reference it
/// and their history must survive an uninstall; only the repository list is dropped.
pub async fn delete(
    conn: &mut PgConnection,
    github_installation_id: GithubInstallationId,
    at: OffsetDateTime,
) -> StoreResult<()> {
    let mut tx = conn.begin().await?;
    sqlx::query!(
        r#"
        DELETE FROM installation_repos
        WHERE installation_id = (SELECT id FROM installations WHERE github_installation_id = $1)
        "#,
        github_installation_id.get(),
    )
    .execute(&mut *tx)
    .await?;
    set_suspended(&mut *tx, github_installation_id, Some(at)).await?;
    tx.commit().await?;
    Ok(())
}

/// Replaces the repository list (`installation.created`).
pub async fn set_repos(
    conn: &mut PgConnection,
    installation_id: InstallationId,
    repos: &[InstallationRepo],
) -> StoreResult<()> {
    let mut tx = conn.begin().await?;
    sqlx::query!(
        "DELETE FROM installation_repos WHERE installation_id = $1",
        installation_id.as_uuid(),
    )
    .execute(&mut *tx)
    .await?;
    add_repos(&mut *tx, installation_id, repos).await?;
    tx.commit().await?;
    Ok(())
}

/// Adds repositories (`installation_repositories.added`); existing rows get the new name.
pub async fn add_repos(
    exec: impl PgExecutor<'_>,
    installation_id: InstallationId,
    repos: &[InstallationRepo],
) -> StoreResult<()> {
    let (ids, names): (Vec<i64>, Vec<String>) = repos
        .iter()
        .map(|r| (r.github_repo_id, r.full_name.clone()))
        .unzip();
    sqlx::query!(
        r#"
        INSERT INTO installation_repos (installation_id, github_repo_id, full_name)
        SELECT $1, * FROM UNNEST($2::bigint[], $3::text[])
        ON CONFLICT (installation_id, github_repo_id) DO UPDATE SET full_name = EXCLUDED.full_name
        "#,
        installation_id.as_uuid(),
        &ids,
        &names,
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// Removes repositories (`installation_repositories.removed`).
pub async fn remove_repos(
    exec: impl PgExecutor<'_>,
    installation_id: InstallationId,
    github_repo_ids: &[i64],
) -> StoreResult<()> {
    sqlx::query!(
        "DELETE FROM installation_repos WHERE installation_id = $1 AND github_repo_id = ANY($2)",
        installation_id.as_uuid(),
        github_repo_ids,
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// The active installation covering `owner/name`, if the App can act on that repository.
pub async fn find_for_repo(
    exec: impl PgExecutor<'_>,
    full_name: &str,
) -> StoreResult<Option<Installation>> {
    let row = sqlx::query_as!(
        InstallationRow,
        r#"
        SELECT i.id, i.github_installation_id, i.account_login, i.suspended_at
        FROM installations i
        JOIN installation_repos r ON r.installation_id = i.id
        WHERE lower(r.full_name) = lower($1) AND i.suspended_at IS NULL
        LIMIT 1
        "#,
        full_name,
    )
    .fetch_optional(exec)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn list_repos(
    exec: impl PgExecutor<'_>,
    installation_id: InstallationId,
) -> StoreResult<Vec<InstallationRepo>> {
    let rows = sqlx::query_as!(
        InstallationRepo,
        r#"
        SELECT github_repo_id, full_name FROM installation_repos
        WHERE installation_id = $1 ORDER BY full_name
        "#,
        installation_id.as_uuid(),
    )
    .fetch_all(exec)
    .await?;
    Ok(rows)
}
