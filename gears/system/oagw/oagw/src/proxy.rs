// Created: 2026-09-03 by Constructor Tech
//! The data plane: reverse proxy over `/oagw/v1/proxy/{alias}/...`.
//!
//! The pipeline is alias resolution, endpoint selection, route matching,
//! CORS, rate limiting, guard plugins, header transformation, credential
//! injection and dispatch, followed by the response-side counterparts.
//! Gateway errors are rendered as RFC 9457 problem documents and never
//! rewrite upstream responses (`ADR/0007-error-source-distinction.md`).

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use http::header::{HeaderMap, HeaderName, HeaderValue, CONNECTION, UPGRADE};
use http::{Method, StatusCode, Uri};

use crate::body::{Limited, ProxyBody};
use crate::error::{ErrorKind, OagwError, ERROR_SOURCE_UPSTREAM, TRACE_ID_HEADER};
use crate::model::{
    CorsConfig, Endpoint, EndpointScheme, PassthroughMode, PathSuffixMode, PluginItem, PluginType,
    RateLimitConfig, Route, Upstream,
};
use crate::plugins::{self, ResolvedPlugin};
use crate::rate_limit::RateDecision;
use crate::state::OagwState;

/// Directive selecting an endpoint of a multi-endpoint pool.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Header used to carry the client IP for IP-scoped rate limiting.
pub const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";

/// Hop-by-hop headers that are never forwarded.
const HOP_BY_HOP: [&str; 6] = [
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// The selected dial target of an upstream.
struct ProxyTarget {
    scheme: EndpointScheme,
    host: String,
    port: u16,
}

/// The phase of the exchange a plugin rule applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Request,
    Response,
}

/// The addressable parts of a proxied exchange.
struct ProxyRequest<'a> {
    method: &'a Method,
    alias: &'a str,
    proxy_path: &'a str,
    query: Option<String>,
    trace_id: &'a str,
    /// The inbound `Origin`, which drives the CORS response headers.
    origin: Option<String>,
}

impl ProxyRequest<'_> {
    /// The request URI used as the RFC 9457 `instance` member.
    fn instance(&self) -> String {
        format!("/oagw/v1/proxy/{}{}", self.alias, self.proxy_path)
    }
}

/// Entry point of the data plane.
///
/// Every failure is rendered as a problem document; upstream responses are
/// passed through untouched.
pub async fn handle(
    state: Arc<OagwState>,
    sec: toolkit_security::SecurityContext,
    method: Method,
    alias: String,
    proxy_path: String,
    query: Option<String>,
    incoming: axum::extract::Request,
) -> Response {
    let trace_id = crate::error::new_trace_id();
    let request = ProxyRequest {
        method: &method,
        alias: &alias,
        proxy_path: &proxy_path,
        query,
        trace_id: &trace_id,
        origin: header_str(incoming.headers(), http::header::ORIGIN),
    };
    let outcome = pipeline(&state, &sec, &request, incoming).await;
    match outcome {
        Ok(response) => response,
        Err(error) => error
            .with_instance(request.instance())
            .with_trace_id(trace_id)
            .into_response(),
    }
}

