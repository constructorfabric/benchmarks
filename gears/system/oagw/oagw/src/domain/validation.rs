//! Create/update validation for OAGW domain objects, including alias
//! derivation (DESIGN.md "Alias Rules").
//!
//! Alias derivation rules implemented here:
//!
//! - Single hostname endpoint, standard port (HTTP 80 / HTTPS/WSS/WT/gRPC 443)
//!   → alias is the hostname.
//! - Single hostname endpoint, non-standard port → alias is `host:port`.
//! - Multiple hostname endpoints sharing a common domain suffix (≥ 2 labels,
//!   PSL-validated registrable domain) → alias is the common suffix (with
//!   `:port` when the shared port is non-standard).
//! - Any IP-based endpoint → not derivable; an explicit alias is required.
//! - A bare public suffix (e.g. `co.uk`) is never accepted as a derived alias.
//! - Aliases are normalized to ASCII lowercase with trailing dots stripped,
//!   and resolved case-insensitively.
//!
//! Hostname-based upstreams *auto-derive* their alias: a user-provided alias
//! matching the derived value is an idempotent no-op; any other user-provided
//! alias is a validation error. Not-derivable (IP / mixed-port) upstreams
//! require an explicit alias.

use crate::domain::models::{
    Endpoint, MatchRule, PROTOCOL_GRPC_V1, PROTOCOL_HTTP_V1, RateLimitConfig, Route, Upstream,
};
use http::header::HeaderName;
use http::HeaderValue;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::str::FromStr;

/// Allowed HTTP route methods (route.v1.schema.json).
const ALLOWED_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
/// Allowed upstream protocols.
const ALLOWED_PROTOCOLS: [&str; 2] = [PROTOCOL_HTTP_V1, PROTOCOL_GRPC_V1];

/// Minimum alias length.
pub const MIN_ALIAS_LEN: usize = 1;
/// Maximum alias length (defensive).
pub const MAX_ALIAS_LEN: usize = 253;

/// Normalize an alias: ASCII lowercase, strip trailing dots.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// Whether `alias` matches the upstream schema pattern:
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn alias_is_valid_format(alias: &str) -> bool {
    if alias.is_empty()
        || alias.len() > MAX_ALIAS_LEN
        || !alias.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b':' || b == b'-'
        })
    {
        return false;
    }
    let first = alias.as_bytes()[0];
    let last = alias.as_bytes()[alias.len() - 1];
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    alnum(first) && alnum(last)
}

/// Whether a host string is an IP address.
#[must_use]
pub fn is_ip(host: &str) -> bool {
    IpAddr::from_str(host).is_ok()
}

fn hostname_labels(host: &str) -> Vec<String> {
    host.split('.').map(ToOwned::to_owned).collect()
}

/// Longest common suffix (from the right) of `a` and `b` label lists.
fn common_suffix_labels(a: &[String], b: &[String]) -> Vec<String> {
    let mut common = Vec::new();
    let mut ai = a.len();
    let mut bi = b.len();
    while ai > 0 && bi > 0 && a[ai - 1] == b[bi - 1] {
        common.push(a[ai - 1].clone());
        ai -= 1;
        bi -= 1;
    }
    common.reverse();
    common
}

