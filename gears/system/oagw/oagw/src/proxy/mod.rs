//! The OAGW data plane.
//!
//! Resolution order for a proxied request: tenant chain → upstream by alias →
//! target endpoint → CORS → route match → guards → auth → transforms →
//! upstream call → response transforms. Gateway failures are problem details
//! carrying `X-OAGW-Error-Source: gateway`; anything that came back from the
//! upstream is passed through with `X-OAGW-Error-Source: upstream`.

pub mod circuit;
pub mod cors;
pub mod headers;
pub mod ratelimit;
pub mod transport;
pub mod ws;

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::FromRequestParts;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::{Method, Uri};
use axum::response::Response;
use futures_util::StreamExt;
use http::header;
use http_body_util::BodyExt;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::SecurityContext;
use transport::ProxiedResponse;

use crate::config::OagwConfig;
use crate::domain::alias::compute_derived_alias;
use crate::domain::error::OagwError;
use crate::domain::model::{Cors, Endpoint, PathSuffixMode, Route, SharingMode, Upstream};
use crate::domain::store::{Store, tenant_chain};
use crate::plugins::{
    AuthPluginRegistry, GuardDecision, GuardPluginRegistry, PluginError, RequestContext,
    ResponseContext, TransformPluginRegistry,
};

/// Everything the data plane needs at request time.
pub struct ProxyState {
    /// Configuration store.
    pub store: std::sync::Arc<Store>,
    /// Gear configuration.
    pub config: OagwConfig,
    /// Auth plugin registry.
    pub auth_registry: AuthPluginRegistry,
    /// Guard plugin registry.
    pub guard_registry: GuardPluginRegistry,
    /// Transform plugin registry.
    pub transform_registry: TransformPluginRegistry,
    /// Tenant resolver (absent when the deployment has none).
    pub tenant_resolver: Option<std::sync::Arc<dyn TenantResolverClient>>,
    /// TLS client context.
    pub tls: transport::TlsContext,
    /// Rate limiter.
    pub rate_limiter: std::sync::Arc<ratelimit::RateLimiter>,
    /// Circuit breaker.
    pub circuit: std::sync::Arc<circuit::CircuitBreaker>,
    /// Round-robin cursors keyed by alias.
    pub round_robin: dashmap::DashMap<String, usize>,
}

impl ProxyState {
    /// Builds the data-plane state from a store and a gear configuration.
    #[must_use]
    pub fn new(
        store: std::sync::Arc<Store>,
        config: OagwConfig,
        credstore: std::sync::Arc<dyn credstore_sdk::CredStoreClientV1>,
        tenant_resolver: Option<std::sync::Arc<dyn TenantResolverClient>>,
    ) -> Self {
        Self {
            auth_registry: AuthPluginRegistry::with_builtins(
                credstore,
                config.token_cache_ttl(),
                config.token_cache_capacity,
            ),
            guard_registry: GuardPluginRegistry::with_builtins(),
            transform_registry: TransformPluginRegistry::with_builtins(),
            store,
            config,
            tenant_resolver,
            tls: transport::TlsContext::new(),
            rate_limiter: ratelimit::shared(),
            circuit: circuit::shared(),
            round_robin: dashmap::DashMap::new(),
        }
    }
}

/// Renders an [`OagwError`] as a gateway problem response.
#[must_use]
pub fn problem(err: &OagwError) -> Response {
    err.to_response()
}

/// Entry point for the proxy handlers.
pub async fn proxy(
    state: std::sync::Arc<ProxyState>,
    security: SecurityContext,
    request: axum::extract::Request,
    alias: String,
    path_suffix: String,
) -> Response {
    let (parts, body) = request.into_parts();

    // A preflight is answered permissively at the handler level (ADR-0004),
    // before any resolution and without touching the upstream.
    if cors::is_preflight(&parts.method, &parts.headers) {
        return cors::preflight_response(&parts.headers);
    }

    let upgrade = if is_websocket_upgrade(&parts.headers) {
        let mut parts = parts.clone();
        WebSocketUpgrade::from_request_parts(&mut parts, &())
            .await
            .ok()
    } else {
        None
    };

    let result = match upgrade {
        Some(upgrade) => {
            ws::upgrade(&state, &security, &parts, upgrade, &alias, &path_suffix).await
        }
        None => forward(&state, &security, &parts, body, &alias, &path_suffix).await,
    };
    result.unwrap_or_else(|err| problem(&err))
}