#[allow(clippy::too_many_lines)]
async fn pipeline(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    request: &ProxyRequest<'_>,
    mut incoming: axum::extract::Request,
) -> Result<Response, OagwError> {
    let (method, alias, proxy_path) = (request.method, request.alias, request.proxy_path);
    let upgrade_requested = is_upgrade_request(incoming.headers());
    let client_upgrade = upgrade_requested.then(|| hyper::upgrade::on(&mut incoming));
    let (parts, body) = incoming.into_parts();
    let inbound_headers = parts.headers.clone();

    // ADR 0004 "Preflight Request Handling": a preflight is answered at
    // handler level, before upstream, route or tenant resolution, and
    // permissively — the origin and method are validated on the actual
    // request instead.
    if let Some(response) = cors_preflight(method, &inbound_headers) {
        return Ok(response);
    }

    let chain = state.tenant_chain(sec).await;
    let upstream = resolve_upstream(state, &chain, alias)?;
    let route = match_route(state, &chain, &upstream, method, proxy_path)?;
    let endpoint = select_endpoint(state, &upstream, alias, &inbound_headers)?;
    check_ssrf(state, &endpoint).await?;

    let cors = effective_cors(&upstream, &route);
    check_cors_actual(&cors, method, &inbound_headers)?;
    let announced = hyper::body::Body::size_hint(&body).exact();
    check_body_size(state, &inbound_headers, announced)?;

    let chain = resolve_plugins(state, sec, &upstream, &route)?;
    apply_guard_phase(&chain, &inbound_headers, Phase::Request)?;

    let rate = enforce_rate_limit(state, sec, &upstream, &route, &inbound_headers);
    if let Some(rejected) = rate.rejected {
        return Ok(rejected);
    }

    let mut headers =
        build_request_headers(&inbound_headers, &upstream.headers, &endpoint, upgrade_requested);
    let forwarded_query = filter_query(&route, request.query.as_deref());
    let auth = plugins::auth_config(&upstream, &chain);
    inject_credentials(state, sec, auth, &mut headers, &upstream.alias).await?;
    apply_transform_phase(&chain, &mut headers, Phase::Request)?;

    let io = crate::upstream_client::dial(
        state.connector(),
        endpoint.scheme,
        &endpoint.host,
        endpoint.port,
        state.connect_timeout(),
    )
    .await
    .map_err(|error| error.with_upstream_id(upstream.id.to_string()))?;
    let outgoing = build_outgoing(request, &endpoint, forwarded_query, headers, body, state, upgrade_requested)?;
    let limit_flag = outgoing.1;
    let response = crate::upstream_client::dispatch(io, outgoing.0, state.request_timeout())
        .await
        .map_err(|error| {
            // A body that grows past the ceiling aborts the exchange; report
            // the limit rather than the transport symptom it triggered.
            if limit_flag.load(Ordering::Relaxed) {
                too_large(state, state.config.max_body_bytes + 1)
            } else {
                error.with_upstream_id(upstream.id.to_string())
            }
        })?;

    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        return match client_upgrade {
            Some(hook) => Ok(bridge_upgrade(response, hook)),
            None => Err(OagwError::new(
                ErrorKind::UpgradeUnsupported,
                "the upstream offered an upgrade this exchange cannot carry",
            )),
        };
    }
    if limit_flag.load(Ordering::Relaxed) {
        return Err(too_large(state, state.config.max_body_bytes + 1));
    }
    Ok(render_response(
        &upstream,
        &route,
        &chain,
        response,
        &rate,
        upgrade_requested,
        request.trace_id,
        request.origin.as_deref(),
    ))
}

/// Resolves an alias against the tenant chain, oldest first.
fn resolve_upstream(
    state: &Arc<OagwState>,
    chain: &[uuid::Uuid],
    alias: &str,
) -> Result<Arc<Upstream>, OagwError> {
    for tenant in chain {
        if let Some(upstream) = state.store.get_upstream_by_alias(*tenant, alias) {
            if !upstream.enabled {
                // A disabled upstream refuses every proxy request, but it is
                // still the resolved target, so the problem carries its id.
                return Err(OagwError::new(
                    ErrorKind::LinkUnavailable,
                    format!("upstream '{alias}' is disabled"),
                )
                .with_host(alias)
                .with_upstream_id(upstream.id.to_string()));
            }
            return Ok(upstream);
        }
    }
    Err(OagwError::new(
        ErrorKind::RouteNotFound,
        format!("no upstream is registered for alias '{alias}'"),
    )
    .with_host(alias))
}

/// Selects the most specific enabled HTTP route for a proxy path.
fn match_route(
    state: &Arc<OagwState>,
    chain: &[uuid::Uuid],
    upstream: &Upstream,
    method: &Method,
    proxy_path: &str,
) -> Result<Arc<Route>, OagwError> {
    let mut candidates: Vec<(usize, Arc<Route>)> = Vec::new();
    for tenant in chain {
        for route in state.store.routes_of(*tenant, upstream.id) {
            let Some(http) = &route.r#match.http else {
                continue;
            };
            if !route.enabled || !http.methods.iter().any(|m| m.as_str() == method.as_str()) {
                continue;
            }
            if let Some(depth) = prefix_depth(&http.path, proxy_path, http.path_suffix_mode) {
                candidates.push((depth, route));
            }
        }
    }
    candidates.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    candidates
        .into_iter()
        .next()
        .map(|(_, route)| route)
        .ok_or_else(|| {
            OagwError::new(
                ErrorKind::RouteNotFound,
                format!(
                    "no route matches {method} {proxy_path} for upstream '{}'",
                    upstream.alias
                ),
            )
        })
}

