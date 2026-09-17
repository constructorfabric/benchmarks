//! Target-host selection and header rewriting for the proxy data plane.
//!
//! OAGW sits between an internal caller and an external upstream, so every
//! header it handles belongs to one of three categories (DESIGN.md §3.2
//! "Headers Transformation"):
//!
//! 1. **Routing headers** — consumed here and never forwarded
//!    (`X-OAGW-Target-Host`).
//! 2. **Hop-by-hop headers** — stripped in both directions (R8).
//! 3. **Passthrough headers** — forwarded according to the upstream
//!    `headers.request.passthrough` mode and its set/add/remove rules (R9).
//!
//! This module also owns the `X-OAGW-Target-Host` behaviour matrix (R6), the
//! `Host` rewrite (R7) and the CORS response headers of ADR 0004.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::alias::compute_derived_alias;
use crate::domain::model::{CorsConfig, Endpoint, Host, HttpMethod, PassthroughMode, Upstream};
use crate::error::{GatewayError, GatewayErrorKind};

/// Routing header that selects one endpoint of a multi-endpoint upstream.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// [`TARGET_HOST_HEADER`] as a [`HeaderName`], parsed once.
pub static TARGET_HOST_HEADER_NAME: LazyLock<HeaderName> =
    LazyLock::new(|| HeaderName::from_static(TARGET_HOST_HEADER));

/// The hop-by-hop headers stripped from the forwarded request and from the
/// response returned to the client (R8). WebSocket upgrade is Phase 5.
pub static HOP_BY_HOP_HEADERS: LazyLock<[HeaderName; 8]> = LazyLock::new(|| {
    [
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ]
});

/// Round-robin cursor of every multi-endpoint upstream, shared by the requests
/// of one [`crate::proxy::ProxyService`].
///
/// Cursors are keyed by upstream id and never reset: a pool is served in
/// rotation for the lifetime of the process, which is what DESIGN.md §3.2
/// "Multi-Endpoint Load Balancing" describes.
#[derive(Debug, Default)]
pub struct RoundRobin {
    /// `upstream id -> next endpoint index`.
    cursors: DashMap<Uuid, AtomicUsize>,
}

impl RoundRobin {
    /// The index of the endpoint that serves the next request of `upstream_id`.
    ///
    /// `len` is the number of endpoints, always at least one.
    #[must_use]
    pub fn next_index(&self, upstream_id: Uuid, len: usize) -> usize {
        if len <= 1 {
            return 0;
        }

        let cursor = self.cursors.entry(upstream_id).or_default();
        let previous = cursor.fetch_add(1, Ordering::Relaxed);

        previous % len
    }
}

/// Removes the hop-by-hop headers (R8) from `headers`, in place.
///
/// Headers named by the `Connection` header are hop-by-hop as well (RFC 9110
/// §7.6.1), so they are stripped too.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let connection_named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .filter_map(|token| HeaderName::try_from(token.to_ascii_lowercase()).ok())
        .collect();

    for name in HOP_BY_HOP_HEADERS.iter().chain(connection_named.iter()) {
        headers.remove(name);
    }
}

/// Whether `upstream` declares its endpoints under a *common-suffix* alias,
/// i.e. an alias derived from the registrable domain the pool shares
/// (DESIGN.md §3.2 "Alias Enforcement Rules").
///
/// Such an alias is ambiguous — every endpoint of the pool answers for it — so
/// [`select_target`] requires `X-OAGW-Target-Host` to disambiguate.
#[must_use]
pub fn requires_target_host(upstream: &Upstream) -> bool {
    let endpoints = upstream.endpoints();

    endpoints.len() > 1
        && compute_derived_alias(endpoints).is_some_and(|derived| derived == upstream.alias())
}

/// The endpoint hosts of an upstream, in pool order.
fn valid_hosts(upstream: &Upstream) -> Vec<String> {
    upstream
        .endpoints()
        .iter()
        .map(|endpoint| endpoint.host.as_str().to_owned())
        .collect()
}

/// Selects the endpoint a request is forwarded to (R6).
///
/// # Errors
///
/// Returns the three 400 routing problems of ADR 0007 when
/// `X-OAGW-Target-Host` is required but absent, malformed, or matches no
/// configured endpoint.
pub fn select_target(
    upstream: &Upstream,
    target_host: Option<&HeaderValue>,
    round_robin: &RoundRobin,
) -> Result<Endpoint, GatewayError> {
    let endpoints = upstream.endpoints();

    // A single-endpoint upstream has nothing to disambiguate: the header is
    // ignored when present (R6).
    if endpoints.len() == 1 {
        return Ok(endpoints[0].clone());
    }

    let Some(raw) = target_host.and_then(|value| value.to_str().ok()) else {
        return if requires_target_host(upstream) {
            Err(missing_target_host(upstream))
        } else {
            Ok(next_endpoint(upstream, round_robin))
        };
    };

    let raw = raw.trim();
    let host = Host::parse(raw).map_err(|_| invalid_target_host(upstream, raw))?;

    endpoints
        .iter()
        .find(|endpoint| endpoint.host.as_str() == host.as_str())
        .cloned()
        .ok_or_else(|| unknown_target_host(upstream, host.as_str()))
}

