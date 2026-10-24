//! The API error contract: `{"error":{"code":"snake_case","message":"Human text"}}`.

use std::borrow::Cow;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use mergetip_store::StoreError;
use serde::Serialize;
use utoipa::ToSchema;

/// An error a handler returns. `code` is stable and machine-readable (the frontend maps it to
/// copy); `message` is for developers and may change.
#[derive(Debug, thiserror::Error)]
#[error("{status} {code}: {message}")]
pub struct ApiError {
    pub status: StatusCode,
    pub code: Cow<'static, str>,
    pub message: Cow<'static, str>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Stable `snake_case` identifier, e.g. `nwc_invalid_uri`.
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "The resource does not exist.",
        )
    }

    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "Sign in to continue.",
        )
    }

    pub fn unprocessable(
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    /// Logs the cause with full context and returns a generic 500, so internals never leak.
    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Something went wrong on our side.",
        )
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound => Self::not_found(),
            other => Self::internal(other),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.into_owned(),
                message: self.message.into_owned(),
            },
        };
        (self.status, Json(body)).into_response()
    }
}