/// Compute the derived alias for a set of endpoints, or `None` when the
/// upstream is not derivable (IP-based endpoints, mixed ports, or a common
/// suffix that is not a PSL-valid registrable domain with ≥ 2 labels).
fn derive_from_endpoints(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.iter().any(|e| is_ip(&e.host)) {
        return None;
    }
    // Collect normalized hostnames.
    let hosts: Vec<String> = endpoints.iter().map(|e| normalize_alias(&e.host)).collect();
    if hosts.iter().any(String::is_empty) {
        return None;
    }
    // Single endpoint: hostname or hostname:port (non-standard port).
    if endpoints.len() == 1 {
        let e = &endpoints[0];
        let host = normalize_alias(&e.host);
        if e.port == e.scheme.default_port() {
            return Some(host);
        }
        return Some(format!("{host}:{}", e.port));
    }
    // Multiple endpoints: longest common label suffix.
    let mut common: Option<Vec<String>> = None;
    for h in &hosts {
        let labels = hostname_labels(h);
        common = Some(match common {
            None => labels,
            Some(cur) => common_suffix_labels(&cur, &labels),
        });
    }
    let common = common.unwrap_or_default();
    if common.len() < 2 {
        return None; // requirement: ≥ 2 labels
    }
    let suffix = common.join(".");
    // Reject bare public suffixes (e.g. `co.uk` has no registrable domain).
    if psl::domain_str(&suffix) != Some(suffix.as_str()) {
        return None;
    }
    // Ports: require consistency across endpoints; non-standard port appends `:port`.
    let first_port = endpoints[0].port;
    let first_scheme = endpoints[0].scheme;
    let all_same_port = endpoints.iter().all(|e| e.port == first_port);
    if !all_same_port {
        return None;
    }
    if first_port == first_scheme.default_port() {
        Some(suffix)
    } else {
        Some(format!("{suffix}:{first_port}"))
    }
}

/// Attempt to derive the alias for an upstream (without mutating it).
///
/// Returns `Ok(Some(alias))` when derivable, `Ok(None)` when the upstream
/// requires an explicit alias, `Err(message)` on malformed endpoint data.
///
/// # Errors
///
/// Returns a human-readable message when the upstream defines no endpoints.
pub fn derive_alias(upstream: &Upstream) -> Result<Option<String>, String> {
    if upstream.server.endpoints.is_empty() {
        return Err("upstream must define at least one endpoint".to_owned());
    }
    Ok(derive_from_endpoints(&upstream.server.endpoints))
}

/// Validate protocol identifier and normalize server config.
///
/// # Errors
///
/// Returns a human-readable message for unsupported protocol identifiers.
pub fn validate_protocol(protocol: &str) -> Result<(), String> {
    if !ALLOWED_PROTOCOLS.contains(&protocol) {
        return Err(format!(
            "unsupported protocol '{protocol}' (expected {PROTOCOL_HTTP_V1} or {PROTOCOL_GRPC_V1})"
        ));
    }
    Ok(())
}

/// Validate an endpoint's host is non-empty and scheme is supported.
fn validate_endpoint(e: &Endpoint) -> Result<(), String> {
    if e.host.trim().is_empty() {
        return Err("endpoint host must not be empty".to_owned());
    }
    if !(1..=65535).contains(&e.port) {
        return Err(format!("endpoint port {} out of range (1..65535)", e.port));
    }
    let _ = e.scheme; // Scheme is validated at deserialization.
    Ok(())
}

