//! Sign-in with GitHub (FR-6.1) and session handling (RFC 0002).

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mergetip_core::GithubUserId;
use mergetip_crypto::{constant_time_eq, random_token};
use mergetip_store::{
    sessions::{self, SessionKind},
    users::{self, EmailSource, GithubProfile},
};
use serde::{Deserialize, Serialize};
use time::Duration;

use crate::{ApiError, AppState, cookies, extract::SessionUser};

/// Web sessions last 30 days and slide forward with use.
pub const WEB_SESSION_TTL: Duration = Duration::days(30);
const OAUTH_COOKIE_PATH: &str = "/v1/auth";
const OAUTH_COOKIE_TTL_SECS: i64 = 600;
const DEFAULT_REDIRECT: &str = "/dashboard";

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct LoginQuery {
    /// Web app path to return to after signing in, e.g. `/claim/abc`. Defaults to `/dashboard`.
    redirect: Option<String>,
}

/// What `mt_oauth` holds between the redirect to GitHub and the callback.
#[derive(Serialize, Deserialize)]
struct OAuthState {
    state: String,
    redirect: String,
}

/// Start signing in: redirect to GitHub.
#[utoipa::path(
    get,
    path = "/v1/auth/github/login",
    operation_id = "github_login",
    tag = "auth",
    params(LoginQuery),
    responses((status = 302, description = "Redirect to GitHub"))
)]
pub async fn login(
    State(state): State<AppState>,
    Query(query): Query<LoginQuery>,
) -> Result<Response, ApiError> {
    let token = random_token().map_err(ApiError::internal)?;
    let pending = OAuthState {
        state: token.raw.expose().clone(),
        redirect: safe_redirect(query.redirect.as_deref()).to_owned(),
    };
    let cookie_value =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&pending).map_err(ApiError::internal)?);
    let config = state.config();
    let location = state
        .oauth()
        .authorize_url(&pending.state, &config.oauth_callback_url());

    let mut response = Redirect::to(&location).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        cookies::set(
            cookies::OAUTH_STATE,
            &cookie_value,
            OAUTH_COOKIE_PATH,
            OAUTH_COOKIE_TTL_SECS,
            config.secure_cookies(),
        ),
    );
    Ok(response)
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// Set by GitHub when the user declined.
    error: Option<String>,
}

/// GitHub's redirect back after sign-in: create the session and return to the web app.
#[utoipa::path(
    get,
    path = "/v1/auth/github/callback",
    operation_id = "github_callback",
    tag = "auth",
    params(CallbackQuery),
    responses(
        (status = 302, description = "Signed in; redirect to the web app with the session cookie set"),
        (status = 400, description = "State mismatch or missing code", body = crate::error::ErrorBody),
    )
)]
pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    let config = state.config();
    let pending = cookies::get(&headers, cookies::OAUTH_STATE)
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .and_then(|bytes| serde_json::from_slice::<OAuthState>(&bytes).ok())
        .ok_or_else(state_mismatch)?;
    let returned_state = query.state.as_deref().ok_or_else(state_mismatch)?;
    if !constant_time_eq(returned_state, &pending.state) {
        return Err(state_mismatch());
    }

    if let Some(error) = query.error {
        // The user declined on GitHub: back to the web app, which explains.
        let location = format!(
            "{}/?auth_error={}",
            config.web_base_url(),
            urlencode(&error)
        );
        return Ok(with_cleared_oauth_cookie(
            Redirect::to(&location).into_response(),
            config.secure_cookies(),
        ));
    }
    let code = query.code.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "oauth_missing_code",
            "GitHub did not send a code.",
        )
    })?;

    let tokens = state
        .oauth()
        .exchange_code(&code, &config.oauth_callback_url())
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "GitHub code exchange failed");
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "oauth_failed",
                "Signing in with GitHub failed. Please try again.",
            )
        })?;
    let identity = state
        .oauth()
        .identity(tokens.access_token.expose())
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "GitHub identity lookup failed");
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "oauth_failed",
                "Signing in with GitHub failed. Please try again.",
            )
        })?;

    let now = state.clock().now();
    let session_token = random_token().map_err(ApiError::internal)?;
    let keyring = state.keyring();
    let mut tx = state.pool().begin().await.map_err(ApiError::internal)?;
    let user = users::upsert_by_github_id(
        &mut tx,
        &GithubProfile {
            github_id: GithubUserId::new(identity.account.id),
            login: identity.account.login.clone(),
            avatar_url: identity.account.avatar_url.clone(),
            created_at: identity.account.created_at,
        },
    )
    .await?;
    // Signing in after deleting the account starts it afresh.
    if user.deleted_at.is_some() {
        users::reactivate(&mut *tx, user.id).await?;
        tracing::info!(user_id = %user.id, "deleted account signed in again");
    }
    let seal = |value: &str, column: &str| {
        keyring
            .seal(
                value.as_bytes(),
                format!("users.{column}:{}", user.id).as_bytes(),
            )
            .map_err(ApiError::internal)
    };
    let access = seal(tokens.access_token.expose(), "github_token")?;
    let refresh = tokens
        .refresh_token
        .as_ref()
        .map(|t| seal(t.expose(), "github_refresh_token"))
        .transpose()?;
    users::set_github_tokens(
        &mut *tx,
        user.id,
        &access,
        refresh.as_deref(),
        tokens.expires_at,
    )
    .await?;
    if let Some(email) = &identity.verified_email {
        users::set_email(
            &mut *tx,
            user.id,
            &seal(email, "email")?,
            EmailSource::Verified,
        )
        .await?;
    }
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.chars().take(256).collect::<String>());
    sessions::create(
        &mut *tx,
        user.id,
        SessionKind::Web,
        &session_token.hash,
        now + WEB_SESSION_TTL,
        user_agent.as_deref(),
    )
    .await?;
    tx.commit().await.map_err(ApiError::internal)?;
    tracing::info!(user_id = %user.id, "signed in with GitHub");

    let location = format!("{}{}", config.web_base_url(), pending.redirect);
    let mut response = Redirect::to(&location).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        cookies::set(
            cookies::SESSION,
            session_token.raw.expose(),
            "/",
            WEB_SESSION_TTL.whole_seconds(),
            config.secure_cookies(),
        ),
    );
    Ok(with_cleared_oauth_cookie(response, config.secure_cookies()))
}

