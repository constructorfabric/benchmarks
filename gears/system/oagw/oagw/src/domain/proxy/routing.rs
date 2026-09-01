// Created: 2026-08-31 by Constructor Tech
//! Pure routing decisions of the proxy data plane (DESIGN §3.2, ADR-0001).
//!
//! Nothing here touches the store, the network or the clock: a decision is
//! derived from the domain model plus the request pieces, or rejected with a
//! 4xx [`OagwError`]. [`super::service::ProxyService`] owns the I/O.

use url::Url;

use super::super::alias;
use crate::domain::model::{Endpoint, HttpMatch, Route, RouteMatch, Upstream};
use crate::domain::model::{HttpMethod, PathSuffixMode};
use crate::error::{OagwError, OagwResult};

/// Header a client sets to pin the target endpoint of a pool (ADR-0001).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// A route matched for a proxy request, plus the part of the request path the
/// route's match prefix does not cover.
#[derive(Debug)]
pub struct RouteSelection<'a> {
    /// Route that matched.
    pub route: &'a Route,
    /// `http` branch of the matched rule; only `http` routes are selectable.
    pub http: &'a HttpMatch,
    /// Request path beyond the matched prefix, without its leading slash.
    pub suffix: String,
}

/// Select the route for a proxy request (DESIGN §3.2 "Request Routing").
///
/// A candidate must be enabled, expose an `http` match rule, allow the request
/// method and own a prefix of the request path. The longest prefix wins; ties
/// (same prefix, different rule shapes) are broken by the more specific rule
/// and finally by record id so the outcome is deterministic. A method that is
/// not in the allowlist removes the candidate, which surfaces as
/// `route.not_found.v1`: the guard table of DESIGN §3.2 lists it as a plain
/// rejection and 404 is the only documented code for a non-matching route.
#[must_use]
pub fn select_route<'a>(
    routes: &'a [Route],
    method: &http::Method,
    request_path: &str,
) -> Option<RouteSelection<'a>> {
    let candidate = HttpMethod::parse(method.as_str())?;
    routes
        .iter()
        .filter(|route| route.enabled)
        .filter_map(|route| {
            let http = route.match_rule.http.as_ref()?;
            if !http.methods.contains(&candidate) {
                return None;
            }
            let suffix = remainder_after_prefix(&http.path, request_path)?;
            Some((
                (http.path.len(), match_keys(&route.match_rule), route.id),
                route,
                http,
                suffix,
            ))
        })
        .max_by_key(|(score, route, _, _)| (*score, route.id))
        .map(|(_, route, http, suffix)| RouteSelection {
            route,
            http,
            suffix,
        })
}

/// Longest-prefix, segment-aware match.
///
/// `/v1` matches `/v1` and `/v1/chat` but not `/v1beta`; the returned value is
/// the uncovered remainder without the separating slash.
fn remainder_after_prefix(prefix: &str, request_path: &str) -> Option<String> {
    if request_path == prefix {
        return Some(String::new());
    }
    let boundary = format!("{prefix}/");
    request_path.strip_prefix(&boundary).map(str::to_owned)
}

/// Number of conditions a match rule expresses; a tie-break for equal paths.
fn match_keys(match_rule: &RouteMatch) -> usize {
    match match_rule {
        RouteMatch {
            http: Some(http),
            grpc: None,
        } => {
            1 + usize::from(http.methods.len() > 1) + usize::from(!http.query_allowlist.is_empty())
        }
        RouteMatch {
            http: None,
            grpc: Some(_),
        } => 1,
        RouteMatch { .. } => 0,
    }
}

/// Build the upstream request path from the match path and the raw suffix.
///
/// The suffix arrives **as the client sent it**, still percent-encoded: the
/// request URI is not decoded, and every segment is decoded, checked and
/// encoded again. That is what keeps a decoded `..` from escaping the matched
/// prefix and a `%3F` or `%23` from injecting a query or a fragment behind the
/// route's back (DESIGN §4.4, fail closed).
///
/// # Errors
/// 400 when the route rejects path suffixes (`path_suffix_mode: disabled`) and
/// a non-empty suffix was requested (DESIGN §3.2 "Guard Rules"), or when a
/// suffix segment cannot be rebuilt safely.
pub fn upstream_path(match_rule: &HttpMatch, suffix: &str) -> OagwResult<String> {
    let segments = suffix_segments(suffix)?;
    if !matches!(match_rule.path_suffix_mode, PathSuffixMode::Append) && !segments.is_empty() {
        return Err(OagwError::validation(format!(
            "route '{}' does not accept a path suffix",
            match_rule.path
        ))
        .with_extension(|ext| ext.invalid_value = Some(suffix.to_owned())));
    }
    let base = encode_path(&match_rule.path);
    let mut path = base;
    for segment in segments {
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(&segment);
    }
    Ok(path)
}