/// The endpoint a multi-endpoint, explicit-alias upstream serves next.
fn next_endpoint(upstream: &Upstream, round_robin: &RoundRobin) -> Endpoint {
    let endpoints = upstream.endpoints();
    let index = round_robin.next_index(upstream.id, endpoints.len());

    endpoints[index].clone()
}

/// 400 `routing.missing_target_host.v1` (ADR 0007, Appendix A).
fn missing_target_host(upstream: &Upstream) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::MissingTargetHost,
        format!(
            "`{TARGET_HOST_HEADER}` is required for the multi-endpoint upstream with the \
             common-suffix alias `{}`. Valid hosts: {:?}",
            upstream.alias(),
            valid_hosts(upstream),
        ),
    )
    .with_upstream_id(upstream.id.to_string())
    .with_extension("alias", upstream.alias().to_owned())
    .with_extension("valid_hosts", valid_hosts(upstream))
}

/// 400 `routing.invalid_target_host.v1`: not a bare hostname or IP literal.
fn invalid_target_host(upstream: &Upstream, value: &str) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::InvalidTargetHost,
        format!(
            "`{TARGET_HOST_HEADER}` must be a bare hostname or IP address without a port, path \
             or special characters, got `{value}`"
        ),
    )
    .with_upstream_id(upstream.id.to_string())
    .with_extension("invalid_value", value.to_owned())
}

/// 400 `routing.unknown_target_host.v1`: no configured endpoint matches.
fn unknown_target_host(upstream: &Upstream, value: &str) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::UnknownTargetHost,
        format!(
            "`{TARGET_HOST_HEADER}` value `{value}` does not match any configured endpoint of \
             `{}`. Valid hosts: {:?}",
            upstream.alias(),
            valid_hosts(upstream),
        ),
    )
    .with_upstream_id(upstream.id.to_string())
    .with_extension("invalid_value", value.to_owned())
    .with_extension("valid_hosts", valid_hosts(upstream))
}

/// The `host[:port]` authority of an endpoint, used for both the outbound URL
/// and the rewritten `Host` header (R7). IPv6 literals are bracketed.
#[must_use]
pub fn authority(endpoint: &Endpoint) -> String {
    if endpoint.port == endpoint.scheme.standard_port() {
        return endpoint.host.as_str().to_owned();
    }

    endpoint.host.with_port(endpoint.port)
}

/// Builds the headers of the outgoing request (R7, R8, R9).
///
/// The inbound headers are filtered by the upstream `headers.request`
/// passthrough mode, the routing and hop-by-hop headers are dropped, the
/// upstream header rules are applied, and `Host` is rewritten to the target
/// endpoint. `content-type` always travels with the forwarded body, and
/// `content-length` is set from the body that is actually forwarded.
#[must_use]
pub fn build_request_headers(
    inbound: &HeaderMap,
    upstream: &Upstream,
    endpoint: &Endpoint,
    body_len: usize,
) -> HeaderMap {
    let request_rules = upstream
        .config
        .headers
        .as_ref()
        .and_then(|headers| headers.request.as_ref());

    let mut headers = passthrough_headers(inbound, request_rules);
    apply_rules(
        &mut headers,
        request_rules.map(|rules| (rules.remove.as_slice(), &rules.set, &rules.add)),
    );
    strip_hop_by_hop(&mut headers);
    headers.remove(TARGET_HOST_HEADER_NAME.as_str());
    headers.remove(header::HOST);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body_len));
    set_host(&mut headers, endpoint);

    headers
}

/// Rewrites the `Host` header to the target endpoint (R7).
///
/// The HTTP/2 `:authority` pseudo-header is not a header: it is carried by the
/// outbound URI, which [`crate::proxy::forward`] builds from the same
/// authority, so both HTTP/1.1 `Host` and HTTP/2 `:authority` name the upstream
/// host.
fn set_host(headers: &mut HeaderMap, endpoint: &Endpoint) {
    if let Ok(host) = HeaderValue::from_str(&authority(endpoint)) {
        headers.insert(header::HOST, host);
    }
}

