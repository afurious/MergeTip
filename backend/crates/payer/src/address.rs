use std::{fmt, str::FromStr};

use crate::ResolveError;

/// A LUD-16 Lightning address, `user@domain`, normalised to lower case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LightningAddress {
    user: String,
    domain: String,
}

impl LightningAddress {
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Host, with an optional `:port` (used by local mock servers).
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Hosts that may be reached over plain HTTP when local mocks are enabled: loopback and
    /// single-label names (Docker Compose service names). Never a public domain.
    pub(crate) fn is_local(&self) -> bool {
        let host = self.domain.split(':').next().unwrap_or_default();
        host == "localhost" || host == "127.0.0.1" || !host.contains('.')
    }
}

impl fmt::Display for LightningAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.user, self.domain)
    }
}

impl FromStr for LightningAddress {
    type Err = ResolveError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || ResolveError::InvalidAddress(s.chars().take(100).collect());
        let lower = s.trim().to_ascii_lowercase();
        let (user, domain) = lower.split_once('@').ok_or_else(invalid)?;
        let (host, port) = match domain.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (domain, None),
        };
        let user_ok = !user.is_empty()
            && user.len() <= 64
            && user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'));
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && !host.starts_with(['.', '-'])
            && !host.ends_with(['.', '-'])
            && !host.contains("..")
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
        let port_ok = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0));
        if user_ok && host_ok && port_ok {
            Ok(Self {
                user: user.to_owned(),
                domain: domain.to_owned(),
            })
        } else {
            Err(invalid())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalises() {
        let address: LightningAddress = " Alice@Getalby.com ".parse().unwrap();
        assert_eq!(address.to_string(), "alice@getalby.com");
        assert!(!address.is_local());
        let local: LightningAddress = "bob@mock-lnurl:8090".parse().unwrap();
        assert!(local.is_local());
        assert!(
            "x@127.0.0.1:9"
                .parse::<LightningAddress>()
                .unwrap()
                .is_local()
        );
    }

    #[test]
    fn rejects_malformed_addresses() {
        for bad in [
            "",
            "alice",
            "@getalby.com",
            "alice@",
            "a b@getalby.com",
            "alice@getalby..com/x",
            "alice@-getalby.com",
            "alice@getalby.com:0",
            "alice@getalby.com:99999",
            "alice@evil.com/../x",
        ] {
            assert!(bad.parse::<LightningAddress>().is_err(), "{bad:?}");
        }
    }
}
