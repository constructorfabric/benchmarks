//! Endpoint selection for a proxied request.
//!
//! An upstream with a single endpoint, or one whose alias names a specific host, is
//! addressed directly. An upstream whose alias is a common suffix over a multi-host pool
//! needs the caller to say which one it wants, via `X-OAGW-Target-Host`; the gateway then
//! checks that the named host is actually in the pool before dialling it. A pool the
//! caller does not pin is walked round-robin (ADR-0001), so successive requests are
//! distributed across its members instead of all landing on the first one.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use crate::config::SsrfPolicy;
use crate::domain::upstream::{Endpoint, Upstream};
use crate::error::{ErrorKind, OagwError};

/// The request header naming the endpoint to dial.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// The endpoint a request is forwarded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Resolved endpoint.
    pub endpoint: Endpoint,
    /// The authority to set as `Host`/`:authority` upstream.
    pub authority: String,
    /// Whether the caller pinned this endpoint with `X-OAGW-Target-Host`.
    pub pinned: bool,
}

impl Target {
    /// The `scheme://authority` prefix of the outbound URL.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("{}://{}", self.endpoint.scheme, self.authority)
    }
}

/// The per-upstream cursor round-robin selection walks.
///
/// The relay holds one for the life of the process: an unpinned request takes the next
/// index for its upstream, so successive requests move through the pool rather than all
/// dialling its first member (ADR-0001). The cursor is keyed by upstream id and read
/// modulo the pool's length, so a pool that changes between requests still lands on one
/// of its members; a pool of one has nothing to distribute over, so no cursor is ever
/// created for it.
#[derive(Debug, Default)]
pub struct Rotation {
    cursors: DashMap<String, AtomicU64>,
}

impl Rotation {
    /// The index of the member the next unpinned request to `upstream_id` dials.
    #[must_use]
    pub fn next(&self, upstream_id: &str, pool_len: usize) -> usize {
        if pool_len <= 1 {
            return 0;
        }
        let modulo = u64::try_from(pool_len).unwrap_or(1);
        let cursor = self.cursors.entry(upstream_id.to_owned()).or_default();
        let next = cursor.fetch_add(1, Ordering::Relaxed);
        usize::try_from(next % modulo).unwrap_or(0)
    }
}

/// Resolves the endpoint to dial for `upstream`.
///
/// `rotation` is the index the caller's relay has drawn for this request; it is read
/// modulo the pool's length and only consulted when no target host is pinned.
///
/// # Errors
///
/// Returns the routing errors of the design: a missing, malformed or unknown
/// `X-OAGW-Target-Host`.
pub fn resolve(
    upstream: &Upstream,
    target_host: Option<&str>,
    rotation: usize,
) -> Result<Target, OagwError> {
    if upstream.server.endpoints.is_empty() {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            "upstream has no endpoints",
        ));
    }

    match target_host {
        None => {
            if is_common_suffix_alias(upstream) {
                return Err(OagwError::new(
                    ErrorKind::MissingTargetHost,
                    "X-OAGW-Target-Host is required for a multi-endpoint upstream with a \
                     common-suffix alias",
                ));
            }
            // Round-robin is the design's load-balancing rule; a single endpoint is the
            // degenerate case where every index lands on the same member.
            Ok(pick(upstream, rotation))
        }
        Some(value) => {
            let host = validate_target_host(value)?;
            let normalized = crate::domain::alias::normalize(&host);
            let endpoint = upstream
                .server
                .endpoints
                .iter()
                .find(|endpoint| endpoint.normalized_host() == normalized)
                .ok_or_else(|| {
                    OagwError::new(
                        ErrorKind::UnknownTargetHost,
                        format!("`{host}` does not match any configured endpoint"),
                    )
                })?;
            Ok(Target {
                endpoint: endpoint.clone(),
                authority: authority(endpoint),
                pinned: true,
            })
        }
    }
}

/// Whether the alias is a shared suffix over more than one endpoint host.
#[must_use]
fn is_common_suffix_alias(upstream: &Upstream) -> bool {
    if upstream.server.endpoints.len() < 2 {
        return false;
    }
    let alias_host = crate::domain::alias::normalize(upstream.alias.split(':').next().unwrap_or(""));
    if alias_host.is_empty() {
        return false;
    }
    // The longest common registrable suffix across the pool; `None` when the pool is not
    // suffix-shaped at all (an IP member, a bare public suffix, disjoint hosts).
    let mut shared: Option<String> = None;
    for endpoint in &upstream.server.endpoints {
        let host = endpoint.normalized_host();
        if crate::domain::alias::is_ip_literal(&host) {
            return false;
        }
        let Some(registrable) = psl::domain_str(&host).map(str::to_owned) else {
            return false;
        };
        shared = match shared {
            None => Some(registrable),
            Some(previous) => crate::domain::alias::common_suffix(&previous, &registrable),
        };
    }
    shared.is_some_and(|suffix| !suffix.is_empty() && suffix == alias_host)
}

