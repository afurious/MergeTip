use std::{path::PathBuf, sync::Arc};

use mergetip_crypto::{Keyring, Redacted};
use mergetip_runtime::{AppEnv, ConfigError, config::Env};

use crate::RunnerConfig;

#[derive(Debug)]
pub struct WorkerConfig {
    pub app_env: AppEnv,
    pub database_url: Redacted<String>,
    /// Built at startup so a bad `MASTER_KEY` stops the process before any job runs.
    pub keyring: Arc<Keyring>,
    pub runner: RunnerConfig,
    pub github: GithubAppConfig,
    /// The web app's base URL, for links in bot replies.
    pub web_base_url: String,
    /// Pay tips with a fake proof (staging demos only). Refused in production.
    pub stub_payer: bool,
    /// The Lightning network invoices must be for (`bitcoin` in production).
    pub lightning_network: mergetip_payer::Network,
    /// The API's public origin, for unsubscribe links in emails.
    pub api_base_url: String,
    /// How pledge notices are emailed.
    pub mailer: mergetip_notify::MailerConfig,
    /// Bitnob credentials when `FIAT_PAYOUTS_ENABLED=true` (RFC 0004).
    pub bitnob: Option<mergetip_fiat::BitnobConfig>,
    /// Prometheus `/metrics` on this internal address (`METRICS_ADDR`); unset disables it.
    pub metrics_addr: Option<std::net::SocketAddr>,
    /// Errors go to Sentry when set.
    pub sentry_dsn: Option<Redacted<String>>,
}

#[derive(Debug, Clone)]
pub struct GithubAppConfig {
    pub app_id: u64,
    pub private_key_path: PathBuf,
    /// Overrides `https://api.github.com` (GitHub Enterprise or a local mock).
    pub api_url: Option<String>,
}

impl WorkerConfig {
    pub fn from_env(env: &impl Env) -> Result<Self, ConfigError> {
        let app_env = env.parse_or("APP_ENV", AppEnv::Dev)?;
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

        let concurrency = env.parse_or("WORKER_CONCURRENCY", 8_usize)?;
        if concurrency == 0 {
            return Err(ConfigError::Invalid {
                name: "WORKER_CONCURRENCY",
                reason: "must be at least 1".into(),
            });
        }

        let stub_payer = env.parse_or("STUB_PAYER", false)?;
        if stub_payer && app_env == AppEnv::Production {
            return Err(ConfigError::Invalid {
                name: "STUB_PAYER",
                reason: "the stub payer fakes payments and cannot run in production".into(),
            });
        }

        Ok(Self {
            app_env,
            database_url: Redacted::new(env.required("DATABASE_URL")?),
            keyring: Arc::new(keyring),
            runner: RunnerConfig {
                concurrency,
                ..RunnerConfig::default()
            },
            github: GithubAppConfig {
                app_id: env.required("GITHUB_APP_ID")?.trim().parse().map_err(
                    |err: std::num::ParseIntError| ConfigError::Invalid {
                        name: "GITHUB_APP_ID",
                        reason: err.to_string(),
                    },
                )?,
                private_key_path: env.required("GITHUB_APP_PRIVATE_KEY_PATH")?.into(),
                api_url: env.optional("GITHUB_API_URL"),
            },
            web_base_url: env
                .required("PUBLIC_BASE_URL")?
                .trim_end_matches('/')
                .to_owned(),
            stub_payer,
            lightning_network: env
                .parse_or("LIGHTNING_NETWORK", mergetip_payer::Network::Bitcoin)?,
            api_base_url: env
                .required("API_PUBLIC_URL")?
                .trim_end_matches('/')
                .to_owned(),
            mailer: mergetip_runtime::config::mailer_config(env, app_env)?,
            bitnob: mergetip_runtime::config::bitnob_config(env)?,
            metrics_addr: env
                .optional("METRICS_ADDR")
                .map(|raw| {
                    raw.trim().parse().map_err(|_| ConfigError::Invalid {
                        name: "METRICS_ADDR",
                        reason: format!("{raw:?} is not host:port"),
                    })
                })
                .transpose()?,
            sentry_dsn: env.optional("SENTRY_DSN").map(Redacted::new),
        })
    }

    pub fn pool_size(&self) -> u32 {
        // One connection per job slot, plus the LISTEN connection and headroom for handlers
        // that open a transaction while holding another connection.
        u32::try_from(self.runner.concurrency)
            .unwrap_or(u32::MAX)
            .saturating_add(2)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn base() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("MASTER_KEY", KEY),
            ("DATABASE_URL", "postgres://x"),
            ("GITHUB_APP_ID", "12345"),
            ("GITHUB_APP_PRIVATE_KEY_PATH", "./.secrets/github-app.pem"),
            ("PUBLIC_BASE_URL", "http://localhost:5173/"),
            ("API_PUBLIC_URL", "http://localhost:8080"),
        ])
    }

    #[test]
    fn loads_with_defaults() {
        let env = base();
        let config = WorkerConfig::from_env(&env).unwrap();
        assert_eq!(config.app_env, AppEnv::Dev);
        assert_eq!(config.runner.concurrency, 8);
        assert_eq!(config.pool_size(), 10);
        assert_eq!(config.github.app_id, 12345);
        assert_eq!(config.web_base_url, "http://localhost:5173");
        assert!(!config.stub_payer);
        assert!(!format!("{config:?}").contains("postgres://x"));
    }

    #[test]
    fn stub_payer_is_refused_in_production() {
        let mut env = base();
        env.insert("STUB_PAYER", "true");
        env.insert("APP_ENV", "staging");
        assert!(WorkerConfig::from_env(&env).unwrap().stub_payer);
        env.insert("APP_ENV", "production");
        assert!(matches!(
            WorkerConfig::from_env(&env),
            Err(ConfigError::Invalid {
                name: "STUB_PAYER",
                ..
            })
        ));
    }

    #[test]
    fn github_app_id_is_required() {
        let mut env = base();
        env.remove("GITHUB_APP_ID");
        assert_eq!(
            WorkerConfig::from_env(&env).map(|_| ()),
            Err(ConfigError::Missing("GITHUB_APP_ID"))
        );
    }

    #[test]
    fn rejects_missing_or_bad_master_key() {
        let mut env = base();
        env.remove("MASTER_KEY");
        assert_eq!(
            WorkerConfig::from_env(&env).map(|_| ()),
            Err(ConfigError::Missing("MASTER_KEY"))
        );
        env.insert("MASTER_KEY", "abc");
        assert!(matches!(
            WorkerConfig::from_env(&env),
            Err(ConfigError::Invalid {
                name: "MASTER_KEY",
                ..
            })
        ));
    }

    #[test]
    fn rejects_zero_concurrency() {
        let mut env = base();
        env.insert("WORKER_CONCURRENCY", "0");
        assert!(matches!(
            WorkerConfig::from_env(&env),
            Err(ConfigError::Invalid {
                name: "WORKER_CONCURRENCY",
                ..
            })
        ));
    }
}