/// Whether the request asks for a WebSocket upgrade.
fn is_websocket_upgrade(headers: &http::HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// A target resolved far enough to build an outbound request.
pub(crate) struct Resolved {
    /// The tenant chain, self first.
    pub chain: Vec<uuid::Uuid>,
    /// The upstream the request is proxied to.
    pub upstream: Upstream,
    /// Its UUID, used in problem extensions and breaker scoping.
    pub upstream_uuid: uuid::Uuid,
    /// The endpoint selected for this request.
    pub endpoint: Endpoint,
    /// The route matched for this request.
    pub route: Route,
    /// The effective CORS configuration.
    pub cors: Cors,
}

/// Resolves alias, endpoint and route for a request.
///
/// # Errors
///
/// 404 for an unknown alias or route, 503 for a disabled upstream, 400 for
/// target-host and CORS problems.
pub(crate) async fn resolve(
    state: &std::sync::Arc<ProxyState>,
    security: &SecurityContext,
    parts: &axum::http::request::Parts,
    alias: &str,
    request_path: &str,
) -> Result<Resolved, OagwError> {
    let chain = tenant_chain(state.tenant_resolver.as_ref(), security).await?;

    // Aliases are stored normalized (`DESIGN.md` §Alias Enforcement), so the
    // request's spelling — `API.Vendor.COM.`, say — has to be brought to the
    // same form before the lookup.
    let alias = &normalize_alias(alias);
    let upstream = state
        .store
        .find_upstream_by_alias(&chain, alias)
        .ok_or_else(|| {
            OagwError::route_not_found(format!("no upstream is registered for alias {alias:?}"))
                .with_extension("alias", alias.as_str())
        })?;
    if !upstream.enabled {
        return Err(
            OagwError::link_unavailable(format!("upstream {alias:?} is disabled"))
                .with_extension("alias", alias.as_str()),
        );
    }

    let endpoint = select_endpoint(state, &upstream, headers::target_host(&parts.headers))?;
    let upstream_uuid =
        crate::domain::store::parse_uuid(upstream.id.as_deref().unwrap_or_default())?;

    let route = match_route(state, &chain, &upstream_uuid, &parts.method, request_path)?;

    let cors = effective_cors(&upstream, &route);
    cors::validate_request(&cors, &parts.method, &parts.headers)?;

    Ok(Resolved {
        chain,
        upstream,
        upstream_uuid,
        endpoint,
        route,
        cors,
    })
}

/// The CORS policy a request is held to: the route's when it enables one, the
/// upstream's otherwise. `DESIGN.md` §Guard Rules describes origin checking
/// against the upstream; the route field narrows it for that route alone.
fn effective_cors(upstream: &Upstream, route: &Route) -> Cors {
    match &route.cors {
        Some(cors) if cors.enabled => cors.clone(),
        _ => upstream.cors.clone().unwrap_or_default(),
    }
}

/// Executes the proxy pipeline for a plain HTTP request.
#[allow(clippy::too_many_lines)]
async fn forward(
    state: &std::sync::Arc<ProxyState>,
    security: &SecurityContext,
    parts: &axum::http::request::Parts,
    body: Body,
    alias: &str,
    path_suffix: &str,
) -> Result<Response, OagwError> {
    let cfg = &state.config;
    let request_path = normalize_suffix(path_suffix);

    let target = resolve(state, security, parts, alias, &request_path).await?;
    let route = target.route.clone();

    // ---- body validation -------------------------------------------------
    let declared_length = declared_content_length(parts)?;
    if let Some(te) = parts.headers.get(header::TRANSFER_ENCODING)
        && !te
            .to_str()
            .unwrap_or_default()
            .split(',')
            .any(|v| v.trim().eq_ignore_ascii_case("chunked"))
    {
        return Err(OagwError::validation(
            "only chunked Transfer-Encoding is supported",
        ));
    }
    let body_bytes = read_body(body, cfg.max_request_body_bytes).await?;
    if let Some(declared) = declared_length
        && declared != body_bytes.len()
    {
        return Err(OagwError::validation(format!(
            "Content-Length {declared} does not match the request body length {}",
            body_bytes.len()
        )));
    }

    // ---- rate limiting ---------------------------------------------------
    let rate_headers = enforce_rate_limit(state, security, &target, &request_path).await?;

    // ---- outbound request ------------------------------------------------
    let outbound_path = build_outbound_path(&route, &request_path)?;
    let query = filter_query(&route, parts.uri.query().unwrap_or_default())?;
    let uri = build_uri(&outbound_path, &query)?;

    let outbound_headers = headers::build_outbound_headers(
        &parts.headers,
        &target.upstream.headers.request,
        &target.endpoint.host_header(),
    );

    let mut request_context =
        run_request_plugins(state, security, &target, outbound_headers).await?;
    let request_headers = std::mem::take(&mut request_context.headers);

    // ---- circuit breaker + upstream call ---------------------------------
    let host = target.endpoint.bare_host();
    if state.circuit.is_open(
        &host,
        cfg.circuit_breaker_failure_threshold,
        cfg.circuit_breaker_window(),
    ) {
        return Err(
            OagwError::circuit_breaker_open(format!("circuit breaker for {host} is open"))
                .with_extension("host", host)
                .with_extension("upstream_id", target.upstream_uuid.to_string())
                .with_extension(
                    "retry_after_seconds",
                    cfg.circuit_breaker_window().as_secs(),
                ),
        );
    }

    let tls_required = !target.endpoint.scheme.is_plaintext();
    let io = match transport::connect(
        &state.tls,
        &target.endpoint.host,
        target.endpoint.effective_port(),
        tls_required,
        cfg.proxy_timeout(),
    )
    .await
    {
        Ok(io) => io,
        Err(err) => {
            record_failure(state, &host);
            return Err(err.with_extension("upstream_id", target.upstream_uuid.to_string()));
        }
    };

    let response = match transport::send_request(
        io,
        &parts.method,
        &uri,
        request_headers,
        (!body_bytes.is_empty()).then_some(body_bytes),
        cfg.proxy_timeout(),
    )
    .await
    {
        Ok(response) => {
            state.circuit.record_success(&host);
            response
        }
        Err(err) => {
            record_failure(state, &host);
            return Err(err.with_extension("upstream_id", target.upstream_uuid.to_string()));
        }
    };

    Ok(build_response(state, &target, parts, response, rate_headers).await)
}

/// Applies the response-side rules and turns a proxied response into a client
/// response.
async fn build_response(
    state: &std::sync::Arc<ProxyState>,
    target: &Resolved,
    parts: &axum::http::request::Parts,
    response: ProxiedResponse,
    rate_headers: ratelimit::RateLimitVerdict,
) -> Response {
    let mut response_headers = response.headers.clone();

    let mut response_context = ResponseContext {
        status: response.status,
        headers: response_headers.clone(),
        config: Default::default(),
    };
    run_response_plugins(state, target, &mut response_context).await;
    response_headers = response_context.headers;

    headers::strip_hop_by_hop(&mut response_headers);
    response_headers =
        headers::apply_response_headers(response_headers, &target.upstream.headers.response);
    cors::response_headers(Some(&target.cors), &parts.headers, &mut response_headers);
    for (name, value) in ratelimit::headers_for(&rate_headers) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            response_headers.insert(name, value);
        }
    }
    if let Some(request_id) = response_headers
        .get(crate::plugins::REQUEST_ID_HEADER)
        .cloned()
        .or_else(|| {
            parts
                .headers
                .get(crate::plugins::REQUEST_ID_HEADER)
                .cloned()
        })
    {
        response_headers.insert(crate::plugins::REQUEST_ID_HEADER, request_id);
    }
    response_headers.insert(
        http::HeaderName::from_static(headers::ERROR_SOURCE_HEADER),
        http::HeaderValue::from_static("upstream"),
    );

    let body = stream_response(response.body, state.config.proxy_timeout());
    let mut builder = Response::builder().status(response.status);
    for (name, value) in &response_headers {
        builder = builder.header(name, value);
    }
    builder.body(body).unwrap_or_else(|e| {
        problem(&OagwError::protocol_error(format!(
            "failed to build the response: {e}"
        )))
    })
}