/// The specificity of a path prefix match, or `None` when it does not match.
#[must_use]
fn prefix_depth(pattern: &str, path: &str, mode: PathSuffixMode) -> Option<usize> {
    if matches!(mode, PathSuffixMode::Disabled) && path != pattern {
        return None;
    }
    if pattern == "/" {
        return Some(0);
    }
    if path == pattern {
        return Some(pattern.len());
    }
    if path.starts_with(&format!("{pattern}/")) {
        Some(pattern.len())
    } else {
        None
    }
}

/// Keeps only the query parameters the route allows.
#[must_use]
fn filter_query(route: &Route, query: Option<&str>) -> Option<String> {
    let allowlist = &route.r#match.http.as_ref()?.query_allowlist;
    let query = query?;
    if allowlist.is_empty() {
        return None;
    }
    let kept: Vec<String> = form_urlencoded::parse(query.as_bytes())
        .filter(|(name, _)| allowlist.iter().any(|allowed| allowed == name))
        .map(|(name, value)| {
            form_urlencoded::Serializer::new(String::new())
                .append_pair(&name, &value)
                .finish()
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("&"))
    }
}

/// Picks the endpoint to dial, honouring `X-OAGW-Target-Host`.
fn select_endpoint(
    state: &Arc<OagwState>,
    upstream: &Upstream,
    alias: &str,
    headers: &HeaderMap,
) -> Result<ProxyTarget, OagwError> {
    let endpoints = &upstream.server.endpoints;
    let first = endpoints
        .first()
        .ok_or_else(|| OagwError::new(ErrorKind::LinkUnavailable, "upstream has no endpoints"))?;
    if let Some(requested) = header_str(headers, target_host_name()) {
        let wanted = crate::alias::validate_host(&requested).map_err(|_| {
            OagwError::new(
                ErrorKind::InvalidTargetHost,
                format!("'{requested}' is not a valid host name"),
            )
        })?;
        return endpoints
            .iter()
            .find(|endpoint| endpoint.host == wanted)
            .map(target_of)
            .ok_or_else(|| {
                OagwError::new(
                    ErrorKind::UnknownTargetHost,
                    format!("'{requested}' does not name an endpoint of this upstream"),
                )
                .with_host(&requested)
            });
    }
    if endpoints.len() > 1 && crate::alias::derive_alias(endpoints).as_deref() == Some(alias) {
        return Err(OagwError::new(
            ErrorKind::MissingTargetHost,
            format!(
                "upstream '{}' exposes {} endpoints; set {}",
                upstream.alias,
                endpoints.len(),
                TARGET_HOST_HEADER
            ),
        ));
    }
    let index = state.store.next_endpoint_index(upstream.id, endpoints.len());
    Ok(target_of(endpoints.get(index).unwrap_or(first)))
}

fn target_of(endpoint: &Endpoint) -> ProxyTarget {
    ProxyTarget {
        scheme: endpoint.scheme,
        host: endpoint.host.clone(),
        port: endpoint.port(),
    }
}

/// The route CORS configuration, falling back to the upstream's.
fn effective_cors(upstream: &Upstream, route: &Route) -> Option<CorsConfig> {
    route.cors.clone().or_else(|| upstream.cors.clone())
}