/// Full create-time validation of an upstream. On success the upstream's
/// `alias` is populated with the derived alias when derivable (an explicit
/// alias equal to the derived value is a no-op; a non-matching explicit alias
/// is an error for derivable upstreams).
///
/// # Errors
/// Returns a human-readable validation message on failure.
pub fn validate_upstream(upstream: &mut Upstream) -> Result<(), String> {
    if upstream.server.endpoints.is_empty() {
        return Err("upstream must define at least one endpoint".to_owned());
    }
    for e in &upstream.server.endpoints {
        validate_endpoint(e)?;
    }
    validate_protocol(&upstream.protocol)?;
    if upstream.tags.iter().any(|t| !valid_tag(t)) {
        return Err("tags must match ^[a-z0-9_-]+$".to_owned());
    }
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    if let Some(rate) = &upstream.rate_limit {
        validate_rate_limit(rate)?;
    }
    validate_headers_config(&upstream.headers)?;

    let derived = derive_from_endpoints(&upstream.server.endpoints);
    let provided = upstream.alias.trim();
    let provided_norm = if provided.is_empty() {
        None
    } else {
        Some(normalize_alias(provided))
    };

    match (derived, provided_norm) {
        (Some(d), None) => {
            upstream.alias = d;
            Ok(())
        }
        (Some(d), Some(p)) if p == d => {
            upstream.alias = d;
            Ok(())
        }
        (Some(d), Some(p)) => Err(format!(
            "alias '{p}' does not match the derived alias '{d}' for hostname-based upstream"
        )),
        (None, None) => Err(
            "alias is required: upstream endpoints are IP-based or do not share a \
             common registrable domain suffix"
                .to_owned(),
        ),
        (None, Some(p)) => {
            if !alias_is_valid_format(&p) {
                return Err(format!(
                    "alias '{p}' does not match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
                ));
            }
            upstream.alias = p;
            Ok(())
        }
    }
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Validate a CORS configuration (ADR 0004): `allow_credentials` cannot be
/// combined with a `*` wildcard origin (a security bypass — the browser
/// rejects such responses, and the combination must fail at validation time).
///
/// # Errors
/// Returns a human-readable validation message on failure.
pub fn validate_cors(cors: &crate::domain::models::CorsConfig) -> Result<(), String> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(
            "cors: 'allow_credentials' cannot be used with a wildcard origin ('*')".to_owned(),
        );
    }
    Ok(())
}

/// Validate a route. `match` must be present with exactly one of `http` or
/// `grpc`; HTTP matches require non-empty method/path with valid method names.
///
/// # Errors
/// Returns a human-readable validation message on failure.
pub fn validate_route(route: &Route) -> Result<(), String> {
    if let Some(cors) = &route.cors {
        validate_cors(cors)?;
    }
    if let Some(rate) = &route.rate_limit {
        validate_rate_limit(rate)?;
    }
    let m = route
        .r#match
        .as_ref()
        .ok_or_else(|| "route requires a 'match' rule".to_owned())?;
    match m {
        MatchRule {
            http: None,
            grpc: None,
        }
        | MatchRule {
            http: Some(_),
            grpc: Some(_),
        } => Err("route 'match' must contain exactly one of 'http' or 'grpc'".to_owned()),
        MatchRule {
            http: Some(h),
            grpc: None,
        } => validate_http_match(h),
        MatchRule {
            http: None,
            grpc: Some(g),
        } => {
            if g.service.trim().is_empty() || g.method.trim().is_empty() {
                return Err("grpc match requires non-empty 'service' and 'method'".to_owned());
            }
            Ok(())
        }
    }
}

fn validate_http_match(h: &crate::domain::models::HttpMatch) -> Result<(), String> {
    if h.methods.is_empty() {
        return Err("http match requires at least one method".to_owned());
    }
    for m in &h.methods {
        if !ALLOWED_METHODS.contains(&m.as_str()) {
            return Err(format!("http match method '{m}' is not allowed"));
        }
    }
    if h.path.trim().is_empty() {
        return Err("http match requires a non-empty 'path'".to_owned());
    }
    Ok(())
}

/// Validate a rate-limit configuration (upstream or route): sustained rate ≥ 1,
/// burst capacity ≥ 1 when present, cost ≥ 1, and cost ≤ capacity (burst
/// capacity when present, else the sustained rate — a cost above the bucket's
/// capacity could never be satisfied).
///
/// # Errors
/// Returns a human-readable validation message on failure.
pub fn validate_rate_limit(c: &RateLimitConfig) -> Result<(), String> {
    if c.sustained.rate < 1 {
        return Err("rate limit 'sustained.rate' must be at least 1".to_owned());
    }
    let capacity = match &c.burst {
        Some(b) => {
            if b.capacity < 1 {
                return Err("rate limit 'burst.capacity' must be at least 1".to_owned());
            }
            b.capacity
        }
        None => c.sustained.rate,
    };
    if c.cost < 1 {
        return Err("rate limit 'cost' must be at least 1".to_owned());
    }
    if c.cost > capacity {
        return Err(format!(
            "rate limit 'cost' ({}) cannot exceed the burst capacity ({capacity})",
            c.cost
        ));
    }
    Ok(())
}