/// Records a breaker failure for a host.
fn record_failure(state: &std::sync::Arc<ProxyState>, host: &str) {
    state.circuit.record_failure(
        host,
        state.config.circuit_breaker_failure_threshold,
        state.config.circuit_breaker_window(),
    );
}

/// Streams the upstream body back to the client, applying the idle timeout per
/// chunk so a long-lived SSE stream survives while a stalled one is cut off.
fn stream_response(body: hyper::body::Incoming, idle_timeout: Duration) -> Body {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut body = body;
        // A frame that errors, an ended body and an idle timeout all end the
        // stream; whatever was already forwarded stays forwarded.
        while let Ok(Some(Ok(frame))) = tokio::time::timeout(idle_timeout, body.frame()).await {
            if let Ok(data) = frame.into_data()
                && tx.send(Ok(data)).await.is_err()
            {
                break;
            }
        }
    });
    // `mpsc::Receiver` is a `Stream` only through `tokio-stream`, which is not
    // a dependency here; poll it directly instead.
    Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

/// Reads and buffers the request body, enforcing the configured ceiling before
/// the excess is buffered.
///
/// # Errors
///
/// 413 when the body exceeds the limit, 400 when a chunk cannot be read.
async fn read_body(body: Body, max_bytes: usize) -> Result<Vec<u8>, OagwError> {
    let mut collected = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| OagwError::validation(format!("failed to read the request body: {e}")))?;
        if collected.len() + chunk.len() > max_bytes {
            return Err(OagwError::payload_too_large(format!(
                "the request body exceeds the {max_bytes} byte limit"
            )));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

/// The declared `Content-Length`, when the header is present and well-formed.
fn declared_content_length(parts: &axum::http::request::Parts) -> Result<Option<usize>, OagwError> {
    let Some(value) = parts.headers.get(header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let raw = value.to_str().unwrap_or_default().trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<usize>().map(Some).map_err(|_| {
        OagwError::validation(format!("Content-Length {raw:?} is not a valid integer"))
    })
}

/// Selects the endpoint an upstream request is sent to.
///
/// Implements the `X-OAGW-Target-Host` behaviour matrix of ADR-0001
/// Appendix A.
///
/// # Errors
///
/// 400 for a malformed, unknown or required-but-absent target host.
fn select_endpoint(
    state: &std::sync::Arc<ProxyState>,
    upstream: &Upstream,
    target_host: Option<String>,
) -> Result<Endpoint, OagwError> {
    let endpoints = upstream.normalized_endpoints();
    let alias = upstream.alias.clone().unwrap_or_default();

    if let Some(target) = target_host {
        let target = target.trim().trim_end_matches('.').to_ascii_lowercase();
        if target.contains(':') || target.contains('/') || target.contains('?') {
            return Err(OagwError::invalid_target_host(format!(
                "X-OAGW-Target-Host {target:?} must be a hostname or IP address without port, path or query"
            ))
            .with_extension("alias", alias));
        }
        return endpoints
            .iter()
            .find(|e| e.bare_host() == target)
            .cloned()
            .ok_or_else(|| {
                OagwError::unknown_target_host(format!(
                    "X-OAGW-Target-Host {target:?} does not match any configured endpoint"
                ))
                .with_extension("alias", alias)
                .with_extension("valid_hosts", host_list(&endpoints))
            });
    }

    if endpoints.len() == 1 {
        return Ok(endpoints[0].clone());
    }

    // A common-suffix alias cannot name an endpoint: the header is required.
    if compute_derived_alias(&endpoints).as_deref() == upstream.alias.as_deref() {
        return Err(OagwError::missing_target_host(format!(
            "upstream {alias:?} has {} endpoints; set X-OAGW-Target-Host to one of them",
            endpoints.len()
        ))
        .with_extension("alias", alias)
        .with_extension("valid_hosts", host_list(&endpoints)));
    }

    let mut cursor = state.round_robin.entry(alias.clone()).or_insert(0usize);
    let index = *cursor % endpoints.len();
    *cursor = cursor.wrapping_add(1);
    drop(cursor);
    Ok(endpoints[index].clone())
}

/// The endpoint hosts as a JSON array, for problem extensions.
fn host_list(endpoints: &[Endpoint]) -> serde_json::Value {
    serde_json::Value::from(
        endpoints
            .iter()
            .map(|e| serde_json::Value::from(e.bare_host()))
            .collect::<Vec<_>>(),
    )
}

/// Finds the route matching the request, longest prefix first.
///
/// # Errors
///
/// 404 when no route matches.
fn match_route(
    state: &std::sync::Arc<ProxyState>,
    chain: &[uuid::Uuid],
    upstream_uuid: &uuid::Uuid,
    method: &Method,
    request_path: &str,
) -> Result<Route, OagwError> {
    let chain_rank = |tenant: Option<uuid::Uuid>| {
        tenant
            .and_then(|t| chain.iter().position(|c| *c == t))
            .unwrap_or(usize::MAX)
    };

    let mut candidates: Vec<(usize, Route)> = state
        .store
        .find_routes_for_upstream(*upstream_uuid)
        .into_iter()
        .map(|route| (chain_rank(route.tenant_id), route))
        .collect();
    // Descendant routes take priority over ancestor routes.
    candidates.sort_by_key(|(rank, _)| *rank);

    let mut best: Option<(usize, Route)> = None;
    for (_, route) in &candidates {
        let Some(http) = &route.match_.http else {
            continue;
        };
        if !http
            .methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method.as_str()))
        {
            continue;
        }
        if split_suffix(&http.path, request_path).is_none() {
            continue;
        }
        let depth = path_depth(&http.path);
        let better = best
            .as_ref()
            .is_none_or(|(best_depth, _)| depth > *best_depth);
        if better {
            best = Some((depth, route.clone()));
        }
    }

    match best {
        Some((_, route)) => Ok(route.clone()),
        None => Err(OagwError::route_not_found(format!(
            "no route matches {method} {request_path:?}"
        ))
        .with_extension("path", request_path)),
    }
}

