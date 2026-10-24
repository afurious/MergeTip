use std::sync::Arc;

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

        Ok(Self {
            app_env,
            database_url: Redacted::new(env.required("DATABASE_URL")?),
            keyring: Arc::new(keyring),
            runner: RunnerConfig {
                concurrency,
                ..RunnerConfig::default()
            },
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

    #[test]
    fn loads_with_defaults() {
        let env = HashMap::from([("MASTER_KEY", KEY), ("DATABASE_URL", "postgres://x")]);
        let config = WorkerConfig::from_env(&env).unwrap();
        assert_eq!(config.app_env, AppEnv::Dev);
        assert_eq!(config.runner.concurrency, 8);
        assert_eq!(config.pool_size(), 10);
        assert!(!format!("{config:?}").contains("postgres://x"));
    }

    #[test]
    fn rejects_missing_or_bad_master_key() {
        let env = HashMap::from([("DATABASE_URL", "postgres://x")]);
        assert_eq!(
            WorkerConfig::from_env(&env).map(|_| ()),
            Err(ConfigError::Missing("MASTER_KEY"))
        );
        let env = HashMap::from([("DATABASE_URL", "postgres://x"), ("MASTER_KEY", "abc")]);
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
        let env = HashMap::from([
            ("MASTER_KEY", KEY),
            ("DATABASE_URL", "postgres://x"),
            ("WORKER_CONCURRENCY", "0"),
        ]);
        assert!(matches!(
            WorkerConfig::from_env(&env),
            Err(ConfigError::Invalid {
                name: "WORKER_CONCURRENCY",
                ..
            })
        ));
    }
}
