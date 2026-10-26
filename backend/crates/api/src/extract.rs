//! Authenticating requests (RFC 0002).
//!
//! [`AuthUser`] accepts the web session cookie or an extension bearer token; [`SessionUser`]
//! accepts only the cookie, for endpoints the extension must never reach (sign out, minting
//! extension tokens, confirming large tips, deleting the account).
//!
//! Cookie-authenticated writes must come from the web app's origin (or an allowed extension
//! origin): browsers attach cookies to cross-site requests, and `SameSite=Lax` alone still lets a
//! top-level form POST through. Bearer tokens are never attached automatically, so they need no
//! such check.

use axum::{
    extract::FromRequestParts,
    http::{Method, StatusCode, header, request::Parts},
};
use mergetip_crypto::hash_token;
use mergetip_store::{
    sessions::{self, Session, SessionKind},
    users::{self, User},
};
use time::Duration;

use crate::{ApiError, AppState, cookies, routes::auth::WEB_SESSION_TTL};

/// Sliding sessions are extended at most this often, so most requests don't write.
const TOUCH_INTERVAL: Duration = Duration::hours(1);

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user: User,
    pub session: Session,
}

/// An [`AuthUser`] authenticated by the web session cookie.
#[derive(Debug, Clone)]
pub struct SessionUser(pub AuthUser);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if let Some(token) = bearer(parts) {
            return authenticate(state, token, SessionKind::Extension).await;
        }
        cookie_session(parts, state).await
    }
}

impl FromRequestParts<AppState> for SessionUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        cookie_session(parts, state).await.map(Self)
    }
}

async fn cookie_session(parts: &Parts, state: &AppState) -> Result<AuthUser, ApiError> {
    let token =
        cookies::get(&parts.headers, cookies::SESSION).ok_or_else(ApiError::unauthenticated)?;
    if !is_safe(&parts.method) && !origin_allowed(parts, state) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "csrf_rejected",
            "This request must come from the MergeTip web app.",
        ));
    }
    authenticate(state, token, SessionKind::Web).await
}

async fn authenticate(
    state: &AppState,
    token: &str,
    kind: SessionKind,
) -> Result<AuthUser, ApiError> {
    let now = state.clock().now();
    let session = sessions::find_active(state.pool(), &hash_token(token), now)
        .await?
        .filter(|s| s.kind == kind)
        .ok_or_else(ApiError::unauthenticated)?;
    if kind == SessionKind::Web && now - session.last_seen_at > TOUCH_INTERVAL {
        sessions::touch(state.pool(), session.id, now, now + WEB_SESSION_TTL).await?;
    }
    let user = users::get(state.pool(), session.user_id).await?;
    Ok(AuthUser { user, session })
}

fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn origin_allowed(parts: &Parts, state: &AppState) -> bool {
    let Some(origin) = parts.headers.get(header::ORIGIN) else {
        return false;
    };
    let config = state.config();
    *origin == config.web_origin
        || config
            .extension_origins
            .iter()
            .any(|allowed| allowed == origin)
}