/// Number of segments in a route pattern, used as the prefix-match score.
fn path_depth(path: &str) -> usize {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .count()
}

/// Splits a request path into the matched prefix and the suffix to append.
///
/// [`None`] when the pattern does not match the request path.
#[must_use]
pub fn split_suffix(pattern: &str, request: &str) -> Option<String> {
    let pattern = normalize_suffix(pattern);
    if pattern == "/" {
        return Some(request.to_owned());
    }
    if request == pattern {
        return Some(String::new());
    }
    let rest = request.strip_prefix(&pattern)?;
    rest.starts_with('/').then(|| rest.to_owned())
}

/// Normalizes an alias to its stored form: ASCII lowercase, trailing dot off.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Normalizes a request path: absolute, no trailing slash.
#[must_use]
pub fn normalize_suffix(path: &str) -> String {
    let trimmed = path.trim();
    let path = if trimmed.is_empty() || trimmed == "/" {
        "/".to_owned()
    } else if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    if path.len() > 1 {
        path.trim_end_matches('/').to_owned()
    } else {
        path
    }
}

/// Builds the outbound path from the route and the request path.
///
/// # Errors
///
/// 400 when a suffix is supplied to a `disabled` route, or when the matched
/// route is a gRPC route (no HTTP proxy path exists).
pub(crate) fn build_outbound_path(route: &Route, request_path: &str) -> Result<String, OagwError> {
    let Some(http) = &route.match_.http else {
        return Err(OagwError::route_not_found(
            "the matched route is a gRPC route; gRPC proxying is not implemented",
        ));
    };
    let base = normalize_suffix(&http.path);
    let suffix = split_suffix(&http.path, request_path).unwrap_or_default();
    if !suffix.is_empty() && http.path_suffix_mode == PathSuffixMode::Disabled {
        return Err(OagwError::validation(
            "this route rejects a path suffix (path_suffix_mode is disabled)",
        ));
    }
    if base == "/" {
        return Ok(if suffix.is_empty() {
            "/".to_owned()
        } else {
            suffix
        });
    }
    Ok(format!("{base}{suffix}"))
}

