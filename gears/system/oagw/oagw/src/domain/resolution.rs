//! Data-plane target resolution (ADR-0001, DESIGN "Alias Resolution").
//!
//! One call turns a proxy request (`alias`, `method`, `path_suffix`,
//! `X-OAGW-Target-Host`) into the concrete [`ResolvedTarget`] the proxy needs:
//! the upstream, the endpoint to dial and the route that served the request.
//! Everything that can reject a request before it is forwarded is decided here,
//! so the transport layer never re-implements a routing rule:
//!
//! 1. **alias lookup** — normalized (ASCII lowercase, trailing dots stripped)
//!    and tenant-scoped. An unknown alias is a 404 `route.not_found.v1` with
//!    the `alias` extension; a present but disabled upstream is a 503
//!    `link.unavailable.v1`.
//! 2. **route match** — only enabled routes of the owning upstream take part.
//!    HTTP upstreams match by method allowlist plus longest path prefix; gRPC
//!    matching is Phase 3 (DESIGN "Proxy API"), so a gRPC route never serves an
//!    HTTP proxy request. No match is a 404 `route.not_found.v1` with the
//!    `upstream_id` extension.
//! 3. **`X-OAGW-Target-Host` matrix** (ADR-0001 Appendix A) — see
//!    [`select_endpoint`].
//! 4. **SSRF policy** — when `gears.oagw.config.ssrf_policy.enabled`, an
//!    endpoint whose host is a literal loopback, private, link-local or
//!    unique-local IP address (v4 and v6, including IPv4-mapped IPv6) is
//!    rejected. A **hostname** endpoint is not inspected at all: the hot path
//!    performs no DNS lookups, so its addresses are only ever known to the
//!    connector, which resolves and dials it directly. Resolving a hostname and
//!    pinning (or rejecting) the addresses it maps to is a separate concern —
//!    DESIGN "Out of Scope" lists "DNS resolution / IP pinning rules" as such —
//!    and is **not** implemented by this gear: a tenant may point an upstream at
//!    a hostname that resolves inside a private segment and the policy will not
//!    stop it. Operators who need that guarantee keep `ssrf_policy.enabled` and
//!    only allow IP-literal endpoints.
//! 5. **scheme gating** — `http` is a legal create-time endpoint scheme, but
//!    the data plane only dials plaintext upstreams when
//!    `gears.oagw.config.allow_http_upstream` is set; otherwise the resolution
//!    fails with 503 `link.unavailable.v1`.
//!
//! # Tenant scope
//!
//! Resolution is **strictly single-tenant**: only the calling tenant's own
//! upstreams are visible, and an ancestor's upstream is an unknown alias. DESIGN
//! "Shadowing Behavior" specifies a descendant-to-root walk with closest-match
//! shadowing and `sharing: enforce` propagation; that walk needs the tenant
//! hierarchy, which the in-memory control plane does not hold and which
//! [`SecurityContext`](toolkit_security::SecurityContext) does not expose
//! (there is no parent chain on the subject). The walk is therefore **not**
//! implemented: recording it here rather than leaving the design claim
//! unqualified. A management-side tenant still never sees another tenant's
//! upstreams, so the gap only widens what a tenant *cannot* reach.
//!
//! # Path matching
//! Let `P` be the route's `match.http.path` (always starts with `/`) and `S`
//! the proxy suffix: the part of the proxy URL that follows
//! `/oagw/v1/proxy/{alias}`, re-normalized to a `/`-prefix (empty when the
//! proxy URL carries no suffix at all). A route matches when
//!
//! * `S == P`, or
//! * `S` starts with `P + "/"` (segment boundaries are respected:
//!   `/v1/chat` must **not** match `/v1/chatfoo`), or
//! * `P == "/"` and `path_suffix_mode` is `append`.
//!
//! A route whose `path_suffix_mode` is `disabled` only matches an `S` that is
//! exactly its own prefix: a suffix that goes beyond the prefix is rejected.
//! The longest matching `P` wins; ties are impossible because the control plane
//! refuses two enabled routes of one upstream with the same prefix and an
//! overlapping method set.