/// Answers a CORS preflight locally, short-circuiting the whole pipeline.
///
/// ADR 0004: a preflight carries no credentials and therefore no tenant
/// context, so it is detected at handler level and answered permissively —
/// the requested origin, method and headers are echoed back without
/// resolving the upstream. Origin and method enforcement is deferred to the
/// actual request, see `check_cors_actual`.
fn cors_preflight(method: &Method, headers: &HeaderMap) -> Option<Response> {
    if method != Method::OPTIONS {
        return None;
    }
    let origin = header_str(headers, http::header::ORIGIN)?;
    let requested_method = header_str(headers, http::header::ACCESS_CONTROL_REQUEST_METHOD)?;
    let requested_headers = header_str(headers, http::header::ACCESS_CONTROL_REQUEST_HEADERS);

    let mut response = (StatusCode::NO_CONTENT, "").into_response();
    let response_headers = response.headers_mut();
    set_header(
        response_headers,
        http::header::ACCESS_CONTROL_ALLOW_ORIGIN.as_str(),
        &origin,
    );
    set_header(
        response_headers,
        http::header::ACCESS_CONTROL_ALLOW_METHODS.as_str(),
        &requested_method,
    );
    if let Some(requested) = requested_headers {
        set_header(
            response_headers,
            http::header::ACCESS_CONTROL_ALLOW_HEADERS.as_str(),
            &requested,
        );
    }
    set_header(
        response_headers,
        http::header::ACCESS_CONTROL_MAX_AGE.as_str(),
        "86400",
    );
    set_header(
        response_headers,
        http::header::VARY.as_str(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
    );
    Some(response)
}

/// Refuses endpoints the SSRF policy forbids, before the connection is dialled.
///
/// `cpt-cf-oagw-nfrssrf-protection`: OAGW dials hostnames it was configured
/// with, which makes it an SSRF vector whenever an operator can name an
/// internal address. With `ssrf_policy.enabled` the resolved endpoint's host
/// is resolved and every resulting address must be public.
///
/// # Errors
/// Returns a 403 `upstream.endpoint_forbidden.v1` problem when the policy is
/// enabled and the host resolves to a non-public address.
async fn check_ssrf(state: &OagwState, endpoint: &ProxyTarget) -> Result<(), OagwError> {
    if !state.config.ssrf_policy.enabled {
        return Ok(());
    }
    let socket = format!("{}:{}", endpoint.host, endpoint.port);
    let resolved = tokio::net::lookup_host(socket.as_str())
        .await
        .map_err(|error| {
            OagwError::new(
                ErrorKind::Validation,
                format!("endpoint host '{}' could not be resolved: {error}", endpoint.host),
            )
        })?;
    if let Some(address) = resolved.map(|candidate| candidate.ip()).find(|ip| is_internal(*ip)) {
        return Err(OagwError::new(
            ErrorKind::EndpointForbidden,
            format!(
                "endpoint host '{}' resolves to the non-public address {address}, \
                 which the SSRF policy refuses",
                endpoint.host
            ),
        )
        .with_host(endpoint.host.clone()));
    }
    Ok(())
}

/// Whether an address belongs to a network segment an SSRF policy must refuse.
///
/// Loopback, private (RFC 1918 / ULA), link-local, CGNAT, benchmarking,
/// multicast, reserved and unspecified space are all refused, as is any
/// address embedded in an IPv6 mapping. The IETF documentation prefixes
/// (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`) are
/// treated as public: they are unreachable by construction and name no
/// internal infrastructure.
#[must_use]
fn is_internal(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_internal_v4(v4),
        IpAddr::V6(v6) => is_internal_v6(v6),
    }
}

fn is_internal_v4(v4: Ipv4Addr) -> bool {
    if v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() {
        return true;
    }
    let value = u32::from(v4);
    // Shared address space (100.64.0.0/10), benchmarking (198.18.0.0/15),
    // IETF protocol assignments (192.0.0.0/24), this network (0.0.0.0/8),
    // multicast (224.0.0.0/4) and reserved (240.0.0.0/4).
    (0x6440_0000..=0x647f_ffff).contains(&value)
        || (0xc612_0000..=0xc613_ffff).contains(&value)
        || (0xc000_0000..=0xc000_00ff).contains(&value)
        || (0xe000_0000..=0xefff_ffff).contains(&value)
        || (0xf000_0000..=0xffff_ffff).contains(&value)
        || value < 0x0100_0000
}

fn is_internal_v6(v6: std::net::Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
        return is_internal_v4(v4);
    }
    if v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local() {
        return true;
    }
    // Unicast link-local (`fe80::/10`): no stable std predicate, so the
    // leading ten bits are checked directly.
    let segments = v6.segments();
    segments[0] & 0xffc0 == 0xfe80
}

/// Validates the origin and method of an actual CORS request.
fn check_cors_actual(
    cors: &Option<CorsConfig>,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), OagwError> {
    let Some(config) = cors.as_ref().filter(|config| config.enabled) else {
        return Ok(());
    };
    let Some(origin) = header_str(headers, http::header::ORIGIN) else {
        return Ok(());
    };
    if !origin_allowed(config, &origin) {
        return Err(OagwError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("origin '{origin}' is not allowed"),
        ));
    }
    let allowed = config
        .allowed_methods
        .clone()
        .unwrap_or_else(|| vec!["GET".to_owned(), "POST".to_owned()]);
    if !allowed
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(OagwError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("method {method} is not allowed for origin '{origin}'"),
        ));
    }
    Ok(())
}

/// Whether a request may carry a tunnelled protocol upgrade.
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap) -> bool {
    let connection = header_str(headers, CONNECTION)
        .unwrap_or_default()
        .to_ascii_lowercase();
    connection.contains("upgrade") && headers.contains_key(UPGRADE)
}

fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .as_ref()
        .is_some_and(|origins| origins.iter().any(|candidate| candidate == "*" || candidate == origin))
}

