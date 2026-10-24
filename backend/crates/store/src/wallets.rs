//! `wallet_connections`: a sender's NWC connection. Writes arrive in M3; until then the engine
//! only needs to know whether a sender has one.

use mergetip_core::UserId;
use sqlx::PgExecutor;

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
