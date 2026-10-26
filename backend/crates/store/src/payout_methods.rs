//! `payout_methods`: where a receiver wants fiat tips paid (RFC 0004). The beneficiary is stored
//! envelope-encrypted; nothing here ever sees it in plain text.

use mergetip_core::UserId;
use sqlx::{Acquire, PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult, corrupt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationType {
    Bank,
    MobileMoney,
}

impl DestinationType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bank => "bank",
            Self::MobileMoney => "mobile_money",
        }
    }
}

impl std::str::FromStr for DestinationType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bank" => Ok(Self::Bank),
            "mobile_money" => Ok(Self::MobileMoney),
            other => Err(format!("unknown destination type {other:?}")),
        }
    }
}

/// A fiat payout method. `beneficiary_ciphertext` opens with AAD
/// `payout_methods.beneficiary:{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutMethod {
    pub id: Uuid,
    pub user_id: UserId,
    pub country: String,
    pub currency: String,
    pub destination_type: DestinationType,
    pub beneficiary_ciphertext: Vec<u8>,
    /// Shown to the receiver instead of the account number (e.g. `****1234`).
    pub provider_customer_id: Option<String>,
    pub verified_at: Option<OffsetDateTime>,
    pub is_default: bool,
    pub created_at: OffsetDateTime,
}

struct Row {
    id: Uuid,
    user_id: Uuid,
    country: Option<String>,
    currency: Option<String>,
    destination_type: Option<String>,
    beneficiary_ciphertext: Option<Vec<u8>>,
    provider_customer_id: Option<String>,
    verified_at: Option<OffsetDateTime>,
    is_default: bool,
    created_at: OffsetDateTime,
}

impl TryFrom<Row> for PayoutMethod {
    type Error = StoreError;

    fn try_from(row: Row) -> Result<Self, Self::Error> {
        let missing = |what: &str| corrupt(format!("fiat payout method without {what}"));
        Ok(Self {
            id: row.id,
            user_id: UserId::from_uuid(row.user_id),
            country: row.country.ok_or_else(|| missing("country"))?,
            currency: row.currency.ok_or_else(|| missing("currency"))?,
            destination_type: row
                .destination_type
                .ok_or_else(|| missing("destination type"))?
                .parse()
                .map_err(corrupt)?,
            beneficiary_ciphertext: row
                .beneficiary_ciphertext
                .ok_or_else(|| missing("beneficiary"))?,
            provider_customer_id: row.provider_customer_id,
            verified_at: row.verified_at,
            is_default: row.is_default,
            created_at: row.created_at,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewPayoutMethod<'a> {
    /// Chosen by the caller so the beneficiary can be sealed with this row's AAD first.
    pub id: Uuid,
    pub user_id: UserId,
    pub country: &'a str,
    pub currency: &'a str,
    pub destination_type: DestinationType,
    pub beneficiary_ciphertext: &'a [u8],
    pub provider_customer_id: Option<&'a str>,
    pub verified_at: OffsetDateTime,
    pub is_default: bool,
}

/// Adds a fiat method. Making it the default clears the previous default in the same
/// transaction, so the one-default index never trips.
pub async fn insert(
    conn: &mut PgConnection,
    new: &NewPayoutMethod<'_>,
) -> StoreResult<PayoutMethod> {
    let mut tx = conn.begin().await?;
    if new.is_default {
        clear_default(&mut *tx, new.user_id).await?;
    }
    let row = sqlx::query_as!(
        Row,
        r#"
        INSERT INTO payout_methods (id, user_id, kind, country, currency, destination_type,
                                    beneficiary_ciphertext, provider_customer_id, verified_at,
                                    is_default)
        VALUES ($1, $2, 'bitnob_fiat', $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, user_id, country, currency, destination_type, beneficiary_ciphertext,
                  provider_customer_id, verified_at, is_default, created_at
        "#,
        new.id,
        new.user_id.as_uuid(),
        new.country,
        new.currency,
        new.destination_type.as_str(),
        new.beneficiary_ciphertext,
        new.provider_customer_id,
        new.verified_at,
        new.is_default,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    row.try_into()
}

pub async fn list_for_user(
    exec: impl PgExecutor<'_>,
    user: UserId,
) -> StoreResult<Vec<PayoutMethod>> {
    sqlx::query_as!(
        Row,
        r#"
        SELECT id, user_id, country, currency, destination_type, beneficiary_ciphertext,
               provider_customer_id, verified_at, is_default, created_at
        FROM payout_methods WHERE user_id = $1 AND kind = 'bitnob_fiat'
        ORDER BY created_at
        "#,
        user.as_uuid(),
    )
    .fetch_all(exec)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// The user's default fiat method, if their default is fiat (otherwise they are paid over
/// Lightning).
pub async fn default_for_user(
    exec: impl PgExecutor<'_>,
    user: UserId,
) -> StoreResult<Option<PayoutMethod>> {
    sqlx::query_as!(
        Row,
        r#"
        SELECT id, user_id, country, currency, destination_type, beneficiary_ciphertext,
               provider_customer_id, verified_at, is_default, created_at
        FROM payout_methods WHERE user_id = $1 AND kind = 'bitnob_fiat' AND is_default
        "#,
        user.as_uuid(),
    )
    .fetch_optional(exec)
    .await?
    .map(TryInto::try_into)
    .transpose()
}

async fn clear_default(exec: impl PgExecutor<'_>, user: UserId) -> StoreResult<()> {
    sqlx::query!(
        "UPDATE payout_methods SET is_default = false WHERE user_id = $1 AND is_default",
        user.as_uuid()
    )
    .execute(exec)
    .await?;
    Ok(())
}

/// Makes the method the user's default, or stops it being the default. Returns `false` if the
/// user has no such method.
pub async fn set_default(
    conn: &mut PgConnection,
    user: UserId,
    id: Uuid,
    is_default: bool,
) -> StoreResult<bool> {
    let mut tx = conn.begin().await?;
    if is_default {
        clear_default(&mut *tx, user).await?;
    }
    let result = sqlx::query!(
        "UPDATE payout_methods SET is_default = $3 WHERE id = $1 AND user_id = $2",
        id,
        user.as_uuid(),
        is_default,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

/// Deletes the user's method. Returns `false` if they have no such method.
pub async fn delete(exec: impl PgExecutor<'_>, user: UserId, id: Uuid) -> StoreResult<bool> {
    let result = sqlx::query!(
        "DELETE FROM payout_methods WHERE id = $1 AND user_id = $2",
        id,
        user.as_uuid()
    )
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Every method of a user, for account deletion.
pub async fn delete_for_user(exec: impl PgExecutor<'_>, user: UserId) -> StoreResult<()> {
    sqlx::query!(
        "DELETE FROM payout_methods WHERE user_id = $1",
        user.as_uuid()
    )
    .execute(exec)
    .await?;
    Ok(())
}