/// The parsed name of the target-host selector header.
fn target_host_name() -> HeaderName {
    HeaderName::from_lowercase(TARGET_HOST_HEADER.as_bytes()).expect("static header name")
}

/// The parsed name of the forwarded-for header.
fn forwarded_for_name() -> HeaderName {
    HeaderName::from_lowercase(FORWARDED_FOR_HEADER.as_bytes()).expect("static header name")
}

fn header_str(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Rejects requests whose declared or announced length exceeds the ceiling.
fn check_body_size(state: &OagwState, headers: &HeaderMap, announced: Option<u64>) -> Result<(), OagwError> {
    let declared = header_str(headers, http::header::CONTENT_LENGTH)
        .and_then(|value| value.parse::<u64>().ok())
        .or(announced);
    if let Some(length) = declared.filter(|length| *length > state.config.max_body_bytes) {
        return Err(too_large(state, length));
    }
    Ok(())
}

/// The 413 problem for a body of `length` bytes.
fn too_large(state: &OagwState, length: u64) -> OagwError {
    OagwError::new(
        ErrorKind::PayloadTooLarge,
        format!(
            "request body of {length} bytes exceeds the limit of {} bytes",
            state.config.max_body_bytes
        ),
    )
}

/// Resolves the plugin chain of an upstream and its route.
fn resolve_plugins(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    upstream: &Upstream,
    route: &Route,
) -> Result<Vec<ResolvedPlugin>, OagwError> {
    let mut items: Vec<PluginItem> = plugins::references_of_upstream(upstream);
    items.extend(plugins::references_of_route(route));
    let store = Arc::clone(state);
    let tenant = sec.subject_tenant_id();
    let lookup = move |reference: &str| -> Option<crate::model::PluginRecord> {
        let id = plugins::custom_reference(reference)?;
        store.store.get_plugin(tenant, id).map(|record| (*record).clone())
    };
    plugins::resolve_chain(&items, &lookup)
}

/// The rate-limit outcome for a request.
struct RateOutcome {
    rejected: Option<Response>,
    decision: Option<RateDecision>,
}

fn enforce_rate_limit(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    upstream: &Upstream,
    route: &Route,
    headers: &HeaderMap,
) -> RateOutcome {
    let Some(config) = merged_rate_limit(upstream, route) else {
        return RateOutcome {
            rejected: None,
            decision: None,
        };
    };
    let client_ip = header_str(headers, forwarded_for_name()).unwrap_or_default();
    let key = crate::rate_limit::scope_key(
        config.scope,
        &upstream.alias,
        &route.id.to_string(),
        &sec.subject_tenant_id().to_string(),
        &sec.subject_id().to_string(),
        &client_ip,
    );
    let decision = state.limiter.check(&key, &config);
    if decision.allowed {
        RateOutcome {
            rejected: None,
            decision: Some(decision),
        }
    } else {
        RateOutcome {
            rejected: Some(reject_rate_limited(&decision)),
            decision: Some(decision),
        }
    }
}

/// Merges the upstream and route rate limits, taking the tightest bound.
#[must_use]
pub fn merged_rate_limit(upstream: &Upstream, route: &Route) -> Option<RateLimitConfig> {
    match (upstream.rate_limit.clone(), route.rate_limit.clone()) {
        (Some(upstream), Some(route)) => Some(merge_limits(upstream, route)),
        (one, other) => one.or(other),
    }
}

fn merge_limits(mut upstream: RateLimitConfig, route: RateLimitConfig) -> RateLimitConfig {
    if route.sustained.rate < upstream.sustained.rate {
        upstream.sustained.rate = route.sustained.rate;
    }
    if let Some(route_burst) = route.burst {
        let capacity = upstream
            .burst
            .map(|existing| existing.capacity.min(route_burst.capacity))
            .unwrap_or(route_burst.capacity);
        upstream.burst = Some(crate::model::BurstConfig { capacity });
    }
    if route.cost > upstream.cost {
        upstream.cost = route.cost;
    }
    upstream
}

fn reject_rate_limited(decision: &RateDecision) -> Response {
    let error = OagwError::new(
        ErrorKind::RateLimitExceeded,
        "rate limit exceeded for this upstream",
    )
    .with_retry_after(decision.retry_after_secs.max(1));
    let mut response = error.into_response();
    let headers = response.headers_mut();
    for (name, value) in rate_limit_headers(decision) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            let _ = headers.insert(name, value);
        }
    }
    response
}

