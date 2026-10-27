//! Keeps LNURL requests on the public internet (SSRF). A receiver controls their Lightning
//! address's server, and through it the callback URL; without this they could point MergeTip's
//! worker at internal services. Two layers: IP-literal hosts are checked directly (they skip
//! DNS), and every name is resolved through [`PublicOnlyResolver`], which drops internal
//! addresses, so a public-looking name can't resolve to one either.

use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Whether `ip` is a public unicast address (not loopback, private, link-local, CGNAT,
/// unique-local, multicast, unspecified or broadcast).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(mapped));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// Whether a URL host may be contacted: a name (checked again at resolution), or a public IP.
pub fn host_allowed(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") || bare.ends_with(".localhost") {
        return false;
    }
    match bare.parse::<IpAddr>() {
        Ok(ip) => is_public(ip),
        Err(_) => true,
    }
}

/// Resolves names with the system resolver and keeps only public addresses.
#[derive(Debug, Clone, Copy, Default)]
pub struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let public: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| is_public(addr.ip()))
                .collect();
            if public.is_empty() {
                return Err(format!("{host} resolves only to non-public addresses").into());
            }
            let addrs: Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.3.4",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.5",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn hosts_are_checked_by_literal() {
        for host in [
            "localhost",
            "api.localhost",
            "10.0.0.5",
            "[::1]",
            "169.254.169.254",
        ] {
            assert!(!host_allowed(host), "{host}");
        }
        for host in ["getalby.com", "8.8.8.8", "[2606:4700:4700::1111]"] {
            assert!(host_allowed(host), "{host}");
        }
    }

    #[tokio::test]
    async fn names_that_resolve_internally_are_refused() {
        let resolved = PublicOnlyResolver
            .resolve("localhost".parse().unwrap())
            .await;
        assert!(resolved.is_err());
    }
}
