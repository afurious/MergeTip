//! Signing users in with the GitHub App's user-to-server OAuth flow (FR-6.1).
//!
//! Behind [`GitHubOAuth`] so the API can be tested without GitHub. The real client talks to
//! `github.com` (authorize, token exchange) and `api.github.com` (who the user is); both base
//! URLs are configurable for tests and GitHub Enterprise.

use std::time::Duration;

use async_trait::async_trait;
use mergetip_crypto::Redacted;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::{Account, PostedComment, RepoName};

/// Tokens for acting as the user. GitHub App user tokens expire (8 h by default) and come with a
/// refresh token, unless the App opted out of expiring tokens.
#[derive(Debug)]
pub struct UserTokens {
    pub access_token: Redacted<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub refresh_token: Option<Redacted<String>>,
}

/// Who signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub account: Account,
    /// The primary email, only if GitHub has verified it.
    pub verified_email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    /// GitHub refused the code or refresh token (expired, reused, wrong client).
    #[error("GitHub rejected the authorization: {0}")]
    Rejected(String),
    #[error("GitHub request failed: {0}")]
    Transport(String),
    #[error("unexpected GitHub response: {0}")]
    Decode(String),
    /// The user may not do this (no access to the repository, or the thread is locked).
    #[error("GitHub refused the request: {0}")]
    Forbidden(String),
}

#[async_trait]
pub trait GitHubOAuth: Send + Sync {
    /// Where to send the browser to start signing in.
    fn authorize_url(&self, state: &str, redirect_uri: &str) -> String;

    async fn exchange_code(&self, code: &str, redirect_uri: &str)
    -> Result<UserTokens, OAuthError>;

    async fn refresh(&self, refresh_token: &str) -> Result<UserTokens, OAuthError>;

    async fn identity(&self, access_token: &str) -> Result<Identity, OAuthError>;

    /// Posts a comment as the user (the extension's `/tip`). GitHub records the user, not the
    /// App, as its author, so the webhook that follows carries the real sender.
    async fn create_comment(
        &self,
        access_token: &str,
        repo: &RepoName,
        number: u32,
        body: &str,
    ) -> Result<PostedComment, OAuthError>;
}

pub struct OAuthClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: Redacted<String>,
    oauth_base: String,
    api_base: String,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field("oauth_base", &self.oauth_base)
            .finish_non_exhaustive()
    }
}

impl OAuthClient {
    pub fn new(
        client_id: String,
        client_secret: Redacted<String>,
        oauth_base: &str,
        api_base: &str,
    ) -> Result<Self, OAuthError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mergetip/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|err| OAuthError::Transport(err.to_string()))?;
        Ok(Self {
            http,
            client_id,
            client_secret,
            oauth_base: oauth_base.trim_end_matches('/').to_owned(),
            api_base: api_base.trim_end_matches('/').to_owned(),
        })
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<UserTokens, OAuthError> {
        let response: TokenResponse = self
            .http
            .post(format!("{}/login/oauth/access_token", self.oauth_base))
            .header("accept", "application/json")
            .form(form)
            .send()
            .await
            .map_err(transport)?
            .error_for_status()
            .map_err(transport)?
            .json()
            .await
            .map_err(|err| OAuthError::Decode(err.to_string()))?;
        // GitHub reports a bad code as HTTP 200 with an `error` field.
        if let Some(error) = response.error {
            return Err(OAuthError::Rejected(error));
        }
        let access_token = response
            .access_token
            .ok_or_else(|| OAuthError::Decode("token response without access_token".into()))?;
        let now = OffsetDateTime::now_utc();
        Ok(UserTokens {
            access_token: Redacted::new(access_token),
            expires_at: response
                .expires_in
                .map(|secs| now + time::Duration::seconds(secs)),
            refresh_token: response.refresh_token.map(Redacted::new),
        })
    }

    async fn api_get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        access_token: &str,
    ) -> Result<Option<T>, OAuthError> {
        let response = self
            .http
            .get(format!("{}{path}", self.api_base))
            .header("accept", "application/vnd.github+json")
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(OAuthError::Rejected("access token not accepted".into()));
        }
        // Missing permission (e.g. email addresses not granted): treat as "not available".
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value = response
            .error_for_status()
            .map_err(transport)?
            .json()
            .await
            .map_err(|err| OAuthError::Decode(err.to_string()))?;
        Ok(Some(value))
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Email {
    email: String,
    primary: bool,
    verified: bool,
}

fn transport(err: reqwest::Error) -> OAuthError {
    OAuthError::Transport(err.to_string())
}

#[async_trait]
impl GitHubOAuth for OAuthClient {
    fn authorize_url(&self, state: &str, redirect_uri: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/login/oauth/authorize", self.oauth_base))
            .expect("the OAuth base URL was validated at startup");
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state);
        url.into()
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<UserTokens, OAuthError> {
        self.token_request(&[
            ("client_id", &self.client_id),
            ("client_secret", self.client_secret.expose()),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<UserTokens, OAuthError> {
        self.token_request(&[
            ("client_id", &self.client_id),
            ("client_secret", self.client_secret.expose()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .await
    }

    async fn identity(&self, access_token: &str) -> Result<Identity, OAuthError> {
        let account: Account = self
            .api_get("/user", access_token)
            .await?
            .ok_or_else(|| OAuthError::Decode("GET /user returned no account".into()))?;
        let emails: Vec<Email> = self
            .api_get("/user/emails", access_token)
            .await?
            .unwrap_or_default();
        let verified_email = emails
            .into_iter()
            .find(|e| e.primary && e.verified)
            .map(|e| e.email);
        Ok(Identity {
            account,
            verified_email,
        })
    }

    async fn create_comment(
        &self,
        access_token: &str,
        repo: &RepoName,
        number: u32,
        body: &str,
    ) -> Result<PostedComment, OAuthError> {
        let response = self
            .http
            .post(format!(
                "{}/repos/{}/{}/issues/{number}/comments",
                self.api_base,
                repo.owner(),
                repo.name()
            ))
            .header("accept", "application/vnd.github+json")
            .bearer_auth(access_token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(transport)?;
        match response.status() {
            status if status.is_success() => response
                .json()
                .await
                .map_err(|err| OAuthError::Decode(err.to_string())),
            reqwest::StatusCode::UNAUTHORIZED => {
                Err(OAuthError::Rejected("access token not accepted".into()))
            }
            status @ (reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::GONE
            | reqwest::StatusCode::UNPROCESSABLE_ENTITY) => {
                Err(OAuthError::Forbidden(status.to_string()))
            }
            status => Err(OAuthError::Transport(status.to_string())),
        }
    }
}