fn rate_limit_headers(decision: &RateDecision) -> [(&'static str, String); 3] {
    [
        ("x-ratelimit-limit", decision.limit.to_string()),
        ("x-ratelimit-remaining", decision.remaining.to_string()),
        ("x-ratelimit-reset", decision.reset_secs.to_string()),
    ]
}

/// Builds the outbound header set from the inbound one plus the rules.
fn build_request_headers(
    inbound: &HeaderMap,
    config: &Option<crate::model::HeadersConfig>,
    target: &ProxyTarget,
    upgrade: bool,
) -> HeaderMap {
    let rules = config
        .as_ref()
        .and_then(|headers| headers.request.clone())
        .unwrap_or_default();
    let removed = rules.remove.clone().unwrap_or_default();
    let mode = rules.passthrough.unwrap_or_default();

    let mut out = HeaderMap::new();
    set_header(&mut out, "host", &format!("{}:{}", target.host, target.port));
    // A protocol upgrade is an opaque exchange: the client's handshake
    // headers reach the upstream even when passthrough is disabled.
    if upgrade || matches!(mode, PassthroughMode::All) {
        for (name, value) in inbound.iter() {
            if is_forwardable(name, &removed, upgrade) {
                out.append(name, value.clone());
            }
        }
    }
    for name in rules.passthrough_allowlist.clone().unwrap_or_default() {
        let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) else { continue };
        if let Some(value) = header_str(inbound, parsed) {
            set_header(&mut out, &name, &value);
        }
    }
    for (name, value) in &rules.add.clone().unwrap_or_default() {
        append_header(&mut out, name, value);
    }
    for (name, value) in &rules.set.clone().unwrap_or_default() {
        set_header(&mut out, name, value);
    }
    out
}

fn is_forwardable(name: &HeaderName, removed: &[String], upgrade: bool) -> bool {
    let lower = name.as_str();
    if lower.starts_with("x-oagw-") {
        return false;
    }
    if !upgrade && (lower == CONNECTION.as_str() || lower == UPGRADE.as_str()) {
        return false;
    }
    if HOP_BY_HOP.contains(&lower) {
        return false;
    }
    !removed.iter().any(|candidate| candidate.eq_ignore_ascii_case(lower))
}

fn append_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.append(name, value);
    }
}

fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

/// Injects upstream credentials according to the resolved auth plugin.
async fn inject_credentials(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    auth: Option<(String, serde_json::Value)>,
    headers: &mut HeaderMap,
    upstream_alias: &str,
) -> Result<(), OagwError> {
    let Some((behaviour, config)) = auth else {
        return Ok(());
    };
    match behaviour.as_str() {
        plugins::AUTH_NOOP => Ok(()),
        plugins::AUTH_APIKEY => {
            let key = api_key(state, sec, &config).await?;
            let name = plugins::config_str(&config, "header")
                .unwrap_or_else(|| "authorization".to_owned());
            let prefix = plugins::config_str(&config, "prefix").unwrap_or_default();
            set_header(headers, &name, &format!("{prefix}{key}"));
            Ok(())
        }
        plugins::AUTH_OAUTH2 => {
            inject_oauth2(state, sec, &config, false, headers, upstream_alias).await
        }
        plugins::AUTH_OAUTH2_BASIC => {
            inject_oauth2(state, sec, &config, true, headers, upstream_alias).await
        }
        other => Err(OagwError::new(
            ErrorKind::AuthFailed,
            format!("auth plugin '{other}' is not implemented"),
        )),
    }
}

/// Resolves an API key from the plugin configuration or the credential store.
async fn api_key(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    config: &serde_json::Value,
) -> Result<String, OagwError> {
    if let Some(literal) = plugins::config_str(config, "key") {
        return Ok(literal);
    }
    let reference = plugins::config_str(config, "key_ref").ok_or_else(|| {
        OagwError::new(
            ErrorKind::AuthFailed,
            "apikey plugin requires config.key or config.key_ref",
        )
    })?;
    crate::oauth::resolve_secret(state.credstore.as_ref(), sec, &reference).await
}