use std::net::IpAddr;

use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::control_plane::{ControlPlane, RouteRecord, UpstreamRecord};
use url::Url;

use crate::domain::model::{
    DerivedAlias, Endpoint, EndpointScheme, PathSuffixMode, derive_alias, normalize_alias,
    normalize_host, upstream_gts_id,
};
use crate::error::OagwError;

/// The outcome of a successful resolution: the upstream, the endpoint to dial
/// and the route that served the request.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTarget {
    /// Upstream the alias resolved to.
    pub upstream: UpstreamRecord,
    /// Endpoint of the upstream's pool the request is forwarded to.
    pub endpoint: Endpoint,
    /// Enabled route that matched the method and path suffix.
    pub route: RouteRecord,
}

/// Resolve the proxy target of a request (see the [module
/// documentation](self)).
///
/// # Errors
///
/// Returns a gateway [`OagwError`] carrying the documented GTS type for every
/// rejection: 404 for an unknown alias or a missing route, 503 for a disabled
/// upstream or a gated scheme, 400 for a malformed or missing
/// `X-OAGW-Target-Host` or an SSRF-blocked endpoint host.
pub fn resolve_proxy_target(
    plane: &ControlPlane,
    config: &OagwConfig,
    tenant_id: Uuid,
    alias: &str,
    method: &str,
    path_suffix: &str,
    target_host: Option<&str>,
) -> Result<ResolvedTarget, OagwError> {
    let normalized = normalize_alias(alias);
    let upstream = plane
        .find_upstream_by_alias(tenant_id, &normalized)
        .ok_or_else(|| unknown_alias(&normalized))?;
    if !upstream.spec.enabled {
        return Err(disabled_upstream(&normalized));
    }

    reject_dot_segments(path_suffix)?;
    let route = match_route(plane, tenant_id, &upstream, method, path_suffix)?;
    let endpoint = select_endpoint(
        plane,
        upstream_id_of(&upstream),
        &upstream,
        &normalized,
        target_host,
    )?;
    enforce_ssrf_policy(config, &endpoint)?;
    enforce_scheme_policy(config, &endpoint)?;

    Ok(ResolvedTarget {
        upstream,
        endpoint,
        route,
    })
}

/// The identifier of an upstream, for the problem-document extensions.
fn upstream_id_of(upstream: &UpstreamRecord) -> String {
    upstream_gts_id(upstream.id)
}

/// 400 problem for a proxy suffix that carries a `.` or `..` path segment.
///
/// The suffix is matched against the routes and forwarded **verbatim**, so a
/// dot-segment would survive to the upstream, which normalises it: a request
/// routed as `/v1/../admin` would arrive at `/admin`, a path no route of the
/// upstream matched. Only the exact spellings are rejected — a segment that
/// merely contains a dot (`.well-known`, `v1.2`) is a normal segment, and
/// percent-encoded bytes stay opaque because the comparison is on the raw
/// suffix.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the suffix carries such a segment.
fn reject_dot_segments(path_suffix: &str) -> Result<(), OagwError> {
    if path_suffix
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(OagwError::validation(format!(
            "the proxy path `{path_suffix}` carries a `.` or `..` segment; the gateway forwards \
             the path verbatim and would otherwise hand the upstream a path no route matched"
        )));
    }
    Ok(())
}

/// 404 problem for an alias the tenant does not own.
fn unknown_alias(alias: &str) -> OagwError {
    OagwError::route_not_found(format!(
        "no upstream with alias `{alias}` exists for this tenant"
    ))
    .with_alias(alias.to_owned())
}

/// 503 problem for a disabled upstream.
fn disabled_upstream(alias: &str) -> OagwError {
    OagwError::link_unavailable(format!(
        "upstream `{alias}` is disabled and does not accept traffic"
    ))
    .with_alias(alias.to_owned())
}

