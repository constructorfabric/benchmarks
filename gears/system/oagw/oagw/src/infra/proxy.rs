//! Outbound proxy engine.
//!
//! Builds the outbound request from a resolved upstream and route, forwards it,
//! and relays the response. Plain responses, server-sent-event streams and
//! WebSocket upgrades all travel through this module.

use crate::domain::error::{DomainError, DomainResult, ErrorKind};
use crate::domain::model::{
    Endpoint, HeadersConfig, PassthroughMode, PathSuffixMode, Route, Scheme, Upstream,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Header the caller uses to pin a request to one endpoint of a pool.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Headers consumed by the gateway and never forwarded.
const ROUTING_HEADERS: [&str; 1] = [TARGET_HOST_HEADER];

/// Hop-by-hop headers stripped per the header categories in `DESIGN.md`
/// section 3.2.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Hard body limit before buffering, in bytes.
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Whether a header is stripped before the request leaves the gateway.
///
/// `Content-Length` is stripped alongside the framing headers: the outbound
/// client recomputes it from the body actually sent, so a stale value copied
/// from the inbound request is never forwarded.
#[must_use]
pub fn is_stripped_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
        || ROUTING_HEADERS.contains(&lower.as_str())
        || lower == "host"
        || lower == "content-length"
}

// @cpt-begin:cpt-cf-oagw-dod-proxy-http-endpoint-selection:p1:inst-endpoint
/// Round-robin cursor shared by every pool.
#[derive(Debug, Default)]
pub struct RoundRobin {
    cursor: AtomicUsize,
}

impl RoundRobin {
    /// Create a cursor starting at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance and return the next index within `len`.
    pub fn next_index(&self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        self.cursor.fetch_add(1, Ordering::Relaxed) % len
    }
}

/// Whether an alias was derived from a shared suffix rather than named outright.
///
/// A pool whose alias is a derived common suffix cannot pick an endpoint on its
/// own, so the caller must name one.
#[must_use]
pub fn alias_is_common_suffix(alias: &str, endpoints: &[Endpoint]) -> bool {
    if endpoints.len() < 2 {
        return false;
    }
    let bare = alias.split(':').next().unwrap_or(alias);
    endpoints
        .iter()
        .all(|e| e.host.to_ascii_lowercase().ends_with(bare))
        && !endpoints.iter().any(|e| e.host.eq_ignore_ascii_case(bare))
}

/// Choose the endpoint a request is forwarded to.
///
/// Implements the `X-OAGW-Target-Host` behaviour matrix of ADR-0001.
///
/// # Errors
/// Returns `MissingTargetHost`, `InvalidTargetHost` or `UnknownTargetHost` when
/// the header is required, malformed, or names no configured endpoint.
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    target_host: Option<&str>,
    round_robin: &RoundRobin,
) -> DomainResult<&'a Endpoint> {
    let endpoints = &upstream.server.endpoints;
    if endpoints.is_empty() {
        return Err(DomainError::new(
            ErrorKind::LinkUnavailable,
            "upstream has no endpoints",
        ));
    }

    if let Some(raw) = target_host {
        let host = raw.trim();
        if host.is_empty() || host.contains('/') || host.contains(':') || host.contains(' ') {
            return Err(DomainError::new(
                ErrorKind::InvalidTargetHost,
                "X-OAGW-Target-Host must be a bare hostname or IP with no port or path",
            )
            .with_context(serde_json::json!({ "invalid_value": raw })));
        }
        return endpoints
            .iter()
            .find(|e| e.host.eq_ignore_ascii_case(host))
            .ok_or_else(|| {
                let valid: Vec<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();
                DomainError::new(
                    ErrorKind::UnknownTargetHost,
                    format!("X-OAGW-Target-Host `{host}` matches no configured endpoint"),
                )
                .with_context(serde_json::json!({
                    "invalid_value": host,
                    "valid_hosts": valid,
                }))
            });
    }

    if endpoints.len() == 1 {
        return Ok(&endpoints[0]);
    }

    if alias_is_common_suffix(&upstream.alias, endpoints) {
        let valid: Vec<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();
        return Err(DomainError::new(
            ErrorKind::MissingTargetHost,
            "X-OAGW-Target-Host is required for a multi-endpoint upstream whose alias is a \
             derived common suffix",
        )
        .with_context(serde_json::json!({ "valid_hosts": valid })));
    }

    // An explicitly named pool distributes across its endpoints.
    let index = round_robin.next_index(endpoints.len());
    Ok(&endpoints[index])
}
// @cpt-end:cpt-cf-oagw-dod-proxy-http-endpoint-selection:p1:inst-endpoint