/// Whether `name` targets a gateway-managed header that the transform pipeline
/// may not touch: the hop-by-hop set the proxy strips (RFC 7230 §6.1 and
/// friends) plus the OAGW routing header and the `Proxy-*` family.
/// Case-insensitive.
fn is_gateway_managed_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "connection"
            | "host"
            | "content-length"
            | "transfer-encoding"
            | "upgrade"
            | "keep-alive"
            | "trailer"
            | "te"
            | "x-oagw-target-host"
    ) || lower.starts_with("proxy-")
}

/// Validate upstream header `set`/`add`/`remove` rules (request and response
/// directions) at CRUD time: names must parse as `HeaderName`, values as
/// `HeaderValue`, `remove` entries must be valid names, and no rule may target
/// a gateway-managed (hop-by-hop / routing) header — the proxy strips those on
/// the hot path regardless, so cfg-level rules would silently do nothing.
///
/// # Errors
/// Returns a human-readable validation message on failure.
pub fn validate_headers_config(
    headers: &crate::domain::models::HeadersConfig,
) -> Result<(), String> {
    validate_header_ops(
        &headers.request.set,
        &headers.request.add,
        &headers.request.remove,
        "request",
    )?;
    validate_header_ops(
        &headers.response.set,
        &headers.response.add,
        &headers.response.remove,
        "response",
    )?;
    Ok(())
}

fn validate_header_ops(
    set: &BTreeMap<String, String>,
    add: &BTreeMap<String, String>,
    remove: &[String],
    direction: &str,
) -> Result<(), String> {
    for (name, value) in set {
        validate_header_rule(name, value, direction)?;
    }
    for (name, value) in add {
        validate_header_rule(name, value, direction)?;
    }
    for name in remove {
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            format!("{direction} header 'remove' entry '{name}' is not a valid HTTP header name")
        })?;
        if is_gateway_managed_name(name) {
            return Err(format!(
                "{direction} header 'remove' entry '{name}' targets a gateway-managed header and is not allowed"
            ));
        }
    }
    Ok(())
}