/// 404 problem for a request no enabled route of `upstream` serves.
fn no_matching_route(upstream: &UpstreamRecord, method: &str, suffix: &str) -> OagwError {
    OagwError::route_not_found(format!(
        "no enabled route of upstream `{}` matches `{method}` for path suffix `{suffix}`",
        upstream.alias
    ))
    .with_upstream_id(upstream_id_of(upstream))
    .with_alias(upstream.alias.clone())
}

/// The enabled route of `upstream` that serves `method` and `path_suffix`.
///
/// Candidates come back longest-path-first from the control plane, so the first
/// match is the longest match.
fn match_route(
    plane: &ControlPlane,
    tenant_id: Uuid,
    upstream: &UpstreamRecord,
    method: &str,
    path_suffix: &str,
) -> Result<RouteRecord, OagwError> {
    plane
        .enabled_routes_for_upstream(tenant_id, upstream.id)
        .into_iter()
        .find(|route| route_serves(route, method, path_suffix))
        .ok_or_else(|| no_matching_route(upstream, method, path_suffix))
}

/// Whether `route` serves `method` and `path_suffix`.
fn route_serves(route: &RouteRecord, method: &str, path_suffix: &str) -> bool {
    let Some(http) = route.spec.match_rules.http() else {
        // gRPC matching is Phase 3: a gRPC route never serves an HTTP proxy
        // request (DESIGN "Proxy API").
        return false;
    };
    http.accepts(method) && path_matches(&http.path, http.path_suffix_mode, path_suffix)
}

/// Whether a proxy suffix `suffix` is served by a route whose match path is
/// `prefix` under `mode` (see the [module documentation](self)).
#[must_use]
pub fn path_matches(prefix: &str, mode: PathSuffixMode, suffix: &str) -> bool {
    if mode == PathSuffixMode::Disabled {
        // A disabled suffix rejects anything beyond the route's own prefix.
        return suffix == prefix;
    }
    if prefix == "/" {
        return true;
    }
    suffix == prefix || suffix.starts_with(&format!("{prefix}/"))
}

/// Pick the endpoint of `upstream` the request is dialed to, following the
/// `X-OAGW-Target-Host` behavior matrix of ADR-0001 Appendix A:
///
/// | Endpoints | Alias | Header | Behaviour |
/// |---|---|---|---|
/// | single | any | absent | that endpoint |
/// | single | any | present | validated against the endpoint set, then used |
/// | multiple | explicit | absent | round-robin |
/// | multiple | explicit | present | that endpoint |
/// | multiple | common suffix | absent | 400 `missing_target_host` |
/// | multiple | common suffix | present | that endpoint |
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the header is not a bare hostname or IP
/// (`invalid_target_host`), names no configured endpoint
/// (`unknown_target_host`, with `valid_hosts`) or is required but absent
/// (`missing_target_host`, with `alias` and `valid_hosts`).
pub fn select_endpoint(
    plane: &ControlPlane,
    upstream_id: String,
    upstream: &UpstreamRecord,
    alias: &str,
    target_host: Option<&str>,
) -> Result<Endpoint, OagwError> {
    let endpoints = &upstream.spec.server.endpoints;
    let Some(value) = target_host else {
        return endpoint_without_header(plane, upstream, alias);
    };
    let requested = parse_target_host(value)?;
    endpoints
        .iter()
        .find(|endpoint| normalize_host(&endpoint.host) == requested)
        .cloned()
        .ok_or_else(|| {
            OagwError::unknown_target_host(format!(
                "`X-OAGW-Target-Host` value `{requested}` does not match a configured endpoint of \
                 `{alias}`"
            ))
            .with_alias(alias.to_owned())
            .with_upstream_id(upstream_id)
            .with_valid_hosts(endpoint_hosts(endpoints))
        })
}