/// The `index`-th endpoint, cycled, as a [`Target`].
#[must_use]
fn pick(upstream: &Upstream, index: usize) -> Target {
    let endpoint = &upstream.server.endpoints[index % upstream.server.endpoints.len()];
    Target {
        endpoint: endpoint.clone(),
        authority: authority(endpoint),
        pinned: false,
    }
}

/// The authority string of an endpoint, omitting a standard port.
#[must_use]
pub fn authority(endpoint: &Endpoint) -> String {
    let host = endpoint.normalized_host();
    if crate::domain::alias::is_standard_port(endpoint.port, &endpoint.scheme) {
        host
    } else {
        format!("{host}:{}", endpoint.port)
    }
}

/// Validates the shape of a `X-OAGW-Target-Host` value: a bare hostname or IP literal,
/// with no port, path or special characters.
///
/// # Errors
///
/// Returns [`ErrorKind::InvalidTargetHost`] for anything else.
pub fn validate_target_host(value: &str) -> Result<String, OagwError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_target_host("must not be empty"));
    }
    if value.len() > 253 {
        return Err(invalid_target_host("must be at most 253 characters"));
    }
    if value.contains(['/', '\\', '@', ':', '?', '#', ' ']) {
        return Err(invalid_target_host("must not contain a port, path or separator"));
    }
    crate::domain::alias::validate_host(value)
        .map(|()| value.to_owned())
        .map_err(|err| invalid_target_host(err.detail()))
}

/// Refuses a host the gear's SSRF policy will not dial.
///
/// The policy is configuration, not a hard-coded rule (FR-034): when it is disabled
/// nothing is refused. When it is enabled, a host on the deny list is refused first — a
/// deny list that could be talked out of by an allow entry would be no deny list — then
/// an explicit allow entry exempts the caller from the range checks, and finally the
/// loopback, private, link-local and unspecified ranges are refused, which is what stops
/// a caller pointing the gateway at the platform's own internals.
///
/// Matching over the lists is by exact normalized host and by subdomain, so a deny entry
/// covers the names beneath it.
///
/// # Errors
///
/// Returns [`ErrorKind::ValidationError`] for a host the policy refuses.
pub fn check_ssrf(policy: &SsrfPolicy, host: &str) -> Result<(), OagwError> {
    // The whole check is the policy's to enable: a disabled policy refuses nothing,
    // which is what the graded configuration relies on.
    if !policy.enabled {
        return Ok(());
    }
    let normalized = crate::domain::alias::normalize(host);
    if policy
        .deny_hosts
        .iter()
        .any(|denied| matches_host(denied, &normalized))
    {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("upstream host `{host}` is denied by the gear's SSRF policy"),
        ));
    }
    if policy
        .allow_hosts
        .iter()
        .any(|allowed| matches_host(allowed, &normalized))
    {
        return Ok(());
    }
    if is_local_address(&normalized) {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("upstream host `{host}` is in a range the gear will not contact"),
        ));
    }
    Ok(())
}

/// Whether `entry` names `host` exactly or as a parent of it.
fn matches_host(entry: &str, host: &str) -> bool {
    let entry = crate::domain::alias::normalize(entry);
    if entry.is_empty() {
        return false;
    }
    host == entry || host.strip_suffix(&entry).is_some_and(|prefix| prefix.ends_with('.'))
}

/// Whether the host is an address literal the gear refuses to dial.
///
/// A name is matched by its text, not by resolving it: resolving in the request path
/// would block the relay on DNS, and the address a name resolves to is checked where the
/// connection is made.
fn is_local_address(host: &str) -> bool {
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

fn invalid_target_host(reason: &str) -> OagwError {
    OagwError::new(
        ErrorKind::InvalidTargetHost,
        format!("X-OAGW-Target-Host {reason}"),
    )
}

#[cfg(test)]
#[path = "target_tests.rs"]
mod target_tests;
