// Updated: 2026-09-01 by Constructor Tech
//! The proxy handler: `{METHOD} /oagw/v1/proxy/{alias}/…`.
//!
//! This is the Data Plane's only public face. It composes the pieces the
//! domain and infra layers provide, in the order the design pins:
//!
//! 0. answer a CORS preflight locally, without touching any upstream,
//! 1. resolve the alias through the tenant chain,
//! 2. match a route,
//! 3. enforce the rate limit and the circuit breaker,
//! 4. validate the CORS origin and method of an actual cross-origin request,
//! 5. run the plugin chain (auth, guards, request transforms),
//! 6. build the upstream request (header transformation, target host),
//! 7. dial, read, and hand the response back — streaming when the upstream
//!    streams, and piped both ways when it upgrades.

use std::collections::BTreeMap;
use std::sync::Mutex;

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::CorsConfig;
use crate::domain::plugin::ResponseContext;
use crate::domain::services::management::ManagementService;
use crate::infra::proxy::engine::{
    Engine, ResolvedEndpoint, ResolvedRequest, UpgradeHandshake, UpstreamResponse,
};
use crate::infra::proxy::error::{ERROR_SOURCE_HEADER, GatewayError, SOURCE_GATEWAY};
use crate::infra::proxy::headers;
use crate::infra::proxy::service::{self, ProxyService, request_context, suffix_admitted};

/// The header the caller uses to pick one endpoint out of a multi-endpoint
/// upstream (design §Header-Transformation).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Headers OAGW adds to a proxied request.
const ALIAS_HEADER: &str = "x-oagw-alias";
const UPSTREAM_ID_HEADER: &str = "x-oagw-upstream-id";

/// How long a client may cache a preflight answer.
const PREFLIGHT_MAX_AGE: &str = "86400";

/// Everything the handler needs, built once per gear.
#[derive(Clone)]
pub struct ProxyState {
    pub proxy: Arc<ProxyService>,
    pub management: Arc<ManagementService>,
    pub config: Arc<OagwConfig>,
}

impl std::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyState").finish_non_exhaustive()
    }
}

/// Handle one proxied request.
pub async fn handle(
    Extension(state): Extension<ProxyState>,
    Path(path_rest): Path<String>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let started = Instant::now();
    let (parts, body) = req.into_parts();

    // The route pattern collapses the alias and the suffix into one segment
    // list; the alias is always the first.
    let (alias, suffix) = match path_rest.split_once('/') {
        Some((a, s)) => (a.to_owned(), s.to_owned()),
        None => (path_rest.clone(), String::new()),
    };

    let ctx = match parts.extensions.get::<SecurityContext>() {
        Some(c) => c.clone(),
        None => SecurityContext::anonymous(),
    };
    // hyper hands the upgrade future to the handler through the extensions;
    // only an upgrading request has one.
    let on_upgrade = parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned();

    let method = parts.method.clone();
    let path = format!("/{suffix}");
    let query = query.unwrap_or_default();

    // 0. A preflight never reaches an upstream: it is answered here, with no
    // upstream resolution and no tenant context, and the actual request is
    // where the origin is actually checked.
    if let Some(resp) = preflight(&method, &parts.headers) {
        return resp;
    }

    let body_bytes = match read_body(body, parts.headers.clone(), &state.config).await {
        Ok(b) => b,
        Err(err) => return err.into_response(),
    };

    let proxied = ProxiedRequest {
        alias: &alias,
        method: &method,
        path: &path,
        query: &query,
        inbound: parts.headers,
        body: body_bytes,
        on_upgrade,
    };
    match proxy_exchange(&state, &ctx, proxied).await {
        Ok(resp) => {
            tracing::info!(
                alias = %alias,
                path = %path,
                method = %method,
                status = resp.status().as_u16(),
                duration_ms = started.elapsed().as_millis() as u64,
                "proxied request"
            );
            resp
        }
        Err(err) => err.into_response(),
    }
}

/// Answer a CORS preflight locally: 204, echoing what the browser asked for.
///
/// Returns `None` for anything that is not a preflight, which then continues
/// through the normal proxy path.
fn preflight(method: &axum::http::Method, inbound: &HeaderMap) -> Option<Response> {
    if method != axum::http::Method::OPTIONS {
        return None;
    }
    let origin = header_str(inbound, "origin")?;
    let requested = header_str(inbound, "access-control-request-method")?;
    if origin.is_empty() || requested.is_empty() {
        return None;
    }

    let mut out = HeaderMap::new();
    insert(&mut out, "access-control-allow-origin", origin);
    insert(&mut out, "access-control-allow-methods", requested);
    if let Some(requested_headers) = header_str(inbound, "access-control-request-headers") {
        insert(&mut out, "access-control-allow-headers", requested_headers);
    }
    insert(&mut out, "access-control-max-age", PREFLIGHT_MAX_AGE);
    out.insert(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    *response.headers_mut() = out;
    Some(response)
}

/// The refusal a cross-origin request earns when a route matched by path but
/// not by method.
///
/// `None` when the request is not cross-origin, when the upstream has no CORS
/// policy of its own, or when the policy admits the method after all — in
/// which case the route's own 405 is the answer.
#[must_use]
fn cross_origin_method_rejection(
    upstream: &crate::domain::dto::Upstream,
    method: &axum::http::Method,
    inbound: &HeaderMap,
) -> Option<GatewayError> {
    let cors = upstream.cors.as_ref().filter(|c| c.is_enabled())?;
    let origin = header_str(inbound, "origin").filter(|o| !o.is_empty())?;
    if cors
        .methods()
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()))
    {
        return None;
    }
    Some(GatewayError::cors_method(method.as_str(), origin))
}