fn with_cleared_oauth_cookie(mut response: Response, secure: bool) -> Response {
    response.headers_mut().append(
        header::SET_COOKIE,
        cookies::clear(cookies::OAUTH_STATE, OAUTH_COOKIE_PATH, secure),
    );
    response
}

fn state_mismatch() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "oauth_state_mismatch",
        "This sign-in link is invalid or expired. Start signing in again.",
    )
}

/// Only same-site relative paths are allowed after sign-in, so the login link can't be turned
/// into an open redirect (`//evil.example`, `https://…`, `/\evil`).
pub(crate) fn safe_redirect(requested: Option<&str>) -> &str {
    match requested {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && path.len() <= 512
                && !path.chars().any(char::is_control) =>
        {
            path
        }
        _ => DEFAULT_REDIRECT,
    }
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_relative_paths_are_followed() {
        assert_eq!(safe_redirect(Some("/claim/abc?x=1")), "/claim/abc?x=1");
        for bad in [
            None,
            Some(""),
            Some("dashboard"),
            Some("//evil.example"),
            Some("/\\evil.example"),
            Some("https://evil.example"),
            Some("/a\nSet-Cookie: x"),
        ] {
            assert_eq!(safe_redirect(bad), DEFAULT_REDIRECT, "{bad:?}");
        }
    }

    #[test]
    fn errors_are_url_encoded() {
        assert_eq!(urlencode("access_denied"), "access_denied");
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }
}

/// Extension tokens last a week; the extension asks the web app for a new one after that.
pub const EXTENSION_TOKEN_TTL: Duration = Duration::days(7);

/// Sign out of this browser.
#[utoipa::path(
    post,
    path = "/v1/auth/logout",
    operation_id = "logout",
    tag = "auth",
    responses((status = 204, description = "Signed out")),
    security(("session" = []))
)]
pub async fn logout(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = cookies::get(&headers, cookies::SESSION) {
        sessions::delete_by_hash(state.pool(), &mergetip_crypto::hash_token(token)).await?;
    }
    tracing::info!(user_id = %auth.user.id, "signed out");
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        cookies::clear(cookies::SESSION, "/", state.config().secure_cookies()),
    );
    Ok(response)
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ExtensionToken {
    /// Send as `Authorization: Bearer <token>`.
    pub token: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: time::OffsetDateTime,
}

/// Mint a bearer token for the browser extension.
#[utoipa::path(
    post,
    path = "/v1/auth/extension-token",
    operation_id = "create_extension_token",
    tag = "auth",
    responses((status = 200, body = ExtensionToken)),
    security(("session" = []))
)]
pub async fn extension_token(
    State(state): State<AppState>,
    SessionUser(auth): SessionUser,
) -> Result<axum::Json<ExtensionToken>, ApiError> {
    let token = random_token().map_err(ApiError::internal)?;
    let expires_at = state.clock().now() + EXTENSION_TOKEN_TTL;
    sessions::create(
        state.pool(),
        auth.user.id,
        SessionKind::Extension,
        &token.hash,
        expires_at,
        None,
    )
    .await?;
    Ok(axum::Json(ExtensionToken {
        token: token.raw.expose().clone(),
        expires_at,
    }))
}