// @cpt-begin:cpt-cf-oagw-dod-proxy-http-route-and-guards:p1:inst-match
/// Choose the route that matches a request.
///
/// Matching is by method allowlist and longest path prefix. Disabled routes are
/// excluded by the caller.
#[must_use]
pub fn match_route<'a>(
    routes: &'a [Route],
    method: &Method,
    path_suffix: &str,
) -> Option<&'a Route> {
    let method_name = method.as_str();
    let suffix = normalize_path(path_suffix);
    let mut best: Option<(&Route, usize)> = None;
    for route in routes {
        if !route.enabled {
            continue;
        }
        let Some(http) = route.match_config.http.as_ref() else {
            continue;
        };
        if !http.methods.iter().any(|m| m == method_name) {
            continue;
        }
        let route_path = normalize_path(&http.path);
        let matches = route_path == "/"
            || suffix == route_path
            || suffix.starts_with(&format!("{route_path}/"));
        if !matches {
            continue;
        }
        let score = route_path.len();
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((route, score));
        }
    }
    best.map(|(route, _)| route)
}

/// Normalize a path so comparisons ignore a missing or trailing slash.
#[must_use]
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    }
}

/// Reject a path suffix carrying a `.` or `..` segment.
///
/// The suffix is expected already percent-decoded (the `Path` extractor does
/// this), so an encoded `%2e%2e` and a literal `..` are indistinguishable by
/// the time this runs, and both are rejected. This must run before route
/// matching: with `path_suffix_mode: append` a suffix such as
/// `/v1/../admin` matches the `/v1` route and is forwarded literally, which an
/// upstream that normalizes dot segments resolves outside the path prefix the
/// route was meant to confine the caller to. Rejecting outright is safer than
/// canonicalizing the suffix ourselves, because canonicalizing silently
/// changes what the caller asked for.
///
/// # Errors
/// Returns a validation error when any segment of the suffix is exactly `.`
/// or `..`. A segment that merely contains dots, such as `my..file`, is left
/// alone.
pub fn reject_relative_path_segments(path_suffix: &str) -> DomainResult<()> {
    let has_relative_segment = path_suffix
        .split('/')
        .any(|segment| segment == ".." || segment == ".");
    if has_relative_segment {
        return Err(DomainError::validation(
            "a path suffix must not contain a `.` or `..` segment",
        ));
    }
    Ok(())
}