/// Validate an actual cross-origin request against the effective CORS config,
/// and return the response headers CORS obliges the gateway to add.
///
/// `None` when the request is not cross-origin or CORS is not enabled here.
///
/// # Errors
///
/// [`GatewayError::cors_origin`] / [`GatewayError::cors_method`] when the
/// origin or the method is not admitted.
fn check_cors(
    cors: &CorsConfig,
    method: &axum::http::Method,
    inbound: &HeaderMap,
) -> Result<Option<HeaderMap>, GatewayError> {
    let Some(origin) = header_str(inbound, "origin").filter(|o| !o.is_empty()) else {
        return Ok(None);
    };
    if !cors.enabled {
        return Ok(None);
    }

    if !origin_allowed(cors, origin) {
        return Err(GatewayError::cors_origin(origin));
    }
    if !cors
        .methods()
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(GatewayError::cors_method(method.as_str(), origin));
    }

    let mut out = HeaderMap::new();
    insert(&mut out, "access-control-allow-origin", origin);
    if cors.credentials() {
        insert(&mut out, "access-control-allow-credentials", "true");
    }
    if !cors.expose_headers.is_empty() {
        insert(
            &mut out,
            "access-control-expose-headers",
            &cors.expose_headers.join(", "),
        );
    }
    out.insert(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static("Origin"),
    );
    Ok(Some(out))
}

/// Exact, port- and protocol-sensitive origin match; `*` admits everything.
fn origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    cors.allowed_origins.iter().any(|o| o == "*")
        || cors.allowed_origins.iter().any(|o| o == origin)
}

/// Read a header as a trimmed UTF-8 string.
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// Insert a header, dropping a value the wire cannot carry.
fn insert(out: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        axum::http::HeaderName::try_from(name),
        axum::http::HeaderValue::try_from(value),
    ) {
        out.insert(name, value);
    }
}