/// Rebuild the segments of a raw, still-encoded suffix.
///
/// An empty suffix has no segments: the request addressed the match path
/// itself, which is valid for every `path_suffix_mode`.
fn suffix_segments(suffix: &str) -> OagwResult<Vec<String>> {
    if suffix.is_empty() {
        return Ok(Vec::new());
    }
    suffix
        .split('/')
        .map(|segment| {
            let decoded = percent_decode(segment);
            if matches!(decoded.as_str(), "." | "..") {
                return Err(
                    OagwError::validation("path suffix must not contain a dot segment")
                        .with_extension(|ext| ext.invalid_value = Some(segment.to_owned())),
                );
            }
            if decoded.contains(['?', '#']) {
                return Err(OagwError::validation(
                    "path suffix must not carry a query or a fragment",
                )
                .with_extension(|ext| ext.invalid_value = Some(segment.to_owned())));
            }
            Ok(encode_path_segment(&decoded))
        })
        .collect()
}

/// Percent-encode every segment of a route's match path.
fn encode_path(path: &str) -> String {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let encoded: Vec<String> = trimmed.split('/').map(encode_path_segment).collect();
    format!("/{}", encoded.join("/"))
}

/// Bytes RFC 3986 allows in a URL path without escaping.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encode one path segment.
///
/// Only the unreserved set survives, so `/`, `?`, `#` and every control or
/// whitespace byte stay escaped: a segment can never change the shape of the
/// dial target by being re-read.
fn encode_path_segment(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = Vec::with_capacity(segment.len());
    for byte in segment.bytes() {
        if is_unreserved(byte) {
            encoded.push(byte);
        } else {
            encoded.push(b'%');
            encoded.push(HEX[usize::from(byte >> 4)]);
            encoded.push(HEX[usize::from(byte & 0x0F)]);
        }
    }
    String::from_utf8_lossy(&encoded).into_owned()
}