/// Acquires (or reuses) an OAuth2 access token and injects it.
async fn inject_oauth2(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    config: &serde_json::Value,
    basic: bool,
    headers: &mut HeaderMap,
    upstream_alias: &str,
) -> Result<(), OagwError> {
    let key = crate::oauth::cache_key(
        sec.subject_tenant_id(),
        sec.subject_id(),
        crate::oauth::method_tag(basic),
        config,
    );
    if let Some(cached) = state.token_cache.get(&key) {
        set_header(headers, "authorization", &format!("Bearer {}", cached.access_token));
        return Ok(());
    }
    let token = crate::oauth::fetch_token(
        state.credstore.as_ref(),
        state.connector(),
        sec,
        config,
        basic,
        state.request_timeout(),
    )
        .await
        .map_err(|error| error.with_host(upstream_alias))?;
    let remaining = token
        .expires_at
        .saturating_sub(crate::model::now_millis())
        .clamp(0, 3_600_000);
    let ttl_secs = u64::try_from(remaining / 1000).unwrap_or(1).max(1);
    state.token_cache.put(&key, token.clone(), ttl_secs);
    set_header(headers, "authorization", &format!("Bearer {}", token.access_token));
    Ok(())
}

/// Runs the transform plugins of one phase.
fn apply_transform_phase(
    chain: &[ResolvedPlugin],
    headers: &mut HeaderMap,
    phase: Phase,
) -> Result<(), OagwError> {
    for plugin in chain.iter().filter(|plugin| plugin.plugin_type == PluginType::Transform) {
        let key = match phase {
            Phase::Request => "request",
            Phase::Response => "response",
        };
        let scoped = plugin
            .config
            .get(key)
            .cloned()
            .unwrap_or_else(|| plugin.config.clone());
        for name in plugins::config_list(&scoped, "remove") {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                headers.remove(name);
            }
        }
        for (name, value) in plugins::config_map(&scoped, "set") {
            set_header(headers, &name, &value);
        }
        for (name, value) in plugins::config_map(&scoped, "add") {
            append_header(headers, &name, &value);
        }
    }
    Ok(())
}

/// Enforces the required-headers guard of `ADR/0009-required-headers-guard-plugin.md`.
fn apply_guard_phase(chain: &[ResolvedPlugin], headers: &HeaderMap, phase: Phase) -> Result<(), OagwError> {
    for plugin in chain.iter().filter(|plugin| plugin.plugin_type == PluginType::Guard) {
        let key = match phase {
            Phase::Request => "required_request_headers",
            Phase::Response => "required_response_headers",
        };
        for name in plugins::config_names(&plugin.config, key) {
            if !headers.contains_key(name.as_str()) {
                return Err(guard_error(phase, &name));
            }
        }
    }
    Ok(())
}

fn guard_error(phase: Phase, name: &str) -> OagwError {
    match phase {
        Phase::Request => OagwError::new(
            ErrorKind::Validation,
            format!("required header '{name}' is missing"),
        ),
        Phase::Response => OagwError::new(
            ErrorKind::DownstreamError,
            format!("upstream response is missing required header '{name}'"),
        ),
    }
}

/// Assembles the outgoing request and reports its limit flag.
fn build_outgoing(
    request: &ProxyRequest<'_>,
    endpoint: &ProxyTarget,
    query: Option<String>,
    mut headers: HeaderMap,
    body: axum::body::Body,
    state: &OagwState,
    upgrade: bool,
) -> Result<(http::Request<ProxyBody>, Arc<AtomicBool>), OagwError> {
    let (method, proxy_path) = (request.method, request.proxy_path);
    let authority = format!("{}:{}", endpoint.host, endpoint.port);
    set_header(&mut headers, "x-forwarded-proto", scheme_of(endpoint.scheme));
    set_header(&mut headers, "x-forwarded-host", &authority);
    // A request addressed to the bare alias targets the upstream root.
    let path = if proxy_path.is_empty() { "/" } else { proxy_path };
    let path_and_query = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    let uri = Uri::builder()
        .path_and_query(path_and_query)
        .build()
        .map_err(|error| {
            OagwError::new(ErrorKind::Validation, format!("proxy path is invalid: {error}"))
        })?;
    let max_bytes = if upgrade {
        u64::MAX
    } else {
        state.config.max_body_bytes
    };
    let limited = Limited::new(body, max_bytes);
    let flag = limited.limit_flag();
    let mut builder = http::Request::builder().method(method.clone()).uri(uri);
    if let Some(out) = builder.headers_mut() {
        *out = headers;
    }
    let request = builder.body(limited)
        .map_err(|error| {
            OagwError::new(ErrorKind::ProtocolError, format!("request could not be built: {error}"))
        })?;
    Ok((request, flag))
}

fn scheme_of(scheme: EndpointScheme) -> &'static str {
    if matches!(scheme, EndpointScheme::Http) {
        "http"
    } else {
        "https"
    }
}