/// Filters the query string against the route's allowlist.
///
/// # Errors
///
/// 400 when a parameter is not on the allowlist.
pub(crate) fn filter_query(route: &Route, query: &str) -> Result<String, OagwError> {
    let allowlist = route
        .match_
        .http
        .as_ref()
        .map(|http| http.query_allowlist.as_slice())
        .unwrap_or_default();

    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for (key, _) in &pairs {
        if !allowlist_permits(allowlist, key) {
            return Err(OagwError::validation(format!(
                "query parameter {key:?} is not allowed by this route"
            )));
        }
    }
    Ok(form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish())
}

fn allowlist_permits(allowlist: &[String], key: &str) -> bool {
    allowlist.iter().any(|k| k == key)
}

/// Builds the outbound URI.
fn build_uri(path: &str, query: &str) -> Result<Uri, OagwError> {
    // Origin form: the request target carries only the path and query. The
    // authority travels in the `Host` header, which `build_outbound_headers`
    // sets from the endpoint.
    let path = normalize_suffix(path);
    let raw = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    Uri::try_from(raw).map_err(|e| OagwError::protocol_error(format!("invalid upstream URI: {e}")))
}

/// Runs the request-side plugin chain: upstream auth, then upstream plugins,
/// then route plugins (ADR-0002).
///
/// # Errors
///
/// Propagates plugin rejections as problems.
pub(crate) async fn run_request_plugins(
    state: &std::sync::Arc<ProxyState>,
    security: &SecurityContext,
    target: &Resolved,
    headers: http::HeaderMap,
) -> Result<RequestContext, OagwError> {
    let mut ctx = crate::plugins::request_context(security, headers, &Default::default());

    if let Some(auth) = &target.upstream.auth.plugin_type {
        let plugin = state.auth_registry.resolve(auth).ok_or_else(|| {
            OagwError::plugin_not_found(format!("auth plugin {auth:?} is not resolvable"))
                .with_extension("upstream_id", target.upstream_uuid.to_string())
        })?;
        plugin
            .authenticate(&mut ctx)
            .await
            .map_err(|e| plugin_error(e, Some(&target.upstream_uuid.to_string())))?;
    }

    let bindings: Vec<(String, serde_json::Map<String, serde_json::Value>)> = target
        .upstream
        .plugins
        .items
        .iter()
        .chain(target.route.plugins.items.iter())
        .map(|item| (item.clone(), serde_json::Map::new()))
        .collect();

    for (reference, config) in bindings {
        apply_request_plugin(state, target, &reference, &config, &mut ctx).await?;
    }
    Ok(ctx)
}

