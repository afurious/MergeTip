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

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{Method, StatusCode, header, request::Parts},
    middleware::Next,
    response::Response,
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

/// Set on the request by [`slide_session_cookie`]; the extractor flips it when the session
/// cookie authenticated the request.
#[derive(Clone, Default)]
pub(crate) struct SessionSeen(Arc<AtomicBool>);

/// Re-issues the session cookie (another 30 days) on responses to requests it authenticated, so
/// the browser's copy slides with the database expiry instead of lapsing 30 days after sign-in.
/// Responses that set the cookie themselves (sign-in, sign-out) are left alone.
pub async fn slide_session_cookie(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let seen = SessionSeen::default();
    let token = cookies::get(request.headers(), cookies::SESSION).map(str::to_owned);
    request.extensions_mut().insert(seen.clone());
    let mut response = next.run(request).await;
    let already_set = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|v| v.as_bytes().starts_with(cookies::SESSION.as_bytes()));
    if let Some(token) = token
        && seen.0.load(Ordering::Relaxed)
        && !already_set
        && !response.status().is_server_error()
    {
        response.headers_mut().append(
            header::SET_COOKIE,
            cookies::set(
                cookies::SESSION,
                &token,
                "/",
                WEB_SESSION_TTL.whole_seconds(),
                state.config().secure_cookies(),
            ),
        );
    }
    response
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
    let auth = authenticate(state, token, SessionKind::Web).await?;
    if let Some(seen) = parts.extensions.get::<SessionSeen>() {
        seen.0.store(true, Ordering::Relaxed);
    }
    Ok(auth)
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
