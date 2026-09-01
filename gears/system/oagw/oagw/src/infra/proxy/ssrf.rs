//! The SSRF enforcement point (`DESIGN` §4.4, security considerations).
//!
//! The posture comes from `oagw.config.ssrf_policy`. While the deployment runs
//! with `enabled: false` — as the local e2e manifest does, because its upstreams
//! are loopback services — the guard is a no-op: the endpoint selector already
//! refuses a cleartext upstream unless `allow_http_upstream` is set.
//! When it is enabled, a target must be reachable over TLS and must not name a
//! loopback, link-local, private or otherwise non-global address, so a stored
//! alias cannot be turned into a probe of the gateway's own network.

use std::net::IpAddr;

use crate::domain::error::DomainError;

/// SSRF posture for outbound calls, from `oagw.config.ssrf_policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SsrfGuard {
    enabled: bool,
}

impl SsrfGuard {
    /// A guard from the deployment configuration.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// The guard of a deployment with SSRF protection switched off.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { enabled: false }
    }

    /// `true` when the guard inspects outbound targets.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }

    /// Admit `host` as an outbound target.
    ///
    /// # Errors
    /// Returns [`DomainError::AccessDenied`] when the guard is enabled and the
    /// host is not a globally routable name, and
    /// [`DomainError::LinkUnavailable`] when the guard is enabled and the
    /// endpoint is not TLS.
    pub fn check(self, host: &str, requires_tls: bool) -> Result<(), DomainError> {
        if !self.enabled {
            return Ok(());
        }
        if !requires_tls {
            return Err(DomainError::LinkUnavailable {
                detail: "ssrf policy forbids a plaintext upstream".to_owned(),
                retry_after: None,
            });
        }
        if is_local_address(host) {
            return Err(DomainError::AccessDenied {
                detail: format!("outbound target '{host}' is in a protected address range"),
            });
        }
        Ok(())
    }
}

/// `true` for a host an SSRF guard must refuse: loopback, link-local,
/// site-local, the unspecified address, or the `.local`/`.localhost` suffixes.
fn is_local_address(host: &str) -> bool {
    let host = strip_port(host.trim());
    let lowered = host.to_ascii_lowercase();
    if lowered == "localhost" {
        return true;
    }
    match lowered.rsplit_once('.') {
        // `.local` and `.localhost` are mDNS/link-local names, never a global
        // target, whatever precedes them.
        Some((_, label)) if label == "localhost" || label == "local" => return true,
        _ => {}
    }
    lowered.parse::<IpAddr>().is_ok_and(|ip| match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_private()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
        }
    })
}

/// Drop a trailing `:port` from an authority, so a host written as
/// `10.0.0.1:8080` or `[::1]:8080` is still recognised. An IPv6 literal without
/// a port is returned unchanged.
fn strip_port(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((address, _)) => address,
            None => rest,
        };
    }
    if host.match_indices(':').count() == 1
        && host
            .split_once(':')
            .is_some_and(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
    {
        return host.split_once(':').map_or(host, |(address, _)| address);
    }
    host
}