/// Read and size-check the request body.
///
/// # Errors
///
/// [`GatewayError::payload_too_large`] when the body exceeds the limit,
/// [`GatewayError::request_timeout`] when it does not arrive in time.
pub async fn read_body(
    body: Body,
    inbound: HeaderMap,
    config: &OagwConfig,
) -> Result<Bytes, GatewayError> {
    // Content-Length is checked before the body is read, so an oversized
    // request never reaches the wire.
    if let Some(len) = inbound
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && len > config.max_payload_bytes
    {
        return Err(GatewayError::payload_too_large(config.max_payload_bytes));
    }

    let mut out = Vec::with_capacity(1024);
    let mut stream = body.into_data_stream();
    while let Some(chunk) = tokio::time::timeout(config.proxy_timeout, stream.next())
        .await
        .map_err(|_| GatewayError::request_timeout("the request body could not be read in time"))?
        .transpose()
        .map_err(|e| GatewayError::validation(format!("reading the request body: {e}")))?
    {
        if out.len() + chunk.len() > config.max_payload_bytes {
            return Err(GatewayError::payload_too_large(config.max_payload_bytes));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

/// The one request the handler is putting through the gateway, bundling what
/// the exchange needs off the wire. `alias`, `method`, `path` and `query` are
/// borrowed from the handler's own locals; the headers, body and upgrade
/// future are moved in, because the exchange alone uses them from here on.
pub struct ProxiedRequest<'a> {
    pub alias: &'a str,
    pub method: &'a axum::http::Method,
    pub path: &'a str,
    pub query: &'a str,
    pub inbound: HeaderMap,
    pub body: Bytes,
    pub on_upgrade: Option<hyper::upgrade::OnUpgrade>,
}

/// The whole exchange above the wire.
///
/// # Errors
///
/// [`GatewayError`] for every failure the gateway originates.
#[allow(clippy::too_many_lines)]
pub async fn proxy_exchange(
    state: &ProxyState,
    security: &SecurityContext,
    proxied: ProxiedRequest<'_>,
) -> Result<Response, GatewayError> {
    let ProxiedRequest {
        alias,
        method,
        path,
        query,
        inbound,
        body,
        on_upgrade,
    } = proxied;
    let proxy = &state.proxy;
    let config = &state.config;

    // The caller must be allowed to proxy at all.
    state
        .management
        .authorize_proxy(security)
        .await
        .map_err(GatewayError::from)?;

    // 1. Resolve the alias through the tenant chain.
    let (upstream, upstream_tenant) = proxy.resolve_upstream(security, alias).await?;
    let upstream_id = upstream.id.unwrap_or_default();

    // 2. Match a route.
    let route = match proxy.match_route(&upstream, method, path).await {
        Ok(route) => route,
        Err(err) if err.status == StatusCode::METHOD_NOT_ALLOWED => {
            // A cross-origin request is governed by the CORS policy before the
            // route's own method gate: ADR-0004 pins a method the policy does
            // not admit at 403, and a browser cannot tell a 405 from a 403, so
            // the more specific answer is the one to give.
            match cross_origin_method_rejection(&upstream, method, &inbound) {
                Some(rejection) => return Err(rejection),
                None => return Err(err),
            }
        }
        Err(err) => return Err(err),
    };

    // 3. The suffix rule: a route that disables suffixes rejects one.
    let route_path = route
        .r#match
        .http
        .as_ref()
        .map(|m| m.path.clone())
        .unwrap_or_default();
    let suffix = path.strip_prefix(&route_path).unwrap_or_default();
    let suffix_mode = route
        .r#match
        .http
        .as_ref()
        .map(|m| m.path_suffix_mode)
        .unwrap_or_default();
    if !suffix.is_empty() && !suffix_admitted(suffix_mode) {
        return Err(GatewayError::validation(format!(
            "route '{route_path}' does not accept a path suffix"
        ))
        .with("path", serde_json::json!(path)));
    }

    let effective = service::merge(&upstream, &route);

    // 4. CORS, on the actual request: origin, then method.
    let cors_headers = match &effective.cors {
        Some(cors) => check_cors(cors, method, &inbound)?,
        None => None,
    };
    let mut rate_limit_headers = None;

    // 5. Rate limit, then the circuit breaker.
    let request_id = format!("req_{}", Uuid::new_v4().simple());
    let mut req_ctx = request_context(
        &request_id,
        security.clone(),
        upstream_tenant,
        &upstream_id,
        alias,
        path,
        query,
        method,
        inbound.clone(),
        body,
    );
    if let Some(limit) = &effective.rate_limit {
        let outcome = proxy
            .enforce_rate_limit(
                limit,
                upstream_tenant,
                &route.id.unwrap_or_default(),
                &req_ctx,
            )
            .map_err(|e| e.with("alias", serde_json::json!(alias)))?;
        // The budget this exchange spent travels with the response, whether or
        // not it was the last one the bucket had.
        rate_limit_headers = Some(outcome.headers());
    }
    proxy.check_breaker(&upstream_id)?;

    // 6. The plugin chain: auth, then guards, then request transforms.
    proxy
        .run_request_plugins(&effective.plugins, &mut req_ctx)
        .await?;

    // 7. Resolve the endpoints — DNS and the SSRF screen, before anything is
    //    dialled — and pick the one this request is destined for.
    let endpoints = resolve_targets(config, &upstream).await?;
    let target = select_target(&upstream, &endpoints, &inbound)?;

    // 8. Build the upstream request.
    let upstream_path = build_upstream_path(&route_path, suffix, query);
    let extra: BTreeMap<&'static str, String> = BTreeMap::from([
        (ALIAS_HEADER, alias.to_owned()),
        (
            UPSTREAM_ID_HEADER,
            crate::gts::instance_id(crate::gts::UPSTREAM_TYPE, upstream_id),
        ),
    ]);
    let mut upstream_headers = headers::build_upstream_request(
        &req_ctx.headers,
        &effective.headers.request,
        extra.into_iter(),
    );
    upstream_headers.insert(
        axum::http::header::HOST,
        axum::http::HeaderValue::from_str(&target.host_header)
            .map_err(|e| GatewayError::validation(format!("invalid Host header: {e}")))?,
    );
    // The routing header named a *gateway* endpoint, so it has done its work.
    upstream_headers.remove(TARGET_HOST_HEADER);

    // Whether the caller asked for an upgrade is decided from the inbound
    // head: the engine reports what the upstream actually did, and a 101 is
    // the only thing that turns this into a raw pipe.
    let upgrade = headers::requested_upgrade(&inbound)
        .map(|(protocol, headers)| UpgradeHandshake { protocol, headers });
    let request = ResolvedRequest {
        method: method.clone(),
        uri: upstream_path,
        headers: upstream_headers,
        body: req_ctx.body.clone(),
        endpoint: target,
        read_timeout: config.proxy_timeout,
        write_timeout: config.connect_timeout,
        upgrade,
    };

    // 9. Dial.
    let engine = Engine::new((**config).clone());
    match engine.send(request).await {
        Ok(UpstreamResponse::Upgraded {
            status,
            headers: upstream_headers,
            stream,
        }) => {
            proxy.record_upstream_success(&upstream_id);
            Ok(upgrade_response(
                status,
                upstream_headers,
                stream,
                on_upgrade,
            ))
        }
        Ok(UpstreamResponse::Response {
            status,
            headers: upstream_headers,
            body: stream,
        }) => {
            proxy.record_upstream_success(&upstream_id);
            Ok(response_phase(
                proxy,
                &effective,
                status,
                upstream_headers,
                stream,
                cors_headers,
                rate_limit_headers,
            )
            .await)
        }
        Err(err) => {
            proxy.record_upstream_failure(&upstream_id);
            Err(err)
        }
    }
}

