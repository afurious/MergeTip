use std::time::Duration;

use sqlx::{PgPool, postgres::PgPoolOptions};

/// Connects with bounded waits so a database outage fails health checks instead of hanging.
pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await
}

/// Applies pending migrations. Safe to run from several processes at once: sqlx serialises
/// migrators with a Postgres advisory lock.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    mergetip_store::MIGRATOR.run(pool).await
}