/// Decode the `%XX` escapes of a raw path segment.
///
/// An incomplete or non-hexadecimal escape is copied through: `Url::parse`
/// rejects it later, and a copied `%` can only ever shorten the candidate set
/// of dial targets, never widen it.
#[must_use]
pub fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%'
            && let Some(escape) = decode_escape(bytes, index)
        {
            decoded.push(escape);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Value of the `%XX` escape that starts at `index`, `None` when either digit
/// is missing or not a hexadecimal digit.
fn decode_escape(bytes: &[u8], index: usize) -> Option<u8> {
    let high = decode_hex_digit(*bytes.get(index + 1)?)?;
    let low = decode_hex_digit(*bytes.get(index + 2)?)?;
    Some((high << 4) | low)
}

/// Numeric value of one hexadecimal byte, `None` when it is not a hex digit.
fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `host` or `host:port` of an endpoint, eliding the scheme's default port
/// (DESIGN §3.2 "Standard ports").
#[must_use]
pub fn authority(endpoint: &Endpoint) -> String {
    alias::endpoint_alias_key(endpoint)
}

/// Assemble, parse and re-verify the absolute upstream URL.
///
/// The assembled string is parsed again with `url::Url` **and then checked
/// against what was assembled**: the scheme and the authority must be the
/// endpoint's, the parsed path must still carry the route prefix and the
/// parsed query must still be exactly the filtered one. A percent-escaped byte
/// that survives route matching therefore cannot smuggle a different
/// authority, scheme, path or query into the dial (DESIGN §4.4, fail closed).
///
/// # Errors
/// 400 when the assembled URL does not parse or when the re-verification
/// disagrees with the parts it was built from.
pub fn target_url(
    endpoint: &Endpoint,
    route_path: &str,
    path: &str,
    query: Option<&str>,
) -> OagwResult<Url> {
    let scheme = endpoint.scheme.as_str();
    let host = authority(endpoint);
    let raw = match query {
        Some(query) if !query.is_empty() => format!("{scheme}://{host}{path}?{query}"),
        _ => format!("{scheme}://{host}{path}"),
    };
    let url = Url::parse(&raw).map_err(|error| {
        let redacted = redacted_target(scheme, &host, path, query);
        tracing::debug!(target = %redacted, %error, "assembled upstream URL is not valid");
        OagwError::validation("assembled upstream URL is not a valid URL").with_extension(|ext| {
            ext.invalid_value = Some(redacted);
        })
    })?;
    verify_target(&url, endpoint, route_path, query)?;
    Ok(url)
}

/// A dial target with every query **value** elided.
///
/// The assembled URL can carry an injected credential: an auth plugin with a
/// `query_param` binding writes the resolved API key into the query, and the
/// URL is finalised after the plugin phase. A failing dial target is therefore
/// never echoed — not into a problem document, not into a log line — it only
/// ever names the parameters it would have carried.
fn redacted_target(scheme: &str, host: &str, path: &str, query: Option<&str>) -> String {
    let names = query.map_or_else(Vec::new, |query| {
        form_urlencoded::parse(query.as_bytes())
            .map(|(name, _)| name.into_owned())
            .collect::<Vec<_>>()
    });
    let query = names
        .iter()
        .map(|name| format!("{name}=<elided>"))
        .collect::<Vec<_>>()
        .join("&");
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    format!("{scheme}://{host}{path}{query}")
}

/// [`redacted_target`] of an already parsed dial target.
fn redacted_url(url: &Url) -> String {
    redacted_target(
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.path(),
        url.query(),
    )
}

/// Re-verification of a parsed dial target.
fn verify_target(
    url: &Url,
    endpoint: &Endpoint,
    route_path: &str,
    query: Option<&str>,
) -> OagwResult<()> {
    let rejected = |detail: &'static str, invalid: String| {
        OagwError::validation(detail).with_extension(|ext| ext.invalid_value = Some(invalid))
    };
    let port = if endpoint.port == endpoint.scheme.default_port() {
        None
    } else {
        Some(endpoint.port)
    };
    let authority = url
        .host_str()
        .is_some_and(|host| host == alias::normalize(&endpoint.host))
        && url.port() == port
        && url.scheme() == endpoint.scheme.as_str();
    let prefix = encode_path(route_path);
    if !authority {
        return Err(rejected(
            "assembled upstream URL does not address the selected endpoint",
            redacted_url(url),
        ));
    }
    if !url.path().starts_with(&prefix) {
        return Err(rejected(
            "assembled upstream URL escaped the matched route path",
            redacted_url(url),
        ));
    }
    if !query_survived(url, query) {
        return Err(rejected(
            "assembled upstream URL carries a query the route did not allow",
            redacted_url(url),
        ));
    }
    Ok(())
}

/// Whether the parsed URL kept exactly the parameters the filter allowed.
///
/// Compared by decoded parameter names, in order: `url` re-encodes what it is
/// given, so the spelling may differ while the set may not.
fn query_survived(url: &Url, filtered: Option<&str>) -> bool {
    let names = |query: &str| -> Vec<String> {
        form_urlencoded::parse(query.as_bytes())
            .map(|(name, _)| name.into_owned())
            .collect()
    };
    match (url.query(), filtered) {
        (None, None) => true,
        (Some(actual), Some(expected)) => names(actual) == names(expected),
        _ => false,
    }
}

/// Keep only the allowlisted query parameters (DESIGN §3.2 "Query allowlist").
///
/// An empty allowlist drops every parameter, an absent query stays absent, and
/// the surviving segments keep their original spelling (order, encoding and
/// duplicate names included). Unknown parameters are dropped rather than
/// rejected: the guard table of DESIGN §3.2 names a rejection, but a proxy that
/// filters is the only behaviour the §3.2 "Transformation Rules" row
/// ("Passthrough allowed params") can be read as.
#[must_use]
pub fn filter_query(allowlist: &[String], query: Option<&str>) -> Option<String> {
    let query = query?;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|segment| !segment.is_empty())
        .filter(|segment| is_allowed(allowlist, segment))
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