/// Relays a `101 Switching Protocols` exchange between client and upstream.
fn bridge_upgrade(
    mut upstream: http::Response<hyper::body::Incoming>,
    client_upgrade: hyper::upgrade::OnUpgrade,
) -> Response {
    let headers = {
        let mut relayed = HeaderMap::new();
        for name in [CONNECTION, UPGRADE] {
            if let Some(value) = upstream.headers().get(&name).cloned() {
                relayed.insert(name, value);
            }
        }
        if let Some(accept) = upstream.headers().get("sec-websocket-accept").cloned() {
            relayed.insert(HeaderName::from_static("sec-websocket-accept"), accept);
        }
        relayed
    };
    let response = axum::response::Response::new(axum::body::Body::empty());
    let (mut parts, body) = response.into_parts();
    parts.status = StatusCode::SWITCHING_PROTOCOLS;
    parts.headers.extend(headers);
    let response = axum::response::Response::from_parts(parts, body);

    let upstream_upgrade = hyper::upgrade::on(&mut upstream);
    tokio::spawn(async move {
        let (client, upstream) = futures_util::join!(client_upgrade, upstream_upgrade);
        if let (Ok(client), Ok(upstream)) = (client, upstream) {
            let mut client = hyper_util::rt::TokioIo::new(client);
            let mut upstream = hyper_util::rt::TokioIo::new(upstream);
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
    });
    response
}

/// Converts an upstream response into a client response.
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn render_response(
    upstream: &Upstream,
    route: &Route,
    chain: &[ResolvedPlugin],
    response: http::Response<hyper::body::Incoming>,
    rate: &RateOutcome,
    upgrade: bool,
    trace_id: &str,
    origin: Option<&str>,
) -> Response {
    let status = response.status();
    let (parts, body) = response.into_parts();
    let removed = response_remove_rules(upstream);
    let mut headers = HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        if is_forwardable(name, &removed, upgrade) {
            headers.append(name, value.clone());
        }
    }
    apply_response_rules(upstream, &mut headers);
    if let Err(error) = apply_guard_phase(chain, &headers, Phase::Response) {
        return error.into_response();
    }
    let _ = apply_transform_phase(chain, &mut headers, Phase::Response);
    if status.is_client_error() || status.is_server_error() {
        let _ = headers.insert(
            HeaderName::from_static("x-oagw-error-source"),
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
    }
    if let Some(decision) = &rate.decision {
        for (name, value) in rate_limit_headers(decision) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                let _ = headers.insert(name, value);
            }
        }
    }
    // ADR 0004 "Actual Request Handling" step 5: the CORS response headers
    // describe the request that was accepted, so they are derived from the
    // inbound `Origin`, never from whatever the upstream chose to send back.
    let config = effective_cors(upstream, route).filter(|config| config.enabled);
    if let (Some(config), Some(origin)) = (config.as_ref(), origin)
        && origin_allowed(config, origin)
    {
        for (name, value) in cors_headers(config, origin) {
            let _ = headers.insert(name, value);
        }
        set_header(&mut headers, http::header::VARY.as_str(), "Origin");
    }
    if let Ok(value) = HeaderValue::from_str(trace_id) {
        let _ = headers.insert(HeaderName::from_static(TRACE_ID_HEADER), value);
    }
    let response = axum::response::Response::new(axum::body::Body::new(body));
    let (mut parts, body) = response.into_parts();
    parts.status = status;
    parts.headers = headers;
    axum::response::Response::from_parts(parts, body)
}

fn response_remove_rules(upstream: &Upstream) -> Vec<String> {
    upstream
        .headers
        .as_ref()
        .and_then(|headers| headers.response.clone())
        .and_then(|rules| rules.remove)
        .unwrap_or_default()
}

fn apply_response_rules(upstream: &Upstream, headers: &mut HeaderMap) {
    let Some(rules) = upstream
        .headers
        .as_ref()
        .and_then(|headers| headers.response.clone())
    else {
        return;
    };
    for (name, value) in rules.set.unwrap_or_default() {
        set_header(headers, &name, &value);
    }
    for (name, value) in rules.add.unwrap_or_default() {
        append_header(headers, &name, &value);
    }
}

/// The CORS response headers for an actual request.
fn cors_headers(config: &CorsConfig, origin: &str) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::new();
    if let Ok(value) = HeaderValue::from_str(origin) {
        out.push((http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value));
    }
    if config.allow_credentials.unwrap_or(false) {
        out.push((
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        ));
    }
    if let Some(exposed) = &config.expose_headers
        && let Ok(value) = HeaderValue::from_str(&exposed.join(", ")) {
            out.push((http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value));
        }
    out
}