/// The inbound headers that travel upstream, per `headers.request.passthrough`.
///
/// With no rules at all (and with [`PassthroughMode::None`], the default) only
/// the body's own media type is forwarded: DESIGN.md's table forwards nothing
/// unless the upstream asks for it.
fn passthrough_headers(inbound: &HeaderMap, rules: Option<&RequestRules>) -> HeaderMap {
    let Some(rules) = rules else {
        return keep(inbound, &[header::CONTENT_TYPE]);
    };

    match rules.passthrough {
        PassthroughMode::None => keep(inbound, &[header::CONTENT_TYPE]),
        PassthroughMode::Allowlist => {
            let allowlist: Vec<HeaderName> = rules
                .passthrough_allowlist
                .iter()
                .filter_map(|name| HeaderName::try_from(name.as_str()).ok())
                .chain(std::iter::once(header::CONTENT_TYPE))
                .collect();
            keep(inbound, &allowlist)
        }
        PassthroughMode::All => inbound.clone(),
    }
}

/// The request header rules of an upstream.
type RequestRules = crate::domain::model::RequestHeaders;

/// Keeps the named headers of `inbound`, in inbound order.
fn keep(inbound: &HeaderMap, names: &[HeaderName]) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(inbound.len());

    for name in names {
        for value in inbound.get_all(name) {
            headers.append(name, value.clone());
        }
    }

    headers
}

/// One direction of the upstream `headers` rules: the `remove` list, the `set`
/// map and the `add` map (R9).
type HeaderRules<'a> = (
    &'a [String],
    &'a BTreeMap<String, String>,
    &'a BTreeMap<String, String>,
);

/// Applies one direction's `set` / `add` / `remove` rules (R9).
fn apply_rules(headers: &mut HeaderMap, rules: Option<HeaderRules<'_>>) {
    let Some((remove, set, add)) = rules else {
        return;
    };

    for name in remove {
        if let Ok(name) = HeaderName::try_from(name.as_str()) {
            headers.remove(name);
        }
    }

    for (name, value) in set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    for (name, value) in add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

/// Applies the upstream `headers.response` rules to the response returned to
/// the client, and strips its hop-by-hop headers (R8, R9).
pub fn build_response_headers(upstream: &Upstream, upstream_response: &mut HeaderMap) {
    strip_hop_by_hop(upstream_response);

    let Some(rules) = upstream
        .config
        .headers
        .as_ref()
        .and_then(|headers| headers.response.as_ref())
    else {
        return;
    };

    for name in &rules.remove {
        if let Ok(name) = HeaderName::try_from(name.as_str()) {
            upstream_response.remove(name);
        }
    }

    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            upstream_response.insert(name, value);
        }
    }

    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            upstream_response.append(name, value);
        }
    }
}

/// Whether `request` is a CORS preflight (ADR 0004 "Preflight Request
/// Handling"): `OPTIONS` carrying both `Origin` and
/// `Access-Control-Request-Method`.
#[must_use]
pub fn is_preflight(method: &axum::http::Method, headers: &HeaderMap) -> bool {
    method == axum::http::Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

/// Validates the origin and the method of an actual cross-origin request and
/// returns the CORS response headers (ADR 0004).
///
/// A CORS configuration that is not enabled applies no CORS logic at all, so a
/// gateway with CORS left off never rejects a caller for its `Origin` header.
///
/// # Errors
///
/// Returns a 403 [`GatewayError`] when the origin or the method is not allowed.
pub fn check_cors(
    cors: &CorsConfig,
    origin: Option<&HeaderValue>,
    method: HttpMethod,
) -> Result<Vec<(HeaderName, HeaderValue)>, GatewayError> {
    if !cors.enabled {
        return Ok(Vec::new());
    }

    let Some(origin) = origin.and_then(|value| value.to_str().ok()) else {
        return Ok(Vec::new());
    };

    if !origin_allowed(cors, origin) {
        return Err(GatewayError::new(
            GatewayErrorKind::CorsOriginNotAllowed,
            format!("origin `{origin}` is not an allowed origin of this upstream"),
        )
        .with_extension("origin", origin.to_owned()));
    }

    if !cors.allowed_methods.contains(&method) {
        return Err(GatewayError::new(
            GatewayErrorKind::CorsMethodNotAllowed,
            format!("method `{method}` is not an allowed method of this upstream"),
        )
        .with_extension("origin", origin.to_owned()));
    }

    Ok(cors_headers(cors, origin))
}

fn origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    cors.allows_any_origin() || cors.allowed_origins.iter().any(|allowed| allowed == origin)
}

/// The CORS response headers of an allowed cross-origin request.
fn cors_headers(cors: &CorsConfig, origin: &str) -> Vec<(HeaderName, HeaderValue)> {
    let Ok(allow_origin) = HeaderValue::from_str(origin) else {
        return Vec::new();
    };

    let mut headers = vec![
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin),
        (header::VARY, HeaderValue::from_static("Origin")),
    ];

    if cors.allow_credentials {
        headers.push((
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        ));
    }

    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.push((header::ACCESS_CONTROL_EXPOSE_HEADERS, value));
    }

    headers
}
