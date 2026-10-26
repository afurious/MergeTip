//! `wallet_connections`: a sender's NWC connection. Writes arrive in M3; until then the engine
//! only needs to know whether a sender has one.

use mergetip_core::UserId;
use sqlx::{Acquire, PgConnection, PgExecutor};

use crate::error::StoreResult;

/// Whether the user has a connected, unrevoked wallet.
pub async fn has_active(exec: impl PgExecutor<'_>, user: UserId) -> StoreResult<bool> {
    let active = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM wallet_connections WHERE user_id = $1 AND revoked_at IS NULL
        ) AS "active!"
        "#,
        user.as_uuid(),
    )
    .fetch_one(exec)
    .await?;
    Ok(active)
}

/// A connected wallet, without its encrypted URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnection {
    pub id: uuid::Uuid,
    pub user_id: UserId,
    pub wallet_pubkey: String,
    pub relays: Vec<String>,
    pub methods: Vec<String>,
    pub encryption: String,
    pub created_at: time::OffsetDateTime,
}

struct ConnectionRow {
    id: uuid::Uuid,
    user_id: uuid::Uuid,
    wallet_pubkey: String,
    relays: Vec<String>,
    methods: Vec<String>,
    encryption: String,
    created_at: time::OffsetDateTime,
}

impl From<ConnectionRow> for WalletConnection {
    fn from(row: ConnectionRow) -> Self {
        Self {
            id: row.id,
            user_id: UserId::from_uuid(row.user_id),
            wallet_pubkey: row.wallet_pubkey,
            relays: row.relays,
            methods: row.methods,
            encryption: row.encryption,
            created_at: row.created_at,
        }
    }
}

/// The user's active connection, if any.
pub async fn active(
    exec: impl PgExecutor<'_>,
    user: UserId,
) -> StoreResult<Option<WalletConnection>> {
    let row = sqlx::query_as!(
        ConnectionRow,
        r#"
        SELECT id, user_id, wallet_pubkey, relays, methods, encryption, created_at
        FROM wallet_connections WHERE user_id = $1 AND revoked_at IS NULL
        "#,
        user.as_uuid(),
    )
    .fetch_optional(exec)
    .await?;
    Ok(row.map(Into::into))
}

/// A connection to store. `id` is chosen by the caller because the ciphertext's AAD includes it.
#[derive(Debug, Clone)]
pub struct NewConnection<'a> {
    pub id: uuid::Uuid,
    pub user_id: UserId,
    /// Sealed with AAD `wallet_connections.nwc_uri:{id}`.
    pub nwc_uri_ciphertext: &'a [u8],
    pub wallet_pubkey: &'a str,
    pub relays: &'a [String],
    pub methods: &'a [String],
    pub encryption: &'a str,
}

/// Makes `new` the user's only active connection, revoking any previous one in the same
/// transaction.
pub async fn replace(
    conn: &mut PgConnection,
    new: &NewConnection<'_>,
    now: time::OffsetDateTime,
) -> StoreResult<WalletConnection> {
    let mut tx = conn.begin().await?;
    revoke(&mut *tx, new.user_id, now).await?;
    let row = sqlx::query_as!(
        ConnectionRow,
        r#"
        INSERT INTO wallet_connections (id, user_id, nwc_uri_ciphertext, wallet_pubkey, relays,
                                        methods, encryption)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, user_id, wallet_pubkey, relays, methods, encryption, created_at
        "#,
        new.id,
        new.user_id.as_uuid(),
        new.nwc_uri_ciphertext,
        new.wallet_pubkey,
        new.relays,
        new.methods,
        new.encryption,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.into())
}

/// Revokes the user's active connection, if any. Returns whether one was revoked.
pub async fn revoke(
    exec: impl PgExecutor<'_>,
    user: UserId,
    now: time::OffsetDateTime,
) -> StoreResult<bool> {
    let result = sqlx::query!(
        "UPDATE wallet_connections SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL",
        user.as_uuid(),
        now,
    )
    .execute(exec)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The active connection's id and encrypted URI. Only the worker decrypts it.
pub async fn active_secret(
    exec: impl PgExecutor<'_>,
    user: UserId,
) -> StoreResult<Option<(uuid::Uuid, Vec<u8>)>> {
    let row = sqlx::query!(
        r#"
        SELECT id, nwc_uri_ciphertext FROM wallet_connections
        WHERE user_id = $1 AND revoked_at IS NULL
        "#,
        user.as_uuid(),
    )
    .fetch_optional(exec)
    .await?;
    Ok(row.map(|r| (r.id, r.nwc_uri_ciphertext)))
}
