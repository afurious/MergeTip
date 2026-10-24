//! Schema invariants that Rust types depend on.

use mergetip_core::TipState;
use sqlx::PgPool;

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn core_tables_exist(pool: PgPool) -> sqlx::Result<()> {
    let tables: Vec<String> = sqlx::query_scalar!(
        r#"SELECT table_name AS "table_name!" FROM information_schema.tables
           WHERE table_schema = 'public' ORDER BY table_name"#
    )
    .fetch_all(&pool)
    .await?;
    for expected in [
        "bot_comments",
        "feature_flags",
        "github_events",
        "installation_repos",
        "installations",
        "jobs",
        "tip_events",
        "tips",
        "users",
        "wallet_connections",
    ] {
        assert!(tables.iter().any(|t| t == expected), "missing table {expected}");
    }
    Ok(())
}

/// The `tips.state` CHECK constraint must list exactly the states `core` knows about.
#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn state_check_matches_core_states(pool: PgPool) -> sqlx::Result<()> {
    let definition: String = sqlx::query_scalar!(
        r#"SELECT pg_get_constraintdef(oid) AS "def!" FROM pg_constraint
           WHERE conrelid = 'tips'::regclass AND conname = 'tips_state_check'"#
    )
    .fetch_one(&pool)
    .await?;
    for state in TipState::ALL {
        assert!(
            definition.contains(&format!("'{}'", state.as_str())),
            "{state} missing from {definition}"
        );
    }
    assert_eq!(definition.matches("::text").count(), TipState::ALL.len());
    Ok(())
}

#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]
async fn feature_flags_are_seeded(pool: PgPool) -> sqlx::Result<()> {
    let flags = sqlx::query!("SELECT key, enabled FROM feature_flags ORDER BY key")
        .fetch_all(&pool)
        .await?;
    let flags: Vec<(String, bool)> = flags.into_iter().map(|f| (f.key, f.enabled)).collect();
    assert_eq!(
        flags,
        [
            ("fiat_payouts".to_owned(), false),
            ("payments_enabled".to_owned(), true)
        ]
    );
    Ok(())
}