/// Endpoint selection when no `X-OAGW-Target-Host` was supplied.
fn endpoint_without_header(
    plane: &ControlPlane,
    upstream: &UpstreamRecord,
    alias: &str,
) -> Result<Endpoint, OagwError> {
    let endpoints = &upstream.spec.server.endpoints;
    let Some((first, rest)) = endpoints.split_first() else {
        return Err(OagwError::internal(format!(
            "upstream `{alias}` was stored without an endpoint"
        )));
    };
    if rest.is_empty() {
        return Ok(first.clone());
    }
    if common_suffix_alias(upstream) {
        return Err(OagwError::missing_target_host(format!(
            "`X-OAGW-Target-Host` is required for upstream `{alias}`: its alias is the common \
             suffix of {} endpoints",
            endpoints.len()
        ))
        .with_alias(alias.to_owned())
        .with_valid_hosts(endpoint_hosts(endpoints)));
    }
    let index = plane.next_endpoint_index(upstream.id, endpoints.len());
    Ok(endpoints[index].clone())
}

/// Whether `upstream`'s alias is the common domain suffix of its endpoint pool
/// (`us.vendor.com` + `eu.vendor.com` → `vendor.com`), the case ADR-0001 makes
/// the header mandatory.
///
/// The test is the derivation itself, not a string-suffix scan: the header is
/// mandatory exactly when the pool is the one the alias was *derived from*. A
/// pool that had to be given an explicit alias — IP-addressed hosts, hosts
/// without a shared registrable suffix — is round-robined instead, because the
/// header could not discriminate between endpoints that share a host.
fn common_suffix_alias(upstream: &UpstreamRecord) -> bool {
    let endpoints = &upstream.spec.server.endpoints;
    if endpoints.len() < 2 {
        return false;
    }
    matches!(
        derive_alias(endpoints),
        DerivedAlias::Derived(derived) if normalize_alias(&derived) == upstream.alias
    )
}

/// The normalized host names of an endpoint set, for the `valid_hosts` extension.
fn endpoint_hosts(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints
        .iter()
        .map(|endpoint| normalize_host(&endpoint.host))
        .collect()
}

/// Validate a `X-OAGW-Target-Host` value and return it in canonical form.
///
/// A bare hostname or IP address only: a port, path, scheme, query or any
/// whitespace makes the value invalid, because the header selects an endpoint of
/// the upstream and never overrides its port or scheme.
///
/// # Errors
///
/// Returns a 400 [`OagwError::invalid_target_host`] for every other shape.
pub fn parse_target_host(value: &str) -> Result<String, OagwError> {
    let invalid = || {
        OagwError::invalid_target_host(format!(
            "`X-OAGW-Target-Host` must be a bare hostname or IP address without a port, path, \
             scheme or whitespace; got `{value}`"
        ))
        .with_invalid_value(value.to_owned())
    };
    let unbracketed = value
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value.trim());
    let candidate = normalize_host(unbracketed);
    if candidate.is_empty() {
        return Err(invalid());
    }
    if is_ip_address(&candidate) || is_valid_hostname(&candidate) {
        return Ok(candidate);
    }
    Err(invalid())
}

use crate::domain::model::{is_ip_address, is_valid_hostname};