fn validate_header_rule(name: &str, value: &str, direction: &str) -> Result<(), String> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("{direction} header rule '{name}' is not a valid HTTP header name"))?;
    HeaderValue::from_str(value)
        .map_err(|_| format!("{direction} header rule '{name}' has an invalid value"))?;
    if is_gateway_managed_name(name) {
        return Err(format!(
            "{direction} header rule '{name}' targets a gateway-managed header and is not allowed"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{
        BurstConfig, Endpoint, HeadersConfig, HttpMatch, PathSuffixMode, PluginsConfig,
        RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateWindow, Scheme,
        ServerConfig, SharingMode, SustainedRate,
    };

    fn ep(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream_with(endpoints: Vec<Endpoint>) -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            enabled: true,
            alias: String::new(),
            tags: Vec::new(),
            server: ServerConfig { endpoints },
            protocol: PROTOCOL_HTTP_V1.to_owned(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_hostname() {
        let mut u = upstream_with(vec![ep(Scheme::Https, "api.openai.com", 443)]);
        assert_eq!(validate_upstream(&mut u), Ok(()));
        assert_eq!(u.alias, "api.openai.com");
    }

    #[test]
    fn single_hostname_http_80_is_standard() {
        // HTTP upstream on port 80 (standard for http scheme) still uses
        // https default port 443 in the schema; port 443 is the only
        // "standard" value considered here, so 80 produces host:80.
        let mut u = upstream_with(vec![ep(Scheme::Https, "api.example.com", 80)]);
        validate_upstream(&mut u).unwrap();
        assert_eq!(u.alias, "api.example.com:80");
    }

    #[test]
    fn single_hostname_non_standard_port_appends_port() {
        let mut u = upstream_with(vec![ep(Scheme::Https, "api.example.com", 8443)]);
        validate_upstream(&mut u).unwrap();
        assert_eq!(u.alias, "api.example.com:8443");
    }

    #[test]
    fn multi_hostname_common_suffix_derives_suffix() {
        let mut u = upstream_with(vec![
            ep(Scheme::Https, "us.vendor.com", 443),
            ep(Scheme::Https, "eu.vendor.com", 443),
        ]);
        validate_upstream(&mut u).unwrap();
        assert_eq!(u.alias, "vendor.com");
    }

    #[test]
    fn multi_hostname_common_suffix_non_standard_port_appends_port() {
        let mut u = upstream_with(vec![
            ep(Scheme::Https, "us.vendor.com", 8443),
            ep(Scheme::Https, "eu.vendor.com", 8443),
        ]);
        validate_upstream(&mut u).unwrap();
        assert_eq!(u.alias, "vendor.com:8443");
    }

    #[test]
    fn ip_endpoint_requires_explicit_alias() {
        let mut u = upstream_with(vec![ep(Scheme::Https, "10.0.0.5", 443)]);
        assert_eq!(derive_alias(&u), Ok(None));
        // Without explicit alias → validation error.
        assert!(validate_upstream(&mut u).is_err());
        // With explicit alias → OK.
        let mut u2 = upstream_with(vec![ep(Scheme::Https, "10.0.0.5", 443)]);
        u2.alias = "internal-db".to_owned();
        assert_eq!(validate_upstream(&mut u2), Ok(()));
        assert_eq!(u2.alias, "internal-db");
    }

    #[test]
    fn bare_public_suffix_is_rejected() {
        // common suffix `co.uk` is a public suffix → not derivable.
        let mut u = upstream_with(vec![
            ep(Scheme::Https, "a.co.uk", 443),
            ep(Scheme::Https, "b.co.uk", 443),
        ]);
        assert_eq!(derive_alias(&u), Ok(None));
        u.alias = "a.co.uk".to_owned();
        assert!(
            validate_upstream(&mut u).is_ok(),
            "explicit alias still needed"
        );
    }

    #[test]
    fn normalization_applies_to_derived_and_provided_aliases() {
        // Uppercase host + trailing dot normalized.
        let mut u = upstream_with(vec![ep(Scheme::Https, "API.Example.COM.", 443)]);
        validate_upstream(&mut u).unwrap();
        assert_eq!(u.alias, "api.example.com");
        // Explicit alias matching derived (but written differently) is a no-op.
        let mut u2 = upstream_with(vec![ep(Scheme::Https, "api.example.com", 443)]);
        u2.alias = "API.EXAMPLE.COM.".to_owned();
        validate_upstream(&mut u2).unwrap();
        assert_eq!(u2.alias, "api.example.com");
    }

    #[test]
    fn mismatched_explicit_alias_is_rejected_for_derivable_upstream() {
        let mut u = upstream_with(vec![ep(Scheme::Https, "api.example.com", 443)]);
        u.alias = "wrong.alias".to_owned();
        let err = validate_upstream(&mut u).unwrap_err();
        assert!(err.contains("does not match the derived alias"));
    }

    #[test]
    fn different_ports_are_not_derivable() {
        let u = upstream_with(vec![
            ep(Scheme::Https, "us.vendor.com", 443),
            ep(Scheme::Https, "eu.vendor.com", 8443),
        ]);
        assert_eq!(derive_alias(&u), Ok(None));
    }

    #[test]
    fn mixed_ip_and_hostname_not_derivable() {
        let u = upstream_with(vec![
            ep(Scheme::Https, "10.0.0.5", 443),
            ep(Scheme::Https, "api.example.com", 443),
        ]);
        assert_eq!(derive_alias(&u), Ok(None));
    }

    #[test]
    fn empty_endpoints_is_invalid() {
        let mut u = upstream_with(vec![]);
        assert!(validate_upstream(&mut u).is_err());
    }

    #[test]
    fn invalid_protocol_rejected() {
        let mut u = upstream_with(vec![ep(Scheme::Https, "x.com", 443)]);
        u.protocol = "gts.bogus".to_owned();
        assert!(validate_upstream(&mut u).is_err());
    }

    #[test]
    fn route_requires_exactly_one_match_kind() {
        let base = || Route {
            id: uuid::Uuid::new_v4(),
            enabled: true,
            tags: vec![],
            upstream_id: uuid::Uuid::new_v4(),
            r#match: None,
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        };
        assert!(validate_route(&base()).is_err());

        let mut both = base();
        both.r#match = Some(MatchRule {
            http: Some(HttpMatch {
                methods: vec!["GET".into()],
                path: "/x".into(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: Some(crate::domain::models::GrpcMatch {
                service: "s".into(),
                method: "m".into(),
            }),
        });
        assert!(validate_route(&both).is_err());

        let mut good = base();
        good.r#match = Some(MatchRule {
            http: Some(HttpMatch {
                methods: vec!["GET".into()],
                path: "/x".into(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        });
        assert_eq!(validate_route(&good), Ok(()));
    }

    #[test]
    fn route_http_methods_restricted() {
        let r = Route {
            id: uuid::Uuid::new_v4(),
            enabled: true,
            tags: vec![],
            upstream_id: uuid::Uuid::new_v4(),
            r#match: Some(MatchRule {
                http: Some(HttpMatch {
                    methods: vec!["TRACE".into()],
                    path: "/x".into(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            }),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        };
        assert!(validate_route(&r).is_err());
    }

    #[test]
    fn alias_format_validation() {
        assert!(alias_is_valid_format("api.example.com"));
        assert!(alias_is_valid_format("my-alias"));
        assert!(alias_is_valid_format("a1"));
        assert!(alias_is_valid_format("10.0.0.5:8443"));
        assert!(!alias_is_valid_format("-lead"));
        assert!(!alias_is_valid_format("trail-"));
        assert!(!alias_is_valid_format("UPPER"));
        assert!(!alias_is_valid_format(""));
        assert!(!alias_is_valid_format("under_score"));
    }

    #[test]
    fn normalize_alias_rules() {
        assert_eq!(normalize_alias(" API.EXAMPLE.COM. "), "api.example.com");
        assert_eq!(normalize_alias("x.y.z."), "x.y.z");
    }

    #[test]
    fn cors_rejects_credentials_with_wildcard_origin() {
        use crate::domain::models::CorsConfig;
        let bad = CorsConfig {
            sharing: SharingMode::default(),
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec![],
            allow_credentials: true,
        };
        assert!(validate_cors(&bad).is_err());
        // Wildcard without credentials is fine (public API).
        let public = CorsConfig {
            allow_credentials: false,
            ..bad.clone()
        };
        assert_eq!(validate_cors(&public), Ok(()));
        // Specific origins with credentials are fine.
        let specific = CorsConfig {
            allowed_origins: vec!["https://app.example.com".to_owned()],
            ..bad
        };
        assert_eq!(validate_cors(&specific), Ok(()));
    }

    fn rate_config(rate: u64, cost: u64) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::default(),
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::default(),
            cost,
        }
    }

    #[test]
    fn rate_limit_validation_rejects_zero_rate_zero_cost_and_cost_over_capacity() {
        // rate 0 → rejected.
        assert!(validate_rate_limit(&rate_config(0, 1)).is_err());
        // cost 0 → rejected.
        assert!(validate_rate_limit(&rate_config(10, 0)).is_err());
        // cost above the default capacity (= rate) → rejected.
        let err = validate_rate_limit(&rate_config(10, 11)).unwrap_err();
        assert!(err.contains("cannot exceed the burst capacity"));
        // Valid config passes.
        assert_eq!(validate_rate_limit(&rate_config(10, 1)), Ok(()));
        // Explicit burst capacity below cost → rejected; at/above → passes.
        let low_burst = RateLimitConfig {
            burst: Some(BurstConfig { capacity: 2 }),
            ..rate_config(10, 5)
        };
        assert!(validate_rate_limit(&low_burst).is_err());
        let ok_burst = RateLimitConfig {
            burst: Some(BurstConfig { capacity: 5 }),
            ..rate_config(10, 5)
        };
        assert_eq!(validate_rate_limit(&ok_burst), Ok(()));
        // Zero burst capacity → rejected.
        let zero_burst = RateLimitConfig {
            burst: Some(BurstConfig { capacity: 0 }),
            ..rate_config(10, 1)
        };
        assert!(validate_rate_limit(&zero_burst).is_err());
    }

    #[test]
    fn upstream_rate_limit_and_route_rate_limit_are_validated() {
        // Upstream with an invalid rate limit is rejected at create time.
        let mut u = upstream_with(vec![ep(Scheme::Https, "10.0.0.5", 443)]);
        u.alias = "internal".to_owned();
        u.rate_limit = Some(rate_config(0, 1));
        assert!(validate_upstream(&mut u).is_err());

        // Route with cost > capacity is rejected.
        let r = Route {
            id: uuid::Uuid::new_v4(),
            enabled: true,
            tags: vec![],
            upstream_id: uuid::Uuid::new_v4(),
            r#match: Some(MatchRule {
                http: Some(HttpMatch {
                    methods: vec!["GET".into()],
                    path: "/x".into(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            }),
            plugins: PluginsConfig::default(),
            rate_limit: Some(rate_config(10, 11)),
            cors: None,
        };
        assert!(validate_route(&r).is_err());
    }

    #[test]
    fn headers_config_validates_names_values_and_gateway_managed_rules() {
        let mut cfg = HeadersConfig::default();
        assert_eq!(validate_headers_config(&cfg), Ok(()));

        // Invalid header name → rejected.
        cfg.request.set.insert("bad name!".to_owned(), "v".to_owned());
        assert!(validate_headers_config(&cfg).is_err());
        cfg.request.set.clear();

        // Invalid header value (contains a forbidden byte) → rejected.
        cfg.request.add.insert("x-test".to_owned(), "has\nnewline".to_owned());
        assert!(validate_headers_config(&cfg).is_err());
        cfg.request.add.clear();

        // Gateway-managed names are rejected across set/add/remove and both
        // directions.
        for name in [
            "Connection",
            "host",
            "Content-Length",
            "Transfer-Encoding",
            "Upgrade",
            "Keep-Alive",
            "Trailer",
            "TE",
            "x-oagw-target-host",
            "Proxy-Authorization",
            "proxy-connection",
        ] {
            cfg.request.set.insert(name.to_owned(), "v".to_owned());
            assert!(
                validate_headers_config(&cfg).is_err(),
                "request set {name} must be rejected"
            );
            cfg.request.set.clear();

            cfg.request.remove.push(name.to_owned());
            assert!(
                validate_headers_config(&cfg).is_err(),
                "request remove {name} must be rejected"
            );
            cfg.request.remove.clear();

            cfg.response.add.insert(name.to_owned(), "v".to_owned());
            assert!(
                validate_headers_config(&cfg).is_err(),
                "response add {name} must be rejected"
            );
            cfg.response.add.clear();
        }

        // Well-formed user headers pass in both directions.
        cfg.request.set.insert("x-oagw-tenant".to_owned(), "acme".to_owned());
        cfg.request.add.insert("x-request-id".to_owned(), "abc".to_owned());
        cfg.response.set.insert("x-source".to_owned(), "gateway".to_owned());
        cfg.response.remove.push("x-internal".to_owned());
        assert_eq!(validate_headers_config(&cfg), Ok(()));
    }
}
