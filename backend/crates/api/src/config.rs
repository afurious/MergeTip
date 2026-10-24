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

        Ok(Self {
            app_env: env.parse_or("APP_ENV", AppEnv::Dev)?,
            addr: env.parse_or("API_ADDR", SocketAddr::from(([0, 0, 0, 0], 8080)))?,
            web_origin: parse_origin("PUBLIC_BASE_URL", &env.required("PUBLIC_BASE_URL")?)?,
            database_url: Redacted::new(env.required("DATABASE_URL")?),
            keyring: Arc::new(keyring),
        })
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
        ])
    }

    #[test]
    fn loads_with_defaults() {
        let config = ApiConfig::from_env(&env("http://localhost:5173/")).unwrap();
        assert_eq!(config.addr, SocketAddr::from(([0, 0, 0, 0], 8080)));
        assert_eq!(config.web_base_url(), "http://localhost:5173");
        assert!(!format!("{config:?}").contains("secret@db"));
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