/// The response phase: run the response guards and transforms against the
/// head the upstream sent, apply the response header rules, and hand the body
/// straight through.
async fn response_phase(
    proxy: &ProxyService,
    effective: &service::Effective,
    status: StatusCode,
    upstream_headers: HeaderMap,
    stream: futures_util::stream::BoxStream<'static, Result<Bytes, GatewayError>>,
    cors_headers: Option<HeaderMap>,
    rate_limit_headers: Option<HeaderMap>,
) -> Response {
    // One chunk is read ahead so the response phase sees the status the
    // upstream actually returned before anything reaches the caller; the
    // chunk is forwarded again immediately, so nothing buffers whole.
    let mut stream = stream;
    let first = stream.next().await;

    let mut resp_ctx = ResponseContext {
        status,
        headers: upstream_headers,
        body: None,
        config: BTreeMap::new(),
    };
    if let Err(err) = proxy
        .run_response_plugins(&effective.plugins, &mut resp_ctx)
        .await
    {
        return err.into_response();
    }

    let client_headers =
        headers::build_client_response(&resp_ctx.headers, &effective.headers.response);
    let client_headers = match cors_headers {
        Some(cors) => extend(client_headers, cors),
        None => client_headers,
    };
    let client_headers = match rate_limit_headers {
        Some(limit) => extend(client_headers, limit),
        None => client_headers,
    };
    streaming_response(resp_ctx.status, client_headers, first, stream)
}

