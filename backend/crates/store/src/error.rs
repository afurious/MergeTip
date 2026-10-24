use mergetip_core::IllegalTransition;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("record not found")]
    NotFound,
    /// A compare-and-set lost a race: the row changed since it was read.
    #[error("record was modified concurrently")]
    Conflict,
    #[error(transparent)]
    IllegalTransition(#[from] IllegalTransition),
    /// A stored value no longer maps onto a domain type (e.g. an unknown state string).
    #[error("corrupt record: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(sqlx::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        match err {
            sqlx::Error::RowNotFound => Self::NotFound,
            other => Self::Database(other),
        }
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

pub(crate) fn corrupt(what: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(what.to_string())
}
