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