/// Assemble the response handed back to the caller.
fn streaming_response(
    status: StatusCode,
    mut client_headers: HeaderMap,
    first: Option<Result<Bytes, GatewayError>>,
    rest: futures_util::stream::BoxStream<'static, Result<Bytes, GatewayError>>,
) -> Response {
    let stream: futures_util::stream::BoxStream<'static, Result<Bytes, GatewayError>> = match first
    {
        Some(Ok(chunk)) if !chunk.is_empty() => {
            Box::pin(futures_util::stream::once(async move { Ok(chunk) }).chain(rest))
        }
        Some(Err(e)) => Box::pin(futures_util::stream::once(async move { Err(e) }).chain(rest)),
        _ => rest,
    };

    client_headers.insert(
        ERROR_SOURCE_HEADER,
        axum::http::HeaderValue::from_static(SOURCE_GATEWAY),
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = client_headers;
    response
}

/// Hand an upgraded connection back to the caller and pipe it both ways.
fn upgrade_response(
    status: StatusCode,
    upstream_headers: HeaderMap,
    stream: pingora_core::protocols::Stream,
    on_upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> Response {
    let mut client_headers = headers::build_client_response(
        &upstream_headers,
        &crate::domain::dto::ResponseHeaderRules::default(),
    );
    // An upgraded connection has no framing of its own to describe.
    client_headers.remove(axum::http::header::TRANSFER_ENCODING);
    client_headers.remove(axum::http::header::CONTENT_LENGTH);
    client_headers.insert(
        ERROR_SOURCE_HEADER,
        axum::http::HeaderValue::from_static(SOURCE_GATEWAY),
    );

    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    *response.headers_mut() = client_headers;

    if let Some(on_upgrade) = on_upgrade {
        let mut upstream = stream;
        tokio::spawn(async move {
            // The caller's half of the upgrade is only ready once this
            // response has been written, so the pipe is moved off the handler.
            match on_upgrade.await {
                Ok(upgraded) => {
                    let mut client = crate::infra::proxy::upgrade::HyperIo(upgraded);
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Err(_) => {
                    // The caller went away; the upstream socket closes with it.
                }
            }
        });
    }
    response
}

/// Concatenate two header sets, `extra` winning.
fn extend(mut base: HeaderMap, extra: HeaderMap) -> HeaderMap {
    base.extend(extra);
    base
}

/// Resolve every endpoint and screen it.
///
/// # Errors
///
/// [`GatewayError`] when the SSRF policy denies a host or nothing resolves.
async fn resolve_targets(
    config: &OagwConfig,
    upstream: &crate::domain::dto::Upstream,
) -> Result<Vec<ResolvedEndpoint>, GatewayError> {
    Engine::new(config.clone())
        .resolve_endpoints(&upstream.server.endpoints)
        .await
}

/// Pick the endpoint the request is destined for.
///
/// The behaviour matrix is:
///
/// | endpoints | alias      | `X-OAGW-Target-Host` | behaviour |
/// |---|---|---|---|
/// | 1         | any        | absent | that endpoint |
/// | 1         | any        | present | validated, then that endpoint |
/// | 2+        | explicit   | absent | round-robin |
/// | 2+        | explicit   | present | the named endpoint |
/// | 2+        | derived suffix | absent | 400 |
/// | 2+        | derived suffix | present | the named endpoint |
///
/// "Explicit" and "derived suffix" are distinguished by comparing the stored
/// alias with what [`alias::derive`] would produce from the endpoints: the
/// derived value names the members individually, an operator-supplied one does
/// not.
///
/// # Errors
///
/// [`GatewayError`] when the target header is malformed, unknown, or required
/// and missing.
fn select_target(
    upstream: &crate::domain::dto::Upstream,
    endpoints: &[ResolvedEndpoint],
    inbound: &HeaderMap,
) -> Result<ResolvedEndpoint, GatewayError> {
    if endpoints.is_empty() {
        return Err(GatewayError::link_unavailable(
            "the upstream has no endpoint that resolves",
        ));
    }

    let explicit = inbound
        .get(TARGET_HOST_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    if let Some(want) = explicit {
        let normalized = crate::infra::proxy::alias::normalize(want);
        if !is_valid_target_host(&normalized) {
            return Err(GatewayError::invalid_target_host(want));
        }
        if let Some(ep) = endpoints
            .iter()
            .find(|ep| ep.host == normalized || ep.host_header == normalized)
        {
            return Ok(ep.clone());
        }
        return Err(GatewayError::unknown_target_host(
            &normalized,
            &host_list(endpoints),
        ));
    }

    // A pool whose alias *is* the derived summary of its members — the
    // common-suffix form — cannot be dialled without naming one. An alias an
    // operator chose (`my-service`) names the pool as a unit, so the members
    // are interchangeable and round-robin is the correct answer. Deriving
    // again and comparing to the stored alias is what separates the two: the
    // members may well share a suffix either way.
    if endpoints.len() > 1 && alias_summarizes_pool(upstream) {
        return Err(GatewayError::missing_target_host(
            upstream.alias.as_deref().unwrap_or_default(),
            &host_list(endpoints),
        ));
    }

    // Round-robin over the pool.
    let index = next_round_robin(&upstream.id.unwrap_or_default(), endpoints.len());
    Ok(endpoints[index].clone())
}

fn host_list(endpoints: &[ResolvedEndpoint]) -> Vec<String> {
    endpoints.iter().map(|ep| ep.host.clone()).collect()
}

/// Whether the upstream's alias is the name its own endpoints derive — the
/// common-suffix summary, which stands for the members individually rather
/// than for the pool as a unit.
///
/// The stored alias already equals the derivation when there is one: the
/// management API rejects a supplied alias that differs from it (DESIGN,
/// "Alias behavior is determined entirely by endpoint type"). So the question
/// is only whether the endpoints could be named at all — an alias like
/// `vendor.com:8443` summarises its members, while the alias of an IP pool or
/// of hosts with no common suffix was chosen by hand and stands for the pool
/// as a unit.
#[must_use]
fn alias_summarizes_pool(upstream: &crate::domain::dto::Upstream) -> bool {
    let Some(alias) = upstream.alias.as_deref() else {
        return false;
    };
    match crate::infra::proxy::alias::derive(&upstream.server.endpoints) {
        crate::infra::proxy::alias::AliasDerivation::Derived(derived) => derived == alias,
        crate::infra::proxy::alias::AliasDerivation::RequiresExplicit(_) => false,
    }
}

/// Whether `host` is a bare hostname or IP literal — no port, no path.
#[must_use]
pub fn is_valid_target_host(host: &str) -> bool {
    if host.contains('/') || host.contains('?') || host.contains('#') || host.contains(':') {
        return false;
    }
    crate::domain::dto::is_valid_host(host)
}

/// A process-wide round-robin cursor per upstream.
fn next_round_robin(upstream_id: &Uuid, len: usize) -> usize {
    // One lock guards every upstream's cursor: the critical section is a
    // handful of instructions and it is never held across an await.
    static COUNTERS: std::sync::OnceLock<Mutex<BTreeMap<Uuid, u64>>> = std::sync::OnceLock::new();
    let counters = COUNTERS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut map = counters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cursor = map.entry(*upstream_id).or_insert(0);
    let next = (*cursor % len.max(1) as u64) as usize;
    *cursor = cursor.wrapping_add(1);
    next
}

/// The path sent upstream: the route's prefix, the suffix appended, the query
/// preserved verbatim.
#[must_use]
pub fn build_upstream_path(route_path: &str, suffix: &str, query: &str) -> String {
    let mut out = route_path.trim_end_matches('/').to_owned();
    if !suffix.is_empty() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(suffix.trim_start_matches('/'));
    }
    if out.is_empty() {
        out.push('/');
    }
    // A route matched at the root leaves an empty prefix, and the join then
    // hands the engine a relative URI, which pingora rejects outright.
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    if !query.is_empty() {
        out.push('?');
        out.push_str(query);
    }
    out
}

/// Whether the caller asked for a protocol upgrade.
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap) -> bool {
    let wants_upgrade = headers
        .get(axum::http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.to_ascii_lowercase()
                .split(',')
                .any(|t| t.trim() == "upgrade")
        });
    let has_protocol = headers
        .get(axum::http::header::UPGRADE)
        .is_some_and(|v| !v.is_empty());
    wants_upgrade && has_protocol
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Endpoint, Scheme};
    use http_body_util::Full;

    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    fn resolved(host: &str, port: u16) -> ResolvedEndpoint {
        ResolvedEndpoint {
            scheme: crate::domain::dto::Scheme::Http,
            host: host.to_owned(),
            port,
            addr: format!("127.0.0.1:{port}").parse().unwrap(),
            host_header: format!("{host}:{port}"),
        }
    }

    fn upstream(alias: &str, endpoints: Vec<Endpoint>) -> crate::domain::dto::Upstream {
        crate::domain::dto::Upstream {
            id: None,
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: vec![],
            server: crate::domain::dto::ServerConfig { endpoints },
            protocol: crate::domain::dto::Protocol::Http,
            auth: None,
            headers: crate::domain::dto::HeadersConfig::default(),
            plugins: crate::domain::dto::PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_unread() {
        let config = OagwConfig {
            max_payload_bytes: 16,
            ..OagwConfig::default()
        };
        let mut inbound = HeaderMap::new();
        inbound.insert(axum::http::header::CONTENT_LENGTH, "64".parse().unwrap());
        let err = read_body(Body::from("x"), inbound, &config)
            .await
            .expect_err("declared length is over the limit");
        assert_eq!(err.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(err.type_id, crate::gts::ERR_PAYLOAD_TOO_LARGE);
        assert_eq!(err.body()["max_payload_bytes"], 16);
    }

    #[tokio::test]
    async fn an_undersized_body_passes_whole() {
        let config = OagwConfig {
            max_payload_bytes: 16,
            ..OagwConfig::default()
        };
        let body = read_body(Body::from("hello"), HeaderMap::new(), &config)
            .await
            .expect("under the limit");
        assert_eq!(&body[..], b"hello");
    }

    #[tokio::test]
    async fn a_streamed_body_over_the_limit_is_refused_mid_read() {
        // No Content-Length, so the refusal has to come from the running total.
        let config = OagwConfig {
            max_payload_bytes: 4,
            ..OagwConfig::default()
        };
        let err = read_body(
            Body::new(Full::new(bytes::Bytes::from_static(b"abcdefghij"))),
            HeaderMap::new(),
            &config,
        )
        .await
        .expect_err("the running total is over the limit");
        assert_eq!(err.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn a_single_endpoint_is_selected_without_a_header() {
        let u = upstream(
            "api.example.com",
            vec![endpoint(Scheme::Http, "api.example.com", 8080)],
        );
        let eps = vec![resolved("api.example.com", 8080)];
        let target = select_target(&u, &eps, &HeaderMap::new()).unwrap();
        assert_eq!(target.host, "api.example.com");
    }

    #[test]
    fn a_target_header_on_a_single_endpoint_is_validated() {
        let u = upstream(
            "api.example.com",
            vec![endpoint(Scheme::Http, "api.example.com", 8080)],
        );
        let eps = vec![resolved("api.example.com", 8080)];
        let mut inbound = HeaderMap::new();
        inbound.insert(TARGET_HOST_HEADER, "api.example.com".parse().unwrap());
        assert_eq!(
            select_target(&u, &eps, &inbound).unwrap().host,
            "api.example.com"
        );
    }

    #[test]
    fn an_explicit_multi_endpoint_pool_load_balances() {
        let u = upstream(
            "my-service",
            vec![
                endpoint(Scheme::Http, "10.0.0.1", 8080),
                endpoint(Scheme::Http, "10.0.0.2", 8080),
            ],
        );
        let eps = vec![resolved("10.0.0.1", 8080), resolved("10.0.0.2", 8080)];
        // IP endpoints cannot be named by a derivation, so the alias was
        // supplied — it stands for the whole pool and the requests are spread
        // across it.
        let first = select_target(&u, &eps, &HeaderMap::new()).unwrap();
        let second = select_target(&u, &eps, &HeaderMap::new()).unwrap();
        assert_ne!(first.host, second.host);
    }

    #[test]
    fn a_common_suffix_pool_requires_a_target_host() {
        let u = upstream(
            "vendor.com",
            vec![
                endpoint(Scheme::Http, "us.vendor.com", 80),
                endpoint(Scheme::Http, "eu.vendor.com", 80),
            ],
        );
        let eps = vec![resolved("us.vendor.com", 80), resolved("eu.vendor.com", 80)];
        let err = select_target(&u, &eps, &HeaderMap::new()).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.type_id, crate::gts::ERR_MISSING_TARGET_HOST);
        assert_eq!(err.body()["valid_hosts"][0], "us.vendor.com");
    }

    #[test]
    fn a_target_header_names_a_member_of_the_pool() {
        let u = upstream(
            "vendor.com",
            vec![
                endpoint(Scheme::Http, "us.vendor.com", 8080),
                endpoint(Scheme::Http, "eu.vendor.com", 8080),
            ],
        );
        let eps = vec![
            resolved("us.vendor.com", 8080),
            resolved("eu.vendor.com", 8080),
        ];
        let mut inbound = HeaderMap::new();
        inbound.insert(TARGET_HOST_HEADER, "eu.vendor.com".parse().unwrap());
        assert_eq!(
            select_target(&u, &eps, &inbound).unwrap().host,
            "eu.vendor.com"
        );
    }

    #[test]
    fn a_target_header_with_a_port_is_rejected() {
        let u = upstream(
            "vendor.com",
            vec![endpoint(Scheme::Http, "us.vendor.com", 8080)],
        );
        let eps = vec![resolved("us.vendor.com", 8080)];
        let mut inbound = HeaderMap::new();
        inbound.insert(TARGET_HOST_HEADER, "us.vendor.com:8080".parse().unwrap());
        let err = select_target(&u, &eps, &inbound).unwrap_err();
        assert_eq!(err.type_id, crate::gts::ERR_INVALID_TARGET_HOST);
    }

    #[test]
    fn a_target_header_naming_no_endpoint_is_rejected() {
        let u = upstream(
            "vendor.com",
            vec![endpoint(Scheme::Http, "us.vendor.com", 8080)],
        );
        let eps = vec![resolved("us.vendor.com", 8080)];
        let mut inbound = HeaderMap::new();
        inbound.insert(TARGET_HOST_HEADER, "apac.vendor.com".parse().unwrap());
        let err = select_target(&u, &eps, &inbound).unwrap_err();
        assert_eq!(err.type_id, crate::gts::ERR_UNKNOWN_TARGET_HOST);
    }

    #[test]
    fn no_resolvable_endpoint_is_a_link_unavailable() {
        let u = upstream(
            "vendor.com",
            vec![endpoint(Scheme::Http, "us.vendor.com", 8080)],
        );
        let err = select_target(&u, &[], &HeaderMap::new()).unwrap_err();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_derived_suffix_alias_summarizes_the_pool_with_its_port() {
        // Non-standard port: the derivation appends `:8080` (DESIGN, "Alias
        // Normalization"), and that is the only alias the API would have
        // accepted.
        let u = upstream(
            "vendor.com:8080",
            vec![
                endpoint(Scheme::Http, "us.vendor.com", 8080),
                endpoint(Scheme::Http, "eu.vendor.com", 8080),
            ],
        );
        assert!(alias_summarizes_pool(&u));
    }

    #[test]
    fn an_alias_the_endpoints_cannot_derive_does_not_summarize_the_pool() {
        // Hosts with no common suffix, or IP endpoints, are named by hand —
        // and a hand-picked alias stands for the whole pool.
        let u = upstream(
            "my-service",
            vec![
                endpoint(Scheme::Http, "10.0.0.1", 8080),
                endpoint(Scheme::Http, "10.0.0.2", 8080),
            ],
        );
        assert!(!alias_summarizes_pool(&u));

        let u = upstream(
            "my-service",
            vec![
                endpoint(Scheme::Http, "server-a.one.test", 8080),
                endpoint(Scheme::Http, "server-b.other.test", 8080),
            ],
        );
        assert!(!alias_summarizes_pool(&u));
    }

    #[test]
    fn the_upstream_path_keeps_the_query_and_drops_the_trailing_slash() {
        assert_eq!(
            build_upstream_path("/v1", "chat/completions", "a=1"),
            "/v1/chat/completions?a=1"
        );
        assert_eq!(build_upstream_path("/v1/", "", ""), "/v1");
        assert_eq!(build_upstream_path("", "", ""), "/");
        // A route matched at the root still produces an absolute path.
        assert_eq!(build_upstream_path("/", "v1/status", ""), "/v1/status");
        assert_eq!(build_upstream_path("/", "", "a=1"), "/?a=1");
    }

    #[test]
    fn an_upgrade_needs_both_the_connection_and_the_upgrade_header() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::CONNECTION,
            "keep-alive".parse().unwrap(),
        );
        h.insert(axum::http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(!is_upgrade_request(&h));

        h.insert(axum::http::header::CONNECTION, "Upgrade".parse().unwrap());
        assert!(is_upgrade_request(&h));

        assert!(!is_upgrade_request(&HeaderMap::new()));
    }

    #[test]
    fn a_preflight_is_answered_locally() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        h.insert("access-control-request-method", "POST".parse().unwrap());
        h.insert(
            "access-control-request-headers",
            "Content-Type".parse().unwrap(),
        );

        let resp = preflight(&axum::http::Method::OPTIONS, &h).expect("preflight");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let headers = resp.headers();
        assert_eq!(
            headers.get("access-control-allow-origin").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(headers.get("access-control-allow-methods").unwrap(), "POST");
        assert_eq!(
            headers.get("access-control-allow-headers").unwrap(),
            "Content-Type"
        );
        assert_eq!(headers.get("access-control-max-age").unwrap(), "86400");
    }

    #[test]
    fn a_plain_options_request_is_not_a_preflight() {
        assert!(preflight(&axum::http::Method::OPTIONS, &HeaderMap::new()).is_none());
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        assert!(preflight(&axum::http::Method::OPTIONS, &h).is_none());
        assert!(preflight(&axum::http::Method::POST, &h).is_none());
    }

    fn cors(origins: &[&str], methods: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing: Default::default(),
            enabled: true,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            expose_headers: vec![],
            allow_credentials: Some(false),
        }
    }

    #[test]
    fn an_allowed_origin_gets_cors_response_headers() {
        let cfg = cors(&["https://app.example.com"], &["GET", "POST"]);
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        let out = check_cors(&cfg, &axum::http::Method::POST, &h)
            .unwrap()
            .unwrap();
        assert_eq!(
            out.get("access-control-allow-origin").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(out.get(axum::http::header::VARY).unwrap(), "Origin");
    }

    #[test]
    fn a_disallowed_origin_is_rejected_before_forwarding() {
        let cfg = cors(&["https://app.example.com"], &["GET", "POST"]);
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://evil.com".parse().unwrap(),
        );
        let err = check_cors(&cfg, &axum::http::Method::POST, &h).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert_eq!(err.type_id, crate::gts::ERR_CORS_ORIGIN_NOT_ALLOWED);
    }

    #[test]
    fn a_disallowed_method_is_rejected_before_forwarding() {
        let cfg = cors(&["https://app.example.com"], &["GET"]);
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        let err = check_cors(&cfg, &axum::http::Method::DELETE, &h).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert_eq!(err.type_id, crate::gts::ERR_CORS_METHOD_NOT_ALLOWED);
    }

    fn upstream_with(cors: Option<crate::domain::dto::CorsConfig>) -> crate::domain::dto::Upstream {
        crate::domain::dto::Upstream {
            cors,
            ..crate::domain::dto::Upstream::default()
        }
    }

    #[test]
    fn a_method_the_policy_refuses_answers_403_not_405() {
        // ADR-0004: "disallowed methods are rejected with 403 on actual
        // requests" — even when the route's own method gate would have said
        // 405 first.
        let u = upstream_with(Some(cors(&["https://app.example.com"], &["GET", "POST"])));
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        let rejection = cross_origin_method_rejection(&u, &axum::http::Method::DELETE, &h);
        let err = rejection.expect("rejection");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert_eq!(err.type_id, crate::gts::ERR_CORS_METHOD_NOT_ALLOWED);
    }

    #[test]
    fn a_cross_origin_method_the_policy_admits_is_the_route_s_answer() {
        let u = upstream_with(Some(cors(&["https://app.example.com"], &["GET", "POST"])));
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        assert!(cross_origin_method_rejection(&u, &axum::http::Method::POST, &h).is_none());
    }

    #[test]
    fn the_method_gate_still_answers_a_same_origin_request() {
        // No `Origin`, no CORS policy: the route's 405 is the whole truth.
        let u = upstream_with(Some(cors(&["https://app.example.com"], &["GET"])));
        assert!(
            cross_origin_method_rejection(&u, &axum::http::Method::DELETE, &HeaderMap::new())
                .is_none()
        );

        let u = upstream_with(None);
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        assert!(cross_origin_method_rejection(&u, &axum::http::Method::DELETE, &h).is_none());
    }

    #[test]
    fn origin_matching_is_protocol_and_port_sensitive() {
        let cfg = cors(&["https://app.example.com"], &["GET"]);
        for origin in ["http://app.example.com", "https://app.example.com:8080"] {
            let mut h = HeaderMap::new();
            h.insert(axum::http::header::ORIGIN, origin.parse().unwrap());
            assert!(
                check_cors(&cfg, &axum::http::Method::GET, &h).is_err(),
                "{origin}"
            );
        }
    }

    #[test]
    fn the_wildcard_admits_every_origin() {
        let cfg = cors(&["*"], &["GET"]);
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://anything.example".parse().unwrap(),
        );
        assert!(check_cors(&cfg, &axum::http::Method::GET, &h).is_ok());
    }

    #[test]
    fn a_request_without_an_origin_is_not_cross_origin() {
        let cfg = cors(&["https://app.example.com"], &["GET"]);
        assert!(
            check_cors(&cfg, &axum::http::Method::GET, &HeaderMap::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cors_that_is_not_enabled_is_not_enforced() {
        let mut cfg = cors(&["https://app.example.com"], &["GET"]);
        cfg.enabled = false;
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            "https://evil.com".parse().unwrap(),
        );
        assert!(
            check_cors(&cfg, &axum::http::Method::GET, &h)
                .unwrap()
                .is_none()
        );
    }
}