/// Whether the host is a literal address the SSRF policy refuses to dial.
fn is_restricted_address(address: IpAddr) -> bool {
    // An IPv4-mapped IPv6 literal (`::ffff:10.0.0.1`) is dialed as its mapped
    // IPv4 address by every connector, so it is classified as that IPv4 address
    // and not as native IPv6: the native-v6 predicates would let loopback,
    // RFC 1918 and link-local targets through.
    let address = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        plain @ IpAddr::V4(_) => plain,
    };
    match address {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

/// Enforce the SSRF policy on the resolved endpoint (step 4 of the resolution).
///
/// Only literal IP addresses are inspected, v4 and v6 (an IPv4-mapped IPv6
/// literal is classified as the IPv4 address it maps to). A **hostname**-based
/// endpoint is not resolved here — the proxy hot path performs no DNS lookups —
/// so it is dialed unchecked: pinning or rejecting the addresses a hostname
/// resolves to is DESIGN "Out of Scope" ("DNS resolution / IP pinning rules")
/// and is not implemented by this gear.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the endpoint host is a loopback, private,
/// link-local or unique-local address and the policy is enabled.
pub fn enforce_ssrf_policy(config: &OagwConfig, endpoint: &Endpoint) -> Result<(), OagwError> {
    if !config.ssrf_policy.enabled {
        return Ok(());
    }
    reject_restricted_endpoint(endpoint)
}

/// The outbound dial policy of an endpoint that is not an upstream `server`
/// member — the `OAuth2` token endpoint is the only one — held to the same
/// rules as an upstream endpoint, without the rest of the gear configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DialPolicy {
    /// Whether a plaintext (`http`) endpoint may be dialed.
    pub allow_http: bool,
    /// Whether the SSRF guard restricts IP-literal endpoint hosts.
    pub ssrf_enabled: bool,
}

/// Enforce the SSRF and plaintext policy on a dialed **URL**.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the host is a restricted IP literal under
/// an enabled SSRF policy, and a 503 when the URL is plaintext and plaintext
/// outbound connections are disabled.
pub fn enforce_url_policy(policy: DialPolicy, url: &Url) -> Result<(), OagwError> {
    let scheme = match url.scheme() {
        "https" => EndpointScheme::Https,
        "http" => EndpointScheme::Http,
        other => {
            return Err(OagwError::validation(format!(
                "the endpoint URL `{url}` uses scheme `{other}`, which this gateway does not dial"
            )));
        }
    };
    let host = url.host_str().unwrap_or_default();
    let endpoint = Endpoint {
        scheme,
        host: host.to_owned(),
        port: url.port_or_known_default().unwrap_or(scheme.default_port()),
    };
    if policy.ssrf_enabled {
        reject_restricted_endpoint(&endpoint)?;
    }
    if scheme == EndpointScheme::Http && !policy.allow_http {
        return Err(OagwError::internal(format!(
            "plaintext endpoint connections are disabled: the endpoint scheme `{}` is not dialable \
             without `gears.oagw.config.allow_http_upstream`",
            scheme.name()
        )));
    }
    Ok(())
}

/// The SSRF rejection of `endpoint`, when its host is a restricted IP literal.
fn reject_restricted_endpoint(endpoint: &Endpoint) -> Result<(), OagwError> {
    let Some(address) = normalize_host(&endpoint.host).parse::<IpAddr>().ok() else {
        return Ok(());
    };
    if !is_restricted_address(address) {
        return Ok(());
    }
    let host = normalize_host(&endpoint.host);
    Err(OagwError::validation(format!(
        "ssrf policy: endpoint host `{host}` is a loopback, private or link-local address and may \
         not be dialed"
    ))
    .with_host(host))
}

/// Enforce the plaintext-upstream policy on the resolved endpoint (step 5).
///
/// `http` stays a legal create-time endpoint scheme; whether the data plane may
/// actually open a plaintext connection is a deployment decision
/// (`gears.oagw.config.allow_http_upstream`).
///
/// # Errors
///
/// Returns a 503 [`OagwError`] when the endpoint scheme is `http` and plaintext
/// upstream connections are not allowed.
pub fn enforce_scheme_policy(config: &OagwConfig, endpoint: &Endpoint) -> Result<(), OagwError> {
    if endpoint.scheme != EndpointScheme::Http || config.allows_http_upstream() {
        return Ok(());
    }
    Err(OagwError::link_unavailable(format!(
        "plaintext upstream connections are disabled: endpoint scheme `{}` is not dialable \
         (allow_http_upstream is false)",
        endpoint.scheme.name()
    ))
    .with_host(normalize_host(&endpoint.host)))
}

#[cfg(test)]
#[path = "resolution_tests.rs"]
mod tests;