/// Whether the decoded name of a raw query segment is in the allowlist.
fn is_allowed(allowlist: &[String], segment: &str) -> bool {
    let name = form_urlencoded::parse(segment.as_bytes())
        .next()
        .map_or_else(String::new, |(decoded, _)| decoded.into_owned());
    allowlist.iter().any(|allowed| allowed == &name)
}

/// Normalized value of [`TARGET_HOST_HEADER`].
///
/// The header selects an endpoint by host, so anything that is not a bare host
/// or IP literal — a `host:port` pair, a path, a wildcard — is rejected
/// (ADR-0001 example: `us.vendor.com:8443`).
///
/// # Errors
/// [`crate::error::OagwErrorKind::InvalidTargetHost`] for a value that cannot
/// be a host.
pub fn validate_target_host(value: &str) -> OagwResult<String> {
    let candidate = value.trim();
    if candidate.is_empty() || alias::classify_host(candidate) == alias::HostKind::Invalid {
        return Err(OagwError::new(
            crate::error::OagwErrorKind::InvalidTargetHost,
            format!("'{value}' is not a host name or IP address"),
        )
        .with_extension(|ext| {
            ext.invalid_value = Some(value.to_owned());
        }));
    }
    Ok(alias::normalize(candidate))
}

/// Index of the endpoint that owns `host`, compared case-insensitively.
#[must_use]
pub fn endpoint_index_for_host(endpoints: &[Endpoint], host: &str) -> Option<usize> {
    let wanted = alias::normalize(host);
    endpoints
        .iter()
        .position(|endpoint| alias::normalize(&endpoint.host) == wanted)
}

/// Whether the alias only names a shared suffix of several hosts, which makes
/// the target endpoint ambiguous (ADR-0001).
///
/// A pool with a single endpoint is never ambiguous; a pool whose alias was
/// set explicitly to a full host name is not either.
#[must_use]
pub fn requires_target_host(upstream: &Upstream) -> bool {
    if upstream.endpoints.len() < 2 {
        return false;
    }
    alias::derive_alias(&upstream.endpoints).is_ok_and(|derived| derived == upstream.alias)
}

/// Why an endpoint was chosen (DESIGN §4.2 `selection_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// `X-OAGW-Target-Host` named it.
    ExplicitHeader,
    /// The pool's round-robin cursor picked it.
    RoundRobin,
    /// The pool holds a single endpoint, so there was nothing to select.
    Default,
}

impl SelectionMethod {
    /// Label value of the routing metric.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// The endpoint a request dials, and how it was chosen.
#[derive(Debug)]
pub struct Selection<'a> {
    /// The endpoint to dial.
    pub endpoint: &'a Endpoint,
    /// What picked it (DESIGN §4.2 `selection_method`).
    pub method: SelectionMethod,
}

/// Resolve the endpoint to dial (ADR-0001 "X-OAGW-Target-Host" matrix).
///
/// An explicit header is validated and must name a configured endpoint, no
/// matter how large the pool is. Without a header an ambiguous pool is
/// rejected, and an unambiguous pool is selected by `round_robin`, which
/// yields the next index of the pool; a pool of one is reported as
/// [`SelectionMethod::Default`], because a cursor over a single entry made no
/// decision.
///
/// # Errors
/// * [`crate::error::OagwErrorKind::InvalidTargetHost`] for a malformed header
/// * [`crate::error::OagwErrorKind::UnknownTargetHost`] when the header names
///   no configured endpoint
/// * [`crate::error::OagwErrorKind::MissingTargetHost`] when the pool is
///   ambiguous and no header was sent
/// * 400 when the upstream carries no endpoint at all
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    header: Option<&str>,
    round_robin: impl FnOnce() -> usize,
) -> OagwResult<Selection<'a>> {
    let endpoints = &upstream.endpoints;
    if endpoints.is_empty() {
        return Err(OagwError::validation(
            "upstream carries no endpoint; cannot proxy",
        ));
    }
    let Some(value) = header else {
        if requires_target_host(upstream) {
            return Err(missing_target_host(upstream));
        }
        let method = if endpoints.len() == 1 {
            SelectionMethod::Default
        } else {
            SelectionMethod::RoundRobin
        };
        let index = round_robin() % endpoints.len();
        return Ok(Selection {
            endpoint: &endpoints[index],
            method,
        });
    };
    let host = validate_target_host(value)?;
    let index = endpoint_index_for_host(endpoints, &host).ok_or_else(|| {
        let hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();
        OagwError::new(
            crate::error::OagwErrorKind::UnknownTargetHost,
            format!(
                "'{host}' matches no endpoint of upstream '{}'",
                upstream.alias
            ),
        )
        .with_extension(|ext| {
            ext.alias = Some(upstream.alias.clone());
            ext.invalid_value = Some(host.clone());
            ext.valid_hosts = Some(hosts);
        })
    })?;
    Ok(Selection {
        endpoint: &endpoints[index],
        method: SelectionMethod::ExplicitHeader,
    })
}

