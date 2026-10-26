//! Every envelope-encrypted column, for re-wrapping data keys after a `MASTER_KEY` rotation
//! (runbook `key-rotation.md`). Each column's AAD is `"{table}.{column}:{id}"`.

use sqlx::PgExecutor;
use uuid::Uuid;

use crate::error::StoreResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    UserEmail,
    UserGithubToken,
    UserGithubRefreshToken,
    WalletNwcUri,
    TipClaimToken,
    PayoutBeneficiary,
}

impl Column {
    pub const ALL: [Self; 6] = [
        Self::UserEmail,
        Self::UserGithubToken,
        Self::UserGithubRefreshToken,
        Self::WalletNwcUri,
        Self::TipClaimToken,
        Self::PayoutBeneficiary,
    ];

    /// The AAD prefix; the row ID follows the colon.
    pub const fn aad_prefix(self) -> &'static str {
        match self {
            Self::UserEmail => "users.email",
            Self::UserGithubToken => "users.github_token",
            Self::UserGithubRefreshToken => "users.github_refresh_token",
            Self::WalletNwcUri => "wallet_connections.nwc_uri",
            Self::TipClaimToken => "tips.claim_token",
            Self::PayoutBeneficiary => "payout_methods.beneficiary",
        }
    }

    pub fn aad(self, id: Uuid) -> String {
        format!("{}:{id}", self.aad_prefix())
    }
}

/// Up to `limit` (id, ciphertext) pairs with an ID above `after`, in ID order.
pub async fn batch(
    exec: impl PgExecutor<'_>,
    column: Column,
    after: Uuid,
    limit: i64,
) -> StoreResult<Vec<(Uuid, Vec<u8>)>> {
    macro_rules! page {
        ($sql:literal) => {
            sqlx::query!($sql, after, limit)
                .fetch_all(exec)
                .await?
                .into_iter()
                .map(|r| (r.id, r.blob))
                .collect()
        };
    }
    Ok(match column {
        Column::UserEmail => page!(
            r#"SELECT id, email_ciphertext AS "blob!" FROM users
               WHERE email_ciphertext IS NOT NULL AND id > $1 ORDER BY id LIMIT $2"#
        ),
        Column::UserGithubToken => page!(
            r#"SELECT id, github_token_ciphertext AS "blob!" FROM users
               WHERE github_token_ciphertext IS NOT NULL AND id > $1 ORDER BY id LIMIT $2"#
        ),
        Column::UserGithubRefreshToken => page!(
            r#"SELECT id, github_refresh_token_ciphertext AS "blob!" FROM users
               WHERE github_refresh_token_ciphertext IS NOT NULL AND id > $1 ORDER BY id LIMIT $2"#
        ),
        Column::WalletNwcUri => page!(
            r#"SELECT id, nwc_uri_ciphertext AS "blob!" FROM wallet_connections
               WHERE id > $1 ORDER BY id LIMIT $2"#
        ),
        Column::TipClaimToken => page!(
            r#"SELECT id, claim_token_ciphertext AS "blob!" FROM tips
               WHERE claim_token_ciphertext IS NOT NULL AND id > $1 ORDER BY id LIMIT $2"#
        ),
        Column::PayoutBeneficiary => page!(
            r#"SELECT id, beneficiary_ciphertext AS "blob!" FROM payout_methods
               WHERE beneficiary_ciphertext IS NOT NULL AND id > $1 ORDER BY id LIMIT $2"#
        ),
    })
}

/// Replaces `old` with `new`, only if the value is still `old` (so a value changed meanwhile,
/// e.g. a reconnected wallet, is never overwritten). Returns whether it was replaced.
pub async fn replace(
    exec: impl PgExecutor<'_>,
    column: Column,
    id: Uuid,
    old: &[u8],
    new: &[u8],
) -> StoreResult<bool> {
    macro_rules! swap {
        ($sql:literal) => {
            sqlx::query!($sql, id, new, old)
                .execute(exec)
                .await?
                .rows_affected()
        };
    }
    let rows = match column {
        Column::UserEmail => {
            swap!("UPDATE users SET email_ciphertext = $2 WHERE id = $1 AND email_ciphertext = $3")
        }
        Column::UserGithubToken => swap!(
            "UPDATE users SET github_token_ciphertext = $2 WHERE id = $1 AND github_token_ciphertext = $3"
        ),
        Column::UserGithubRefreshToken => swap!(
            "UPDATE users SET github_refresh_token_ciphertext = $2
             WHERE id = $1 AND github_refresh_token_ciphertext = $3"
        ),
        Column::WalletNwcUri => swap!(
            "UPDATE wallet_connections SET nwc_uri_ciphertext = $2
             WHERE id = $1 AND nwc_uri_ciphertext = $3"
        ),
        Column::TipClaimToken => swap!(
            "UPDATE tips SET claim_token_ciphertext = $2 WHERE id = $1 AND claim_token_ciphertext = $3"
        ),
        Column::PayoutBeneficiary => swap!(
            "UPDATE payout_methods SET beneficiary_ciphertext = $2
             WHERE id = $1 AND beneficiary_ciphertext = $3"
        ),
    };
    Ok(rows == 1)
}
