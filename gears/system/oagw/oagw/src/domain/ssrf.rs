//! SSRF protection guard (`cpt-cf-oagw-principle-ssrf-defense-in-depth`).
//!
//! Blocks proxy targets that resolve to private/loopback/link-local address
//! space and hostnames outside the public DNS suffix tree (fail-closed).

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use psl::suffix_str;

use crate::domain::error::DomainError;

/// Evaluates a target host for SSRF eligibility.
#[derive(Debug, Clone, Copy)]
pub struct SsrfGuard {
    enabled: bool,
}

impl SsrfGuard {
    /// Builds the guard from the config policy.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Checks whether `host` may be proxied to.
    ///
    /// On success returns the validated addresses for `host` (empty when the
    /// guard is disabled) so the caller can pin the upstream connection to an
    /// already-validated address instead of re-resolving the hostname
    /// (DNS-rebind TOCTOU hardening).
    ///
    /// # Errors
    ///
    /// Returns `SsrfBlocked` when the host is a literal private address or
    /// resolves to private address space, or when its name is not a public
    /// DNS suffix (internal/private hostname), or name resolution fails
    /// (fail-closed under the defense-in-depth principle).
    pub fn check_host(&self, host: &str) -> Result<Vec<IpAddr>, DomainError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        let host = host.trim();
        if host.is_empty() {
            return Err(DomainError::SsrfBlocked("<empty host>".to_owned()));
        }

        // Literal IP: check address space directly (no DNS).
        if let Ok(ip) = host.parse::<IpAddr>() {
            Self::check_ip(ip).map_err(|_| DomainError::SsrfBlocked(host.to_owned()))?;
            return Ok(vec![ip]);
        }

        // Hostname: must sit under a public DNS suffix (deny .internal,
        // .local, bare names, etc. — mirrors the `psl` public-suffix check).
        if suffix_str(host).is_none() {
            return Err(DomainError::SsrfBlocked(host.to_owned()));
        }

        // Resolve and require ALL records to be public, non-private
        // addresses (a mixed private+public A/AAAA set is denied wholesale,
        // not passed on the strength of a single public record).
        let addrs: Vec<SocketAddr> = (host, 0)
            .to_socket_addrs()
            .map(|it| it.collect())
            .map_err(|_| DomainError::SsrfBlocked(host.to_owned()))?;
        let mut validated = Vec::with_capacity(addrs.len());
        for addr in addrs {
            Self::check_ip(addr.ip()).map_err(|_| DomainError::SsrfBlocked(host.to_owned()))?;
            validated.push(addr.ip());
        }
        if validated.is_empty() {
            return Err(DomainError::SsrfBlocked(host.to_owned()));
        }
        Ok(validated)
    }

    fn check_ip(ip: IpAddr) -> Result<(), ()> {
        let denied = match ip {
            IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_unspecified()
                    || v4.is_multicast()
                    || v4.is_documentation()
            }
            IpAddr::V6(v6) => {
                // IPv4-mapped IPv6 (`::ffff:a.b.c.d`) must be judged by its
                // embedded IPv4: a mapped private/link-local literal would
                // otherwise slip past the guard.
                let mapped_ipv4_denied = v6
                    .to_ipv4_mapped()
                    .map(|v4| {
                        v4.is_private()
                            || v4.is_loopback()
                            || v4.is_link_local()
                            || v4.is_broadcast()
                            || v4.is_unspecified()
                            || v4.is_multicast()
                            || v4.is_documentation()
                    })
                    .unwrap_or(false);
                mapped_ipv4_denied
                    || v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
            }
        };
        if denied { Err(()) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_guard_passes() {
        let guard = SsrfGuard::new(false);
        assert!(guard.check_host("127.0.0.1").is_ok());
        assert!(guard.check_host("internal.host").is_ok());
    }

    #[test]
    fn blocks_private_literals() {
        let guard = SsrfGuard::new(true);
        for host in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(guard.check_host(host).is_err(), "{host} must be blocked");
        }
    }

    #[test]
    fn allows_public_literals() {
        let guard = SsrfGuard::new(true);
        assert!(guard.check_host("8.8.8.8").is_ok());
        assert!(guard.check_host("1.1.1.1").is_ok());
    }

    /// IPv4-mapped IPv6 literals must be judged by their embedded IPv4
    /// (`::ffff:169.254.169.254` is the classic SSRF bypass).
    #[test]
    fn blocks_ipv4_mapped_ipv6_private_literals() {
        let guard = SsrfGuard::new(true);
        for host in [
            "::ffff:169.254.169.254",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.9",
            "::ffff:192.168.0.1",
        ] {
            assert!(guard.check_host(host).is_err(), "{host} must be blocked");
        }
        assert!(guard.check_host("::ffff:8.8.8.8").is_ok());
    }

    /// `check_host` returns the validated address set for connect pinning:
    /// the enabled guard pins a single public literal, while the disabled
    /// guard pinning is a no-op (empty set → caller falls back to hostname).
    /// The all-records-must-pass semantics of the hostname path is enforced
    /// by fail-fast denial inside `check_host` (a mixed private+public set is
    /// never passed on the strength of one public record).
    #[test]
    fn check_host_returns_pinned_addresses() {
        let guard = SsrfGuard::new(true);
        let pinned = guard.check_host("8.8.8.8").expect("ok");
        assert_eq!(pinned, vec!["8.8.8.8".parse::<IpAddr>().expect("ip")]);

        let disabled = SsrfGuard::new(false);
        assert_eq!(
            disabled.check_host("8.8.8.8").expect("ok"),
            Vec::<IpAddr>::new(),
            "disabled guard must not pin"
        );
    }

    #[test]
    fn blocks_private_hostnames_without_dns() {
        // `localhost` and `internal` are not public DNS suffixes, so the
        // guard denies them without ever hitting DNS.
        let guard = SsrfGuard::new(true);
        assert!(guard.check_host("localhost").is_err());
        assert!(guard.check_host("meta.internal").is_err());
    }
}
