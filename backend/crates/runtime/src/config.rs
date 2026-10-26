//! Typed environment configuration. Each binary builds its own `Config` from these helpers and
//! fails at startup with a message naming the variable, rather than at first use.

use std::{fmt, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("environment variable {0} is required")]
    Missing(&'static str),
    #[error("environment variable {name} is invalid: {reason}")]
    Invalid { name: &'static str, reason: String },
}

/// Deployment environment. Production refuses unsafe settings (e.g. the stub payer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Dev,
    Staging,
    Production,
}

impl AppEnv {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    pub const fn is_dev(self) -> bool {
        matches!(self, Self::Dev)
    }
}

impl fmt::Display for AppEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AppEnv {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "dev" => Ok(Self::Dev),
            "staging" => Ok(Self::Staging),
            "production" => Ok(Self::Production),
            other => Err(format!(
                "expected dev, staging or production, got {other:?}"
            )),
        }
    }
}

/// Reads variables from a source. The process environment in production, a map in tests.
pub trait Env {
    fn get(&self, name: &str) -> Option<String>;

    /// The value, treating an empty string as unset (as `.env.example` leaves many blank).
    fn optional(&self, name: &str) -> Option<String> {
        self.get(name).filter(|v| !v.trim().is_empty())
    }

    fn required(&self, name: &'static str) -> Result<String, ConfigError> {
        self.optional(name).ok_or(ConfigError::Missing(name))
    }

    fn parse_or<T>(&self, name: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        match self.optional(name) {
            None => Ok(default),
            Some(raw) => raw
                .trim()
                .parse()
                .map_err(|err: T::Err| ConfigError::Invalid {
                    name,
                    reason: err.to_string(),
                }),
        }
    }
}

/// The default sender when `EMAIL_FROM` is unset.
pub const DEFAULT_EMAIL_FROM: &str = "MergeTip <tips@mail.mergetip.dev>";

/// Email delivery settings shared by the api (confirmation codes) and the worker (pledge
/// notices): `RESEND_API_KEY`, `SMTP_URL` and `EMAIL_FROM`. Plain SMTP has no TLS, so it is
/// refused outside dev.
pub fn mailer_config(
    env: &impl Env,
    app_env: AppEnv,
) -> Result<mergetip_notify::MailerConfig, ConfigError> {
    let smtp_url = env.optional("SMTP_URL");
    let resend_api_key = env
        .optional("RESEND_API_KEY")
        .map(mergetip_crypto::Redacted::new);
    if smtp_url.is_some() && resend_api_key.is_none() && !app_env.is_dev() {
        return Err(ConfigError::Invalid {
            name: "SMTP_URL",
            reason: "plain SMTP is for local Mailpit only; set RESEND_API_KEY outside dev".into(),
        });
    }
    Ok(mergetip_notify::MailerConfig {
        resend_api_key,
        smtp_url,
        from: env
            .optional("EMAIL_FROM")
            .unwrap_or_else(|| DEFAULT_EMAIL_FROM.into()),
    })
}

/// Fiat payouts through Bitnob (RFC 0004). `None` unless `FIAT_PAYOUTS_ENABLED=true`; then the
/// Bitnob credentials are required. The `fiat_payouts` flag row must also be on at runtime.
pub fn bitnob_config(env: &impl Env) -> Result<Option<mergetip_fiat::BitnobConfig>, ConfigError> {
    if !env.parse_or("FIAT_PAYOUTS_ENABLED", false)? {
        return Ok(None);
    }
    Ok(Some(mergetip_fiat::BitnobConfig {
        base_url: env.required("BITNOB_BASE_URL")?,
        client_id: env.required("BITNOB_CLIENT_ID")?,
        client_secret: mergetip_crypto::Redacted::new(env.required("BITNOB_SECRET")?),
        retry_backoff: std::time::Duration::from_millis(500),
    }))
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl<S: std::hash::BuildHasher> Env for std::collections::HashMap<&str, &str, S> {
    fn get(&self, name: &str) -> Option<String> {
        std::collections::HashMap::get(self, name).map(|v| (*v).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn empty_values_count_as_missing() {
        let env = HashMap::from([("MASTER_KEY", "  ")]);
        assert_eq!(
            env.required("MASTER_KEY"),
            Err(ConfigError::Missing("MASTER_KEY"))
        );
    }

    #[test]
    fn fiat_is_off_unless_enabled_and_then_needs_credentials() {
        let off = HashMap::from([("BITNOB_CLIENT_ID", "x")]);
        assert!(bitnob_config(&off).unwrap().is_none());
        let on = HashMap::from([("FIAT_PAYOUTS_ENABLED", "true")]);
        assert_eq!(
            bitnob_config(&on).map(|_| ()),
            Err(ConfigError::Missing("BITNOB_BASE_URL"))
        );
        let full = HashMap::from([
            ("FIAT_PAYOUTS_ENABLED", "true"),
            ("BITNOB_BASE_URL", "https://api.bitnob.com"),
            ("BITNOB_CLIENT_ID", "id"),
            ("BITNOB_SECRET", "secret"),
        ]);
        let config = bitnob_config(&full).unwrap().unwrap();
        assert!(!format!("{config:?}").contains("secret\""));
    }

    #[test]
    fn plain_smtp_is_for_dev_only() {
        let env = HashMap::from([("SMTP_URL", "smtp://localhost:1025")]);
        let config = mailer_config(&env, AppEnv::Dev).unwrap();
        assert_eq!(config.smtp_url.as_deref(), Some("smtp://localhost:1025"));
        assert_eq!(config.from, DEFAULT_EMAIL_FROM);
        assert!(mailer_config(&env, AppEnv::Production).is_err());
        let with_resend = HashMap::from([
            ("SMTP_URL", "smtp://localhost:1025"),
            ("RESEND_API_KEY", "re_x"),
        ]);
        assert!(mailer_config(&with_resend, AppEnv::Production).is_ok());
    }

    #[test]
    fn parse_or_uses_default_and_reports_bad_values() {
        let env = HashMap::from([("WORKER_CONCURRENCY", "eight"), ("APP_ENV", "staging")]);
        assert_eq!(env.parse_or("MISSING", 4_usize), Ok(4));
        assert_eq!(env.parse_or("APP_ENV", AppEnv::Dev), Ok(AppEnv::Staging));
        assert!(matches!(
            env.parse_or("WORKER_CONCURRENCY", 8_usize),
            Err(ConfigError::Invalid {
                name: "WORKER_CONCURRENCY",
                ..
            })
        ));
    }
}