/// Runs one request-side plugin, whichever trait it implements.
async fn apply_request_plugin(
    state: &std::sync::Arc<ProxyState>,
    target: &Resolved,
    reference: &str,
    config: &serde_json::Map<String, serde_json::Value>,
    ctx: &mut RequestContext,
) -> Result<(), OagwError> {
    let upstream_id = target.upstream_uuid.to_string();
    let fail = |e: PluginError| plugin_error(e, Some(&upstream_id));

    if let Some(plugin) = state.auth_registry.resolve(reference) {
        ctx.config = config.clone();
        return plugin.authenticate(ctx).await.map_err(fail);
    }
    if let Some(plugin) = state.guard_registry.resolve(reference) {
        let decision = plugin.guard_request(ctx).await.map_err(fail)?;
        return match decision {
            GuardDecision::Continue => Ok(()),
            GuardDecision::Reject(err) => Err(err),
        };
    }
    if let Some(plugin) = state.transform_registry.resolve(reference) {
        ctx.config = config.clone();
        return plugin.transform_request(ctx).await.map_err(fail);
    }
    // Custom (Starlark) plugins are catalogued but not executed by this data
    // plane; an unresolvable binding fails closed.
    Err(
        OagwError::plugin_not_found(format!("plugin {reference:?} is not resolvable"))
            .with_extension("upstream_id", upstream_id),
    )
}

