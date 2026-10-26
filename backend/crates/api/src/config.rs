use std::{net::SocketAddr, sync::Arc};

use axum::http::HeaderValue;
use mergetip_crypto::{Keyring, Redacted};
use mergetip_runtime::{AppEnv, ConfigError, config::Env};

#[derive(Debug)]
pub struct ApiConfig {
    pub app_env: AppEnv,
    pub addr: SocketAddr,
    /// The web app's origin (`PUBLIC_BASE_URL`): the only origin allowed by CORS, and the base
    /// for claim, confirm and settings links.
    pub web_origin: HeaderValue,
    pub database_url: Redacted<String>,
    pub keyring: Arc<Keyring>,
    /// Shared secret GitHub signs webhook deliveries with (`X-Hub-Signature-256`).
    pub github_webhook_secret: Redacted<String>,
    /// This API's public origin; GitHub redirects sign-ins back to it.
    pub api_public_url: String,
    /// The GitHub App's OAuth client credentials.
    pub github_client_id: String,
    pub github_client_secret: Redacted<String>,
    /// `https://github.com` unless overridden (GitHub Enterprise, tests).
    pub github_oauth_url: String,
    /// `https://api.github.com` unless overridden.
    pub github_api_url: String,
    /// Browser-extension origins allowed to make cookie-authenticated writes, besides the web app.
    pub extension_origins: Vec<HeaderValue>,
    /// The Lightning network invoices must be for (`bitcoin` in production).
    pub lightning_network: mergetip_payer::Network,
    /// How confirmation codes are emailed.
    pub mailer: mergetip_notify::MailerConfig,
    /// Svix secret (`whsec_…`) Resend signs delivery events with; unset disables that webhook.
    pub resend_webhook_secret: Option<Redacted<String>>,
}

impl ApiConfig {
    pub fn from_env(env: &impl Env) -> Result<Self, ConfigError> {
        let master_key = Redacted::new(env.required("MASTER_KEY")?);
        let previous = env.optional("MASTER_KEY_PREVIOUS").map(Redacted::new);
        let keyring = Keyring::from_hex(
            master_key.expose(),
            previous.as_ref().map(|k| k.expose().as_str()),
        )
        .map_err(|err| ConfigError::Invalid {
            name: "MASTER_KEY",
            reason: err.to_string(),
        })?;

        let app_env = env.parse_or("APP_ENV", AppEnv::Dev)?;
        Ok(Self {
            app_env,
            mailer: mergetip_runtime::config::mailer_config(env, app_env)?,
            resend_webhook_secret: env.optional("RESEND_WEBHOOK_SECRET").map(Redacted::new),
            addr: env.parse_or("API_ADDR", SocketAddr::from(([0, 0, 0, 0], 8080)))?,
            web_origin: parse_origin("PUBLIC_BASE_URL", &env.required("PUBLIC_BASE_URL")?)?,
            database_url: Redacted::new(env.required("DATABASE_URL")?),
            keyring: Arc::new(keyring),
            github_webhook_secret: Redacted::new(env.required("GITHUB_WEBHOOK_SECRET")?),
            api_public_url: parse_origin("API_PUBLIC_URL", &env.required("API_PUBLIC_URL")?)?
                .to_str()
                .unwrap_or_default()
                .to_owned(),
            github_client_id: env.required("GITHUB_CLIENT_ID")?,
            github_client_secret: Redacted::new(env.required("GITHUB_CLIENT_SECRET")?),
            github_oauth_url: env
                .optional("GITHUB_OAUTH_URL")
                .unwrap_or_else(|| "https://github.com".into()),
            github_api_url: env
                .optional("GITHUB_API_URL")
                .unwrap_or_else(|| "https://api.github.com".into()),
            extension_origins: env
                .optional("EXTENSION_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(|origin| {
                    HeaderValue::from_str(origin).map_err(|_| ConfigError::Invalid {
                        name: "EXTENSION_ORIGINS",
                        reason: format!("{origin:?} is not a valid origin"),
                    })
                })
                .collect::<Result<_, _>>()?,
            lightning_network: env
                .parse_or("LIGHTNING_NETWORK", mergetip_payer::Network::Bitcoin)?,
        })
    }

    /// Where GitHub sends the browser after sign-in. Must match the App's callback URL.
    pub fn oauth_callback_url(&self) -> String {
        format!("{}/v1/auth/github/callback", self.api_public_url)
    }

    /// Cookies get `Secure` everywhere except local development over plain HTTP.
    pub fn secure_cookies(&self) -> bool {
        !self.app_env.is_dev()
    }

    /// The web app base URL without a trailing slash, for building links.
    pub fn web_base_url(&self) -> &str {
        self.web_origin.to_str().unwrap_or_default()
    }
}

/// Accepts `scheme://host[:port]` (one trailing slash tolerated). A path would never match the
/// browser's `Origin` header, so CORS would silently fail; reject it at startup instead.
fn parse_origin(name: &'static str, raw: &str) -> Result<HeaderValue, ConfigError> {
    let invalid = |reason: &str| ConfigError::Invalid {
        name,
        reason: reason.to_owned(),
    };
    let origin = raw.trim().trim_end_matches('/');
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(|| invalid("must start with http:// or https://"))?;
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(invalid(
            "must be an origin like https://mergetip.dev, without a path",
        ));
    }
    HeaderValue::from_str(origin)
        .map_err(|_| invalid("contains characters not allowed in a header"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn env(base_url: &'static str) -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("MASTER_KEY", KEY),
            ("DATABASE_URL", "postgres://secret@db/mergetip"),
            ("PUBLIC_BASE_URL", base_url),
            ("GITHUB_WEBHOOK_SECRET", "hook-secret"),
            ("API_PUBLIC_URL", "http://localhost:8080"),
            ("GITHUB_CLIENT_ID", "Iv1.abc"),
            ("GITHUB_CLIENT_SECRET", "client-secret"),
        ])
    }

    #[test]
    fn loads_with_defaults() {
        let config = ApiConfig::from_env(&env("http://localhost:5173/")).unwrap();
        assert_eq!(config.addr, SocketAddr::from(([0, 0, 0, 0], 8080)));
        assert_eq!(config.web_base_url(), "http://localhost:5173");
        assert!(!format!("{config:?}").contains("secret@db"));
        assert!(!format!("{config:?}").contains("hook-secret"));
        assert!(!format!("{config:?}").contains("client-secret"));
        assert_eq!(
            config.oauth_callback_url(),
            "http://localhost:8080/v1/auth/github/callback"
        );
    }

    #[test]
    fn rejects_base_urls_that_are_not_origins() {
        for bad in [
            "localhost:5173",
            "https://mergetip.dev/app",
            "ftp://x",
            "https://",
        ] {
            assert!(
                matches!(
                    ApiConfig::from_env(&env(bad)),
                    Err(ConfigError::Invalid {
                        name: "PUBLIC_BASE_URL",
                        ..
                    })
                ),
                "{bad}"
            );
        }
    }
}
