mod common;

use mergetip_core::{GithubInstallationId, GithubUserId};
use mergetip_store::{
    StoreError, github_events,
    installations::{self, InstallationRepo},
    users::{self, GithubProfile},
};
use serde_json::json;
use sqlx::PgPool;
use time::macros::datetime;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn user_upsert_follows_renames_and_keeps_known_fields(pool: PgPool) -> sqlx::Result<()> {
    let mut conn = pool.acquire().await?;
    let created = users::upsert_by_github_id(
        &mut conn,
        &GithubProfile {
            github_id: GithubUserId::new(42),
            login: "Alice".into(),
            avatar_url: Some("https://avatars.example/a".into()),
            created_at: Some(datetime!(2020-01-01 0:00 UTC)),
        },
    )
    .await
    .unwrap();
    assert_eq!(created.per_tip_max.get(), 10_000);
    assert_eq!(created.daily_max.get(), 50_000);
    assert_eq!(created.confirm_threshold.get(), 50_000);

    let renamed = users::upsert_by_github_id(
        &mut conn,
        &GithubProfile {
            github_id: GithubUserId::new(42),
            login: "alice-renamed".into(),
            avatar_url: None,
            created_at: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(renamed.id, created.id);
    assert_eq!(renamed.github_login, "alice-renamed");
    assert_eq!(
        renamed.avatar_url.as_deref(),
        Some("https://avatars.example/a")
    );
    assert_eq!(
        renamed.github_created_at,
        Some(datetime!(2020-01-01 0:00 UTC))
    );
    assert_eq!(
        users::get_by_login(&pool, "ALICE-RENAMED")
            .await
            .unwrap()
            .id,
        created.id
    );
    assert!(matches!(
        users::get_by_login(&pool, "Alice").await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(users::get(&pool, created.id).await.unwrap(), renamed);
    Ok(())
}

fn profile(github_id: i64, login: &str) -> GithubProfile {
    GithubProfile {
        github_id: GithubUserId::new(github_id),
        login: login.into(),
        avatar_url: None,
        created_at: None,
    }
}

/// Account A was `alice`, renamed, and account B took `alice`. A's stale row must never answer
/// for `alice` again, even after A's row is updated.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn reused_login_resolves_to_its_current_holder(pool: PgPool) -> sqlx::Result<()> {
    let mut conn = pool.acquire().await?;
    let a = users::upsert_by_github_id(&mut conn, &profile(1, "alice"))
        .await
        .unwrap();
    let b = users::upsert_by_github_id(&mut conn, &profile(2, "Alice"))
        .await
        .unwrap();

    // A write to A's stale row (a settings change, say) must not make it win again.
    sqlx::query!(
        "UPDATE users SET lightning_address = 'a@example.com' WHERE id = $1",
        a.id.as_uuid()
    )
    .execute(&pool)
    .await?;
    assert_eq!(users::get_by_login(&pool, "alice").await.unwrap().id, b.id);

    // When A is seen again under its new login, it becomes current for that login.
    users::upsert_by_github_id(&mut conn, &profile(1, "alice-renamed"))
        .await
        .unwrap();
    assert_eq!(
        users::get_by_login(&pool, "alice-renamed").await.unwrap().id,
        a.id
    );
    assert_eq!(users::get_by_login(&pool, "alice").await.unwrap().id, b.id);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn two_current_rows_cannot_share_a_login(pool: PgPool) -> sqlx::Result<()> {
    let mut conn = pool.acquire().await?;
    users::upsert_by_github_id(&mut conn, &profile(1, "alice"))
        .await
        .unwrap();
    let duplicate = sqlx::query!(
        "INSERT INTO users (github_id, github_login) VALUES (2, 'ALICE')"
    )
    .execute(&pool)
    .await;
    assert!(duplicate.is_err(), "the unique index must reject a second current holder");
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn installation_lifecycle(pool: PgPool) -> sqlx::Result<()> {
    let installation = common::installation(&pool, 77).await;
    let mut conn = pool.acquire().await?;
    installations::set_repos(
        &mut conn,
        installation.id,
        &[
            InstallationRepo {
                github_repo_id: 1,
                full_name: "acme/widgets".into(),
            },
            InstallationRepo {
                github_repo_id: 2,
                full_name: "acme/gadgets".into(),
            },
        ],
    )
    .await
    .unwrap();
    installations::remove_repos(&pool, installation.id, &[2])
        .await
        .unwrap();

    let found = installations::find_for_repo(&pool, "ACME/Widgets")
        .await
        .unwrap();
    assert_eq!(found.map(|i| i.id), Some(installation.id));
    assert!(
        installations::find_for_repo(&pool, "acme/gadgets")
            .await
            .unwrap()
            .is_none()
    );

    installations::delete(
        &mut conn,
        GithubInstallationId::new(77),
        datetime!(2026-11-02 0:00 UTC),
    )
    .await
    .unwrap();
    let uninstalled = installations::get_by_github_id(&pool, GithubInstallationId::new(77))
        .await
        .unwrap();
    assert_eq!(
        uninstalled.suspended_at,
        Some(datetime!(2026-11-02 0:00 UTC))
    );
    assert!(
        installations::list_repos(&pool, installation.id)
            .await
            .unwrap()
            .is_empty()
    );

    let reinstalled = common::installation(&pool, 77).await;
    assert_eq!(reinstalled.id, installation.id);
    assert_eq!(reinstalled.suspended_at, None);
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn duplicate_delivery_is_stored_once(pool: PgPool) -> sqlx::Result<()> {
    let payload = json!({ "action": "created", "comment": { "id": 1 } });
    let first =
        github_events::insert_if_new(&pool, "d-1", "issue_comment", Some("created"), &payload)
            .await
            .unwrap();
    let second =
        github_events::insert_if_new(&pool, "d-1", "issue_comment", Some("created"), &payload)
            .await
            .unwrap();
    assert!(first);
    assert!(!second);

    github_events::mark_processed(&pool, "d-1", datetime!(2026-11-02 0:00 UTC))
        .await
        .unwrap();
    let event = github_events::get(&pool, "d-1").await.unwrap();
    assert_eq!(event.payload, payload);
    assert_eq!(event.processed_at, Some(datetime!(2026-11-02 0:00 UTC)));
    Ok(())
}