/// Runs the response-side guard plugins.
///
/// A response guard cannot change what the client is about to receive once the
/// upstream body has started, so a rejection is logged rather than turned into
/// a second error response.
async fn run_response_plugins(
    state: &std::sync::Arc<ProxyState>,
    target: &Resolved,
    ctx: &mut ResponseContext,
) {
    let bindings: Vec<String> = target
        .upstream
        .plugins
        .items
        .iter()
        .chain(target.route.plugins.items.iter())
        .cloned()
        .collect();

    for reference in bindings {
        let Some(plugin) = state.guard_registry.resolve(reference.as_str()) else {
            continue;
        };
        match plugin.guard_response(ctx).await {
            Ok(GuardDecision::Continue) => {}
            Ok(GuardDecision::Reject(err)) => {
                tracing::warn!(plugin = %reference, "response guard rejected the response: {err}");
            }
            Err(err) => {
                tracing::warn!(plugin = %reference, "response guard failed: {err}");
            }
        }
    }
}

/// Converts a plugin failure into a problem.
fn plugin_error(err: PluginError, upstream_id: Option<&str>) -> OagwError {
    let mut error = match err {
        PluginError::Internal(detail) => OagwError::plugin_not_found(detail),
        PluginError::Reject {
            status,
            type_id,
            detail,
        } => OagwError::custom(status, type_id, detail),
    };
    if let Some(id) = upstream_id {
        error = error.with_extension("upstream_id", id);
    }
    error
}

/// Enforces the merged rate limit for the request.
///
/// Effective limit is `min(selected, route, all ancestor enforced limits)` —
/// see `DESIGN.md` on shadowing and `ADR-0003` on inheritance.
///
/// # Errors
///
/// 429 when the bucket is exhausted.
pub(crate) async fn enforce_rate_limit(
    state: &std::sync::Arc<ProxyState>,
    security: &SecurityContext,
    target: &Resolved,
    request_path: &str,
) -> Result<ratelimit::RateLimitVerdict, OagwError> {
    let mut candidates: Vec<crate::domain::model::RateLimit> = state
        .store
        .upstreams_in(&target.chain)
        .into_iter()
        .filter(|candidate| candidate.id != target.upstream.id)
        .filter(|candidate| {
            candidate
                .rate_limit
                .as_ref()
                .is_some_and(|rl| rl.sharing == SharingMode::Enforce)
        })
        .filter_map(|candidate| candidate.rate_limit)
        .collect();
    if let Some(limit) = &target.upstream.rate_limit {
        candidates.push(limit.clone());
    }
    if let Some(limit) = &target.route.rate_limit {
        candidates.push(limit.clone());
    }

    let Some(merged) = ratelimit::merge_limits(&candidates, Default::default()) else {
        return Ok(ratelimit::RateLimitVerdict::Allowed(None));
    };

    let key = ratelimit::scope_key(
        merged.scope,
        security.subject_tenant_id(),
        security.subject_id(),
        ratelimit::UNKNOWN_CLIENT_IP,
        target.route.id.as_deref().unwrap_or(request_path),
    );

    let verdict = state.rate_limiter.check(
        &key,
        &merged,
        merged.rate_per_second(),
        merged.capacity(),
        merged.cost,
        std::time::Instant::now(),
    );

    if let ratelimit::RateLimitVerdict::Limited { retry_after, .. } = &verdict {
        let err = OagwError::rate_limit_exceeded(format!(
            "rate limit of {} per {} exceeded",
            merged.sustained.rate,
            merged.sustained.window.label()
        ))
        .with_extension("upstream_id", target.upstream_uuid.to_string())
        .with_extension("retry_after_seconds", *retry_after);
        return Err(err);
    }
    Ok(verdict)
}

/// HTTP handler for `/proxy/{alias}/{*path_suffix}`.
pub async fn handler(
    axum::extract::Extension(state): axum::extract::Extension<std::sync::Arc<ProxyState>>,
    axum::extract::Extension(security): axum::extract::Extension<SecurityContext>,
    axum::extract::Path((alias, path_suffix)): axum::extract::Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    proxy(state, security, request, alias, path_suffix).await
}

/// HTTP handler for `/proxy/{alias}`.
pub async fn handler_root(
    axum::extract::Extension(state): axum::extract::Extension<std::sync::Arc<ProxyState>>,
    axum::extract::Extension(security): axum::extract::Extension<SecurityContext>,
    axum::extract::Path(alias): axum::extract::Path<String>,
    request: axum::extract::Request,
) -> Response {
    proxy(state, security, request, alias, String::new()).await
}

/// A status code's problem title.
#[must_use]
pub fn status_title(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Upstream Service Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Request Failed",
    }
}