/// 400 `routing.missing_target_host.v1` with the pool that would be valid.
fn missing_target_host(upstream: &Upstream) -> OagwError {
    let hosts: Vec<String> = upstream
        .endpoints
        .iter()
        .map(|endpoint| endpoint.host.clone())
        .collect();
    OagwError::new(
        crate::error::OagwErrorKind::MissingTargetHost,
        format!(
            "upstream '{}' pools several endpoints; set {}",
            upstream.alias, TARGET_HOST_HEADER
        ),
    )
    .with_extension(|ext| {
        ext.alias = Some(upstream.alias.clone());
        ext.valid_hosts = Some(hosts);
    })
}

/// Whether an endpoint dials a plaintext connection while the deployment only
/// allows TLS.
///
/// The outbound client refuses such a dial as well; this check runs first so
/// the problem carries the endpoint that was refused.
#[must_use]
pub fn plaintext_disallowed(endpoint: &Endpoint, allow_http_upstream: bool) -> bool {
    endpoint.scheme.is_plaintext() && !allow_http_upstream
}

#[cfg(test)]
mod tests {
    use http::Method;
    use url::Url;

    use super::{
        SelectionMethod, authority, endpoint_index_for_host, filter_query, remainder_after_prefix,
        requires_target_host, select_endpoint, select_route, target_url, upstream_path,
        validate_target_host,
    };
    use crate::config::SsrfPolicy;
    use crate::domain::model::{
        Endpoint, HttpMatch, HttpMethod, PathSuffixMode, Route, RouteMatch, Scheme, Timestamps,
        Upstream,
    };
    use crate::error::{OagwErrorKind, ResourceKind};