/// Apply the guard rules that can reject a request before it is forwarded.
///
/// # Errors
/// Returns a validation error when the method, a query parameter, or a path
/// suffix is not permitted by the route.
pub fn apply_guards(
    route: &Route,
    method: &Method,
    path_suffix: &str,
    query: &str,
) -> DomainResult<()> {
    let Some(http) = route.match_config.http.as_ref() else {
        return Ok(());
    };
    if !http.methods.iter().any(|m| m == method.as_str()) {
        return Err(DomainError::validation(format!(
            "method {method} is not permitted by this route"
        )));
    }
    let route_path = normalize_path(&http.path);
    let suffix = normalize_path(path_suffix);
    let extra = suffix
        .strip_prefix(&route_path)
        .unwrap_or("")
        .trim_start_matches('/');
    if http.path_suffix_mode == PathSuffixMode::Disabled && !extra.is_empty() {
        return Err(DomainError::validation(
            "this route does not accept a path suffix",
        ));
    }
    for (name, _) in form_urlencoded::parse(query.as_bytes()) {
        if !http.query_allowlist.iter().any(|a| *a == name) {
            return Err(DomainError::validation(format!(
                "query parameter `{name}` is not permitted by this route"
            )));
        }
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-proxy-http-route-and-guards:p1:inst-match

/// Build the upstream path from the route path and the request suffix.
#[must_use]
pub fn build_upstream_path(route: &Route, path_suffix: &str) -> String {
    let Some(http) = route.match_config.http.as_ref() else {
        return normalize_path(path_suffix);
    };
    let route_path = normalize_path(&http.path);
    if http.path_suffix_mode == PathSuffixMode::Disabled {
        return route_path;
    }
    let suffix = normalize_path(path_suffix);
    if suffix == route_path {
        return route_path;
    }
    // The inbound suffix already carries the route path as its prefix.
    suffix
}

/// Build the absolute upstream URL for a request.
#[must_use]
pub fn build_upstream_url(endpoint: &Endpoint, path: &str, query: &str) -> String {
    let scheme = endpoint.scheme.url_scheme();
    let host = &endpoint.host;
    let port = endpoint.port;
    let authority = if port == endpoint.scheme.standard_port() {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if query.is_empty() {
        format!("{scheme}://{authority}{path}")
    } else {
        format!("{scheme}://{authority}{path}?{query}")
    }
}

// @cpt-begin:cpt-cf-oagw-dod-proxy-http-header-transform:p1:inst-headers
/// Build the outbound header map from the inbound one.
///
/// Routing headers are consumed, hop-by-hop headers (which now includes
/// `Content-Length`; see [`is_stripped_header`]) are stripped, and the
/// remainder is forwarded according to the upstream's passthrough mode. `Host`
/// is replaced with the selected endpoint's authority.
///
/// This is the base construction step only. The configured
/// `headers.request.{remove,set,add}` rules are applied later, by
/// [`apply_request_header_rules`], so that credential injection and the
/// guard/transform plugin chain run first and a configured rule can still
/// override anything they produced.
#[must_use]
pub fn build_outbound_headers(
    inbound: &HeaderMap,
    endpoint: &Endpoint,
    headers_config: Option<&HeadersConfig>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    let passthrough = headers_config.map_or(PassthroughMode::All, |c| c.request.passthrough);
    let allowlist: Vec<String> = headers_config
        .map(|c| {
            c.request
                .passthrough_allowlist
                .iter()
                .map(|n| n.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();

    for (name, value) in inbound {
        let lower = name.as_str().to_ascii_lowercase();
        if is_stripped_header(&lower) {
            continue;
        }
        let forward = match passthrough {
            PassthroughMode::All => true,
            PassthroughMode::None => false,
            PassthroughMode::Allowlist => allowlist.contains(&lower),
        };
        if forward {
            out.append(name.clone(), value.clone());
        }
    }

    // The upstream authority replaces the inbound Host header.
    let authority = if endpoint.port == endpoint.scheme.standard_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    if let Ok(value) = HeaderValue::from_str(&authority) {
        out.insert(http::header::HOST, value);
    }
    out
}

/// Apply the configured `headers.request.{remove,set,add}` rules.
///
/// Runs after credential injection and the guard/transform plugin chain, so a
/// configured `set` rule is the last word and can override an injected
/// credential or a transform's result, as the request-phase ordering
/// documents.
pub fn apply_request_header_rules(headers: &mut HeaderMap, headers_config: Option<&HeadersConfig>) {
    let Some(cfg) = headers_config else {
        return;
    };
    for name in &cfg.request.remove {
        if let Ok(header) = HeaderName::try_from(name.to_ascii_lowercase().as_str()) {
            headers.remove(&header);
        }
    }
    for (name, value) in &cfg.request.set {
        if let (Ok(header), Ok(v)) = (
            HeaderName::try_from(name.to_ascii_lowercase().as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(header, v);
        }
    }
    for (name, value) in &cfg.request.add {
        if let (Ok(header), Ok(v)) = (
            HeaderName::try_from(name.to_ascii_lowercase().as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.append(header, v);
        }
    }
}

/// Apply the configured response header rules to a relayed response.
pub fn apply_response_header_rules(
    headers: &mut HeaderMap,
    headers_config: Option<&HeadersConfig>,
) {
    // Hop-by-hop headers never survive a relay.
    for name in HOP_BY_HOP {
        if let Ok(header) = HeaderName::try_from(name) {
            headers.remove(&header);
        }
    }
    let Some(cfg) = headers_config else {
        return;
    };
    for name in &cfg.response.remove {
        if let Ok(header) = HeaderName::try_from(name.to_ascii_lowercase().as_str()) {
            headers.remove(&header);
        }
    }
    for (name, value) in &cfg.response.set {
        if let (Ok(header), Ok(v)) = (
            HeaderName::try_from(name.to_ascii_lowercase().as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(header, v);
        }
    }
    for (name, value) in &cfg.response.add {
        if let (Ok(header), Ok(v)) = (
            HeaderName::try_from(name.to_ascii_lowercase().as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.append(header, v);
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-proxy-http-header-transform:p1:inst-headers

// @cpt-begin:cpt-cf-oagw-dod-proxy-http-ssrf-guard:p1:inst-ssrf
/// Run the server-side request forgery checks for an endpoint.
///
/// The checks always execute. `enforced` decides whether a failure rejects the
/// request or is merely recorded, so disabling the policy does not remove the
/// check from the request path.
///
/// # Errors
/// Returns a validation error when a check fails and the policy is enforced.
pub fn ssrf_check(endpoint: &Endpoint, enforced: bool) -> DomainResult<()> {
    let host = endpoint.host.to_ascii_lowercase();
    let mut failure: Option<String> = None;

    if host.is_empty() {
        failure = Some("endpoint host is empty".to_owned());
    } else if let Ok(ip) = host.parse::<std::net::IpAddr>()
        && (ip.is_loopback() || ip.is_unspecified() || is_private_address(ip))
    {
        failure = Some(format!("endpoint address {ip} is in a restricted range"));
    }

    match failure {
        Some(reason) if enforced => Err(DomainError::validation(format!(
            "server-side request forgery policy rejected the upstream: {reason}"
        ))),
        Some(reason) => {
            tracing::debug!(
                reason = %reason,
                "ssrf policy is disabled; the check ran and did not reject the request"
            );
            Ok(())
        }
        None => Ok(()),
    }
}

/// Whether an address belongs to a private or link-local range.
fn is_private_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_private_v4(v4),
        std::net::IpAddr::V6(v6) => {
            // `::1` is caught by `is_loopback` before it can be misread as
            // the deprecated IPv4-compatible form (`::0.0.0.1`) below.
            if v6.is_loopback() {
                return true;
            }
            // An IPv4-mapped address (`::ffff:a.b.c.d`) carries a real IPv4
            // address inside an IPv6 literal; unwrap it and apply the IPv4
            // rules, so e.g. `::ffff:169.254.169.254` cannot slip past the
            // checks below meant for a bare IPv6 address.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            let segments = v6.segments();
            // The deprecated IPv4-compatible form (`::a.b.c.d`) shares its
            // top 96 bits with `::` and `::1`, both handled above, so only a
            // genuinely embedded address reaches this branch.
            if segments[0..6] == [0, 0, 0, 0, 0, 0] && (segments[6] != 0 || segments[7] > 1) {
                let octets = v6.octets();
                let v4 = std::net::Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
                return is_private_v4(v4);
            }
            // Unique local (`fc00::/7`) and link-local (`fe80::/10`).
            (segments[0] & 0xfe00) == 0xfc00 || v6.is_unicast_link_local()
        }
    }
}

/// Whether an IPv4 address belongs to a private or link-local range.
fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_private() || v4.is_link_local()
}
// @cpt-end:cpt-cf-oagw-dod-proxy-http-ssrf-guard:p1:inst-ssrf

/// Whether a plaintext connection to this endpoint is permitted.
///
/// Which scheme values the API accepts is a separate question, settled at
/// create time. This decides only whether the connection is actually made.
///
/// # Errors
/// Returns a validation error when the endpoint is plaintext and the gear
/// configuration does not allow a plaintext upstream.
pub fn check_plaintext_allowed(endpoint: &Endpoint, allow_http_upstream: bool) -> DomainResult<()> {
    if endpoint.scheme.is_plaintext() && !allow_http_upstream {
        return Err(DomainError::validation(
            "a plaintext upstream connection is not permitted; set allow_http_upstream to \
             enable it",
        ));
    }
    Ok(())
}

/// Whether a response is a server-sent-event stream.
#[must_use]
pub fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
}

/// Whether an inbound request asks for a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    upgrade && connection
}

/// Build the upstream WebSocket URL for an endpoint.
///
/// The scheme is mapped to its WebSocket form, so a plaintext endpoint dials
/// `ws://` and a TLS endpoint dials `wss://`.
#[must_use]
pub fn build_websocket_url(endpoint: &Endpoint, path: &str, query: &str) -> String {
    let scheme = match endpoint.scheme {
        Scheme::Http | Scheme::Ws => "ws",
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => "wss",
    };
    let authority = if endpoint.port == endpoint.scheme.standard_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if query.is_empty() {
        format!("{scheme}://{authority}{path}")
    } else {
        format!("{scheme}://{authority}{path}?{query}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RoundRobin, alias_is_common_suffix, apply_guards, apply_request_header_rules,
        build_outbound_headers, build_upstream_url, build_websocket_url, check_plaintext_allowed,
        is_event_stream, is_stripped_header, is_websocket_upgrade, match_route, normalize_path,
        reject_relative_path_segments, select_endpoint, ssrf_check,
    };
    use crate::domain::model::{
        Endpoint, HttpMatch, MatchConfig, PROTOCOL_HTTP, PathSuffixMode, Route, Scheme,
        ServerConfig, Upstream,
    };
    use http::{HeaderMap, HeaderValue, Method};
    use uuid::Uuid;

    fn endpoint(host: &str, scheme: Scheme, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            enabled: true,
            server: ServerConfig { endpoints },
            protocol: PROTOCOL_HTTP.to_owned(),
            tags: vec![],
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn route(path: &str, methods: &[&str], suffix_mode: PathSuffixMode) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: suffix_mode,
                }),
                grpc: None,
            },
            tags: vec![],
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn hop_by_hop_and_routing_headers_are_stripped() {
        for name in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "host",
            "x-oagw-target-host",
        ] {
            assert!(is_stripped_header(name), "{name} should be stripped");
        }
        assert!(!is_stripped_header("accept"));
    }

    #[test]
    fn the_host_header_is_replaced_with_the_upstream_authority() {
        let mut inbound = HeaderMap::new();
        inbound.insert(
            http::header::HOST,
            HeaderValue::from_static("gateway.local"),
        );
        inbound.insert("connection", HeaderValue::from_static("keep-alive"));
        inbound.insert("accept", HeaderValue::from_static("application/json"));
        let out = build_outbound_headers(
            &inbound,
            &endpoint("api.example.com", Scheme::Https, 443),
            None,
        );
        assert_eq!(
            out.get(http::header::HOST).expect("host"),
            "api.example.com"
        );
        assert!(!out.contains_key("connection"));
        assert_eq!(out.get("accept").expect("accept"), "application/json");
    }

    #[test]
    fn a_single_endpoint_is_selected_without_a_header() {
        let up = upstream(
            "api.example.com",
            vec![endpoint("api.example.com", Scheme::Https, 443)],
        );
        let chosen = select_endpoint(&up, None, &RoundRobin::new()).expect("selected");
        assert_eq!(chosen.host, "api.example.com");
    }

    #[test]
    fn a_malformed_target_host_is_rejected() {
        let up = upstream(
            "api.example.com",
            vec![endpoint("api.example.com", Scheme::Https, 443)],
        );
        let err = select_endpoint(&up, Some("host:8080"), &RoundRobin::new()).expect_err("invalid");
        assert_eq!(err.status(), 400);
        assert_eq!(err.kind, crate::domain::error::ErrorKind::InvalidTargetHost);
    }

    #[test]
    fn an_unknown_target_host_is_rejected() {
        let up = upstream(
            "api.example.com",
            vec![endpoint("api.example.com", Scheme::Https, 443)],
        );
        let err = select_endpoint(&up, Some("other.example.com"), &RoundRobin::new())
            .expect_err("unknown");
        assert_eq!(err.kind, crate::domain::error::ErrorKind::UnknownTargetHost);
    }

    #[test]
    fn a_common_suffix_pool_requires_the_target_host_header() {
        let up = upstream(
            "vendor.com",
            vec![
                endpoint("us.vendor.com", Scheme::Https, 443),
                endpoint("eu.vendor.com", Scheme::Https, 443),
            ],
        );
        assert!(alias_is_common_suffix(&up.alias, &up.server.endpoints));
        let err = select_endpoint(&up, None, &RoundRobin::new()).expect_err("header required");
        assert_eq!(err.kind, crate::domain::error::ErrorKind::MissingTargetHost);
        let chosen =
            select_endpoint(&up, Some("eu.vendor.com"), &RoundRobin::new()).expect("named");
        assert_eq!(chosen.host, "eu.vendor.com");
    }

    #[test]
    fn an_explicitly_named_pool_round_robins() {
        let up = upstream(
            "my-pool",
            vec![
                endpoint("10.0.0.1", Scheme::Https, 443),
                endpoint("10.0.0.2", Scheme::Https, 443),
            ],
        );
        let rr = RoundRobin::new();
        let first = select_endpoint(&up, None, &rr).expect("first").host.clone();
        let second = select_endpoint(&up, None, &rr)
            .expect("second")
            .host
            .clone();
        assert_ne!(first, second);
    }

    #[test]
    fn the_longest_matching_path_prefix_wins() {
        let routes = vec![
            route("/v1", &["GET"], PathSuffixMode::Append),
            route("/v1/models", &["GET"], PathSuffixMode::Append),
        ];
        let chosen = match_route(&routes, &Method::GET, "/v1/models/list").expect("matched");
        assert_eq!(
            chosen.match_config.http.as_ref().expect("http").path,
            "/v1/models"
        );
    }

    #[test]
    fn a_disabled_route_never_matches() {
        let mut r = route("/v1", &["GET"], PathSuffixMode::Append);
        r.enabled = false;
        assert!(match_route(&[r], &Method::GET, "/v1").is_none());
    }

    #[test]
    fn a_method_outside_the_allowlist_does_not_match() {
        let routes = vec![route("/v1", &["GET"], PathSuffixMode::Append)];
        assert!(match_route(&routes, &Method::POST, "/v1").is_none());
    }

    #[test]
    fn a_disallowed_suffix_is_rejected() {
        let r = route("/v1", &["GET"], PathSuffixMode::Disabled);
        assert!(apply_guards(&r, &Method::GET, "/v1/extra", "").is_err());
        assert!(apply_guards(&r, &Method::GET, "/v1", "").is_ok());
    }

    #[test]
    fn a_query_parameter_outside_the_allowlist_is_rejected() {
        let mut r = route("/v1", &["GET"], PathSuffixMode::Append);
        if let Some(http) = r.match_config.http.as_mut() {
            http.query_allowlist = vec!["limit".to_owned()];
        }
        assert!(apply_guards(&r, &Method::GET, "/v1", "limit=5").is_ok());
        assert!(apply_guards(&r, &Method::GET, "/v1", "offset=5").is_err());
    }

    #[test]
    fn upstream_urls_omit_the_standard_port() {
        let url = build_upstream_url(&endpoint("api.example.com", Scheme::Https, 443), "/v1", "");
        assert_eq!(url, "https://api.example.com/v1");
        let plain = build_upstream_url(&endpoint("stub.local", Scheme::Http, 80), "/v1", "a=1");
        assert_eq!(plain, "http://stub.local/v1?a=1");
        let ported = build_upstream_url(&endpoint("stub.local", Scheme::Http, 8080), "/v1", "");
        assert_eq!(ported, "http://stub.local:8080/v1");
    }

    #[test]
    fn websocket_urls_use_the_websocket_scheme() {
        assert_eq!(
            build_websocket_url(&endpoint("stub.local", Scheme::Http, 8080), "/ws", ""),
            "ws://stub.local:8080/ws"
        );
        assert_eq!(
            build_websocket_url(&endpoint("api.example.com", Scheme::Https, 443), "/ws", ""),
            "wss://api.example.com/ws"
        );
    }

    #[test]
    fn plaintext_is_gated_by_the_configuration_flag() {
        let plain = endpoint("stub.local", Scheme::Http, 80);
        assert!(check_plaintext_allowed(&plain, true).is_ok());
        assert!(check_plaintext_allowed(&plain, false).is_err());
        let tls = endpoint("api.example.com", Scheme::Https, 443);
        assert!(check_plaintext_allowed(&tls, false).is_ok());
    }

    #[test]
    fn the_ssrf_check_runs_but_does_not_reject_when_the_policy_is_off() {
        let loopback = endpoint("127.0.0.1", Scheme::Http, 8080);
        assert!(ssrf_check(&loopback, false).is_ok());
        assert!(ssrf_check(&loopback, true).is_err());
        let public = endpoint("api.example.com", Scheme::Https, 443);
        assert!(ssrf_check(&public, true).is_ok());
    }

    #[test]
    fn event_streams_are_detected_by_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_event_stream(&headers));
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(!is_event_stream(&headers));
    }

    #[test]
    fn websocket_upgrades_need_both_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!is_websocket_upgrade(&headers));
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("Upgrade"),
        );
        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn paths_normalize_consistently() {
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("v1/models"), "/v1/models");
        assert_eq!(normalize_path("/v1/models/"), "/v1/models");
    }

    #[test]
    fn a_dot_dot_segment_is_rejected() {
        assert!(reject_relative_path_segments("/v1/../admin").is_err());
    }

    #[test]
    fn a_percent_encoded_dot_dot_already_decoded_by_the_extractor_is_rejected() {
        // The `Path` extractor percent-decodes the suffix before this check
        // ever sees it, so an inbound `%2e%2e` arrives here as a literal `..`
        // and is indistinguishable from one the caller typed directly.
        let decoded_suffix = "/v1/%2e%2e/admin".replace("%2e%2e", "..");
        assert_eq!(decoded_suffix, "/v1/../admin");
        assert!(reject_relative_path_segments(&decoded_suffix).is_err());
    }

    #[test]
    fn a_leading_dot_dot_segment_is_rejected() {
        assert!(reject_relative_path_segments("../admin").is_err());
    }

    #[test]
    fn a_single_dot_segment_is_rejected() {
        assert!(reject_relative_path_segments("/v1/./admin").is_err());
    }

    #[test]
    fn a_segment_merely_containing_dots_is_allowed() {
        assert!(reject_relative_path_segments("/v1/my..file").is_ok());
        assert!(reject_relative_path_segments("/v1/models").is_ok());
    }

    #[test]
    fn content_length_is_never_forwarded_to_the_upstream() {
        assert!(is_stripped_header("content-length"));
        assert!(is_stripped_header("Content-Length"));

        let mut inbound = HeaderMap::new();
        inbound.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_static("12345"),
        );
        let out = build_outbound_headers(
            &inbound,
            &endpoint("api.example.com", Scheme::Https, 443),
            None,
        );
        assert!(!out.contains_key(http::header::CONTENT_LENGTH));
    }

    #[test]
    fn request_header_rules_can_override_an_earlier_value() {
        use crate::domain::model::{HeadersConfig, RequestHeaderRules, ResponseHeaderRules};
        use std::collections::BTreeMap;

        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer injected"));

        let mut set = BTreeMap::new();
        set.insert("authorization".to_owned(), "Bearer configured".to_owned());
        let config = HeadersConfig {
            request: RequestHeaderRules {
                set,
                ..Default::default()
            },
            response: ResponseHeaderRules::default(),
        };
        apply_request_header_rules(&mut headers, Some(&config));
        assert_eq!(
            headers.get("authorization").expect("set"),
            "Bearer configured"
        );
    }

    #[test]
    fn ssrf_ipv4_mapped_metadata_address_is_detected() {
        let ep = endpoint("::ffff:169.254.169.254", Scheme::Http, 80);
        assert!(ssrf_check(&ep, true).is_err());
    }

    #[test]
    fn ssrf_ipv4_mapped_private_address_is_detected() {
        let ep = endpoint("::ffff:10.0.0.1", Scheme::Http, 80);
        assert!(ssrf_check(&ep, true).is_err());
    }

    #[test]
    fn ssrf_ipv6_link_local_is_detected() {
        let ep = endpoint("fe80::1", Scheme::Http, 80);
        assert!(ssrf_check(&ep, true).is_err());
    }

    #[test]
    fn ssrf_a_genuinely_public_ipv6_address_passes() {
        let ep = endpoint("2606:4700:4700::1111", Scheme::Https, 443);
        assert!(ssrf_check(&ep, true).is_ok());
    }
}