    const TENANT: uuid::Uuid = uuid::Uuid::nil();

    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(endpoints: Vec<Endpoint>, alias: &str) -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: TENANT,
            alias: alias.to_owned(),
            enabled: true,
            protocol: crate::domain::model::Protocol::Http,
            endpoints,
            tags: Vec::new(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            timestamps: Timestamps {
                created_at: 0,
                updated_at: 0,
            },
        }
    }

    fn route(match_rule: RouteMatch) -> Route {
        Route {
            id: uuid::Uuid::new_v4(),
            tenant_id: TENANT,
            upstream_id: uuid::Uuid::new_v4(),
            enabled: true,
            match_rule,
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
            cors: None,
            timestamps: Timestamps {
                created_at: 0,
                updated_at: 0,
            },
        }
    }

    fn http_route(path: &str, methods: &[HttpMethod], suffix_mode: PathSuffixMode) -> Route {
        route(RouteMatch {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: suffix_mode,
            }),
            grpc: None,
        })
    }

    fn get(path: &str) -> Route {
        http_route(path, &[HttpMethod::Get], PathSuffixMode::Append)
    }

    #[test]
    fn matches_the_longest_prefix() {
        let deep = get("/v1/chat");
        let deep_id = deep.id;
        let routes = [get("/v1"), deep];
        let selected = select_route(&routes, &Method::GET, "/v1/chat/completions").unwrap();
        assert_eq!(selected.route.id, deep_id);
        assert_eq!(selected.suffix, "completions");
    }

    #[test]
    fn does_not_match_a_prefix_that_is_not_a_segment_boundary() {
        let route = get("/v1");
        let routes = [route];
        assert!(select_route(&routes, &Method::GET, "/v1beta").is_none());
        assert!(select_route(&routes, &Method::GET, "/v1").is_some());
    }

    #[test]
    fn rejects_a_method_outside_the_allowlist() {
        let route = get("/v1");
        let routes = [route];
        assert!(select_route(&routes, &Method::POST, "/v1").is_none());
        // HEAD is not a routable method of the schema: 404, not a fallback.
        assert!(select_route(&routes, &Method::HEAD, "/v1").is_none());
    }

    #[test]
    fn ignores_disabled_routes() {
        let mut route = get("/v1");
        route.enabled = false;
        let routes = [route];
        assert!(select_route(&routes, &Method::GET, "/v1").is_none());
    }

    #[test]
    fn ignores_grpc_only_routes() {
        let route = route(RouteMatch {
            http: None,
            grpc: Some(crate::domain::model::GrpcMatch {
                service: "pkg.Svc".to_owned(),
                method: "Get".to_owned(),
            }),
        });
        let routes = [route];
        assert!(select_route(&routes, &Method::GET, "/v1").is_none());
    }

    #[test]
    fn empty_suffix_is_the_exact_path() {
        let route = get("/v1/chat");
        let routes = [route];
        let selected = select_route(&routes, &Method::GET, "/v1/chat").unwrap();
        assert_eq!(selected.suffix, "");
    }

    #[test]
    fn appends_the_suffix_to_the_match_path() {
        let match_rule = HttpMatch {
            methods: vec![HttpMethod::Get],
            path: "/v1/chat".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        };
        assert_eq!(
            upstream_path(&match_rule, "completions").unwrap(),
            "/v1/chat/completions"
        );
        assert_eq!(upstream_path(&match_rule, "").unwrap(), "/v1/chat");
    }

    #[test]
    fn appends_without_a_double_slash() {
        let match_rule = HttpMatch {
            methods: vec![HttpMethod::Get],
            path: "/v1/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        };
        assert_eq!(upstream_path(&match_rule, "chat").unwrap(), "/v1/chat");
        assert_eq!(upstream_path(&match_rule, "chat/").unwrap(), "/v1/chat/");
    }

    #[test]
    fn rejects_a_suffix_on_a_disabled_mode() {
        let match_rule = HttpMatch {
            methods: vec![HttpMethod::Get],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Disabled,
        };
        let error = upstream_path(&match_rule, "chat").unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
        assert_eq!(upstream_path(&match_rule, "").unwrap(), "/v1");
    }

    /// A `..` segment — plain or escaped — must not escape the matched prefix.
    #[test]
    fn rejects_a_dot_segment_in_the_suffix() {
        let rule = append_rule();
        for suffix in ["../admin", "%2E%2E/admin", "chat/../../admin", "."] {
            let error = upstream_path(&rule, suffix).unwrap_err();
            assert_eq!(error.kind(), &OagwErrorKind::Validation, "{suffix}");
        }
    }

    /// An escaped `?` or `#` may not become a query or a fragment of the dial
    /// target: the route's query allowlist is the only way to add one.
    #[test]
    fn rejects_a_query_or_fragment_in_the_suffix() {
        let rule = append_rule();
        for suffix in ["chat%3Finjected%3D1", "chat%23fragment", "a%3Fb/c"] {
            let error = upstream_path(&rule, suffix).unwrap_err();
            assert_eq!(error.kind(), &OagwErrorKind::Validation, "{suffix}");
        }
    }

    /// A segment that decodes to a separator stays escaped, so `%2F` cannot
    /// become a second path segment behind the route's back.
    #[test]
    fn re_encodes_an_escaped_separator() {
        let rule = append_rule();
        assert_eq!(upstream_path(&rule, "a%2Fb").unwrap(), "/v1/chat/a%2Fb");
        assert_eq!(upstream_path(&rule, "a b").unwrap(), "/v1/chat/a%20b");
    }

    fn append_rule() -> HttpMatch {
        HttpMatch {
            methods: vec![HttpMethod::Get],
            path: "/v1/chat".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }

    #[test]
    fn elides_the_standard_port() {
        assert_eq!(authority(&endpoint(Scheme::Https, "a.b", 443)), "a.b");
        assert_eq!(authority(&endpoint(Scheme::Http, "a.b", 80)), "a.b");
        assert_eq!(authority(&endpoint(Scheme::Https, "a.b", 8443)), "a.b:8443");
    }

    #[test]
    fn builds_a_url_from_the_endpoint() {
        let url = target_url(
            &endpoint(Scheme::Http, "127.0.0.1", 8099),
            "/v1",
            "/v1/x",
            Some("a=1"),
        )
        .unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:8099/v1/x?a=1");
        let plain = target_url(&endpoint(Scheme::Https, "a.b", 443), "/v1", "/v1", None).unwrap();
        assert_eq!(plain.as_str(), "https://a.b/v1");
    }

    #[test]
    fn rejects_a_url_that_cannot_be_parsed() {
        let error = target_url(
            &endpoint(Scheme::Https, "bad host", 443),
            "/v1",
            "/v1",
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
        assert!(error.extensions().invalid_value.is_some());
    }

    /// The re-verification is the last line of defence: a dial target that
    /// drifted from the parts it was built from is a 400, never a dial.
    #[test]
    fn rejects_a_url_that_escaped_its_parts() {
        let target = endpoint(Scheme::Http, "a.b", 80);
        // A path that does not carry the matched route prefix.
        let error = target_url(&target, "/v1", "/other", None).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
        // An authority that is not the selected endpoint's.
        let url = Url::parse("http://elsewhere.example/v1")
            .unwrap_or_else(|error| panic!("the fixture URL must parse: {error}"));
        let error = super::verify_target(&url, &target, "/v1", None).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
    }

    /// The query re-verification compares decoded parameter names, so a URL
    /// that re-encoded its values still matches what the filter produced.
    #[test]
    fn the_query_re_verification_compares_parameter_names() {
        let url = Url::parse("http://a.b/v1?a%20b=1&c=2")
            .unwrap_or_else(|error| panic!("the fixture URL must parse: {error}"));
        assert!(super::query_survived(&url, Some("a b=1&c=2")));
        assert!(!super::query_survived(&url, Some("a%20b=1")));
        assert!(!super::query_survived(&url, None));
        let empty = Url::parse("http://a.b/v1")
            .unwrap_or_else(|error| panic!("the fixture URL must parse: {error}"));
        assert!(super::query_survived(&empty, None));
        assert!(!super::query_survived(&empty, Some("a=1")));
    }

    /// A percent-encoded segment reaches the upstream with its escapes intact.
    #[test]
    fn url_round_trips_a_percent_encoded_path() {
        let url: Url = target_url(
            &endpoint(Scheme::Http, "h", 80),
            "/v1",
            "/v1/a%2Fb%20c",
            None,
        )
        .unwrap();
        assert_eq!(url.path(), "/v1/a%2Fb%20c");
    }

    /// A query the URL re-encoded keeps its parameter names.
    #[test]
    fn url_keeps_a_re_encoded_query() {
        let url: Url = target_url(
            &endpoint(Scheme::Http, "h", 80),
            "/v1",
            "/v1",
            Some("a b=1&c=2"),
        )
        .unwrap();
        assert_eq!(url.query(), Some("a%20b=1&c=2"));
    }

    #[test]
    fn keeps_the_allowlisted_query_parameters() {
        let allowlist = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(
            filter_query(&allowlist, Some("a=1&c=2&b=3&d=4")).as_deref(),
            Some("a=1&b=3")
        );
        assert_eq!(filter_query(&allowlist, Some("c=2")), None);
        assert_eq!(filter_query(&allowlist, None), None);
        assert_eq!(filter_query(&[], Some("a=1")), None);
    }

    #[test]
    fn keeps_encoded_and_repeated_parameters() {
        let allowlist = vec!["a b".to_owned()];
        assert_eq!(
            filter_query(&allowlist, Some("a%20b=1&a%20b=2")).as_deref(),
            Some("a%20b=1&a%20b=2")
        );
    }

    #[test]
    fn accepts_only_bare_hosts_as_target() {
        assert_eq!(
            validate_target_host("Us.Vendor.com").unwrap(),
            "us.vendor.com"
        );
        assert_eq!(validate_target_host("10.0.1.2").unwrap(), "10.0.1.2");
        for value in ["us.vendor.com:8443", "/v1", "a b", ""] {
            assert_eq!(
                validate_target_host(value).unwrap_err().kind(),
                &OagwErrorKind::InvalidTargetHost
            );
        }
    }

    #[test]
    fn finds_an_endpoint_by_host_case_insensitively() {
        let endpoints = [
            endpoint(Scheme::Https, "us.vendor.com", 443),
            endpoint(Scheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(
            endpoint_index_for_host(&endpoints, "EU.VENDOR.com"),
            Some(1)
        );
        assert_eq!(endpoint_index_for_host(&endpoints, "ap.vendor.com"), None);
    }

    #[test]
    fn suffix_alias_requires_a_target_host() {
        let pool = upstream(
            vec![
                endpoint(Scheme::Https, "us.vendor.com", 443),
                endpoint(Scheme::Https, "eu.vendor.com", 443),
            ],
            "vendor.com",
        );
        assert!(requires_target_host(&pool));
        let error = select_endpoint(&pool, None, || 0).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::MissingTargetHost);
        assert!(error.extensions().valid_hosts.is_some());
    }

    #[test]
    fn single_endpoint_never_needs_a_target_host() {
        let single = upstream(
            vec![endpoint(Scheme::Https, "api.vendor.com", 443)],
            "api.vendor.com",
        );
        assert!(!requires_target_host(&single));
        let picked = select_endpoint(&single, None, || 3).unwrap();
        assert_eq!(picked.endpoint.host, "api.vendor.com");
        assert_eq!(picked.method, SelectionMethod::Default);
    }

    #[test]
    fn explicit_alias_of_a_full_host_does_not_need_a_target_host() {
        let pool = upstream(
            vec![
                endpoint(Scheme::Https, "us.vendor.com", 443),
                endpoint(Scheme::Https, "eu.vendor.com", 443),
            ],
            "pool",
        );
        assert!(!requires_target_host(&pool));
    }

    #[test]
    fn ip_pool_alias_does_not_need_a_target_host() {
        let pool = upstream(
            vec![
                endpoint(Scheme::Https, "10.0.1.1", 443),
                endpoint(Scheme::Https, "10.0.1.2", 443),
            ],
            "payment-pool",
        );
        assert!(!requires_target_host(&pool));
    }

    #[test]
    fn unknown_target_host_lists_the_valid_hosts() {
        let pool = upstream(
            vec![
                endpoint(Scheme::Https, "us.vendor.com", 443),
                endpoint(Scheme::Https, "eu.vendor.com", 443),
            ],
            "vendor.com",
        );
        let error = select_endpoint(&pool, Some("ap.vendor.com"), || 0).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::UnknownTargetHost);
        assert_eq!(
            error.extensions().invalid_value.as_deref(),
            Some("ap.vendor.com")
        );
    }

    #[test]
    fn target_host_pins_the_endpoint_in_any_pool_size() {
        let pool = upstream(
            vec![
                endpoint(Scheme::Https, "us.vendor.com", 443),
                endpoint(Scheme::Https, "eu.vendor.com", 443),
            ],
            "vendor.com",
        );
        let pinned = select_endpoint(&pool, Some("eu.vendor.com"), || 0).unwrap();
        assert_eq!(pinned.endpoint.host, "eu.vendor.com");
        assert_eq!(pinned.method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn empty_pool_is_rejected() {
        let pool = upstream(Vec::new(), "empty");
        let error = select_endpoint(&pool, None, || 0).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
    }

    #[test]
    fn flags_plaintext_when_the_switch_is_off() {
        let http = endpoint(Scheme::Http, "a.b", 80);
        assert!(super::plaintext_disallowed(&http, false));
        assert!(!super::plaintext_disallowed(&http, true));
    }

    #[test]
    fn remainder_needs_a_full_segment() {
        assert_eq!(remainder_after_prefix("/v1", "/v1/x").as_deref(), Some("x"));
        assert_eq!(remainder_after_prefix("/v1", "/v1").as_deref(), Some(""));
        assert!(remainder_after_prefix("/v1", "/v1x").is_none());
    }

    #[test]
    fn problem_extension_carries_the_alias() {
        let pool = upstream(
            vec![
                endpoint(Scheme::Https, "us.vendor.com", 443),
                endpoint(Scheme::Https, "eu.vendor.com", 443),
            ],
            "vendor.com",
        );
        let error = select_endpoint(&pool, None, || 0).unwrap_err();
        assert_eq!(error.extensions().alias.as_deref(), Some("vendor.com"));
        assert_eq!(
            error.gts_type(),
            crate::error::OagwErrorKind::MissingTargetHost.gts_type(ResourceKind::Upstream)
        );
    }

    #[test]
    fn ssrf_policy_stays_out_of_the_data_plane() {
        // The write path validates the SSRF lists; the data plane re-checks
        // them at dial time (`check_egress`), so no routing decision here
        // needs the policy.
        let _policy = SsrfPolicy::default();
    }
}
