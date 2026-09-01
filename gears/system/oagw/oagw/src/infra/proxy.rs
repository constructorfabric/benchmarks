//! Data-plane proxy engine.
//!
//! Implements the DESIGN proxy flow:
//! 1. alias resolution (tenant chain, case-insensitive),
//! 2. route matching (method allowlist + longest path prefix, query
//!    allowlist, path-suffix guard, HTTP-protocol upstreams only),
//! 3. endpoint selection + `X-OAGW-Target-Host` matrix (ADR 0001),
//! 4. header transforms (set/add/remove/passthrough) and hop-by-hop
//!    stripping,
//! 5. body validation (Content-Length / Transfer-Encoding / 100 MiB hard
//!    limit → 400/413),
//! 6. forwarding over plain HTTP via the hyper-util legacy client,
//!    streaming SSE responses and bridging WebSocket upgrades,
//! 7. gateway error mapping (502/503/504) with `X-OAGW-Error-Source`
//!    on every response (ADR 0007).
//!
//! # DESIGN-led deviations
//!
//! - TLS to upstreams is not implemented in the MVP (no TLS client
//!   dependency); endpoints are reached over plain HTTP and `allow_http_upstream`
//!   fails closed by default. See the module doc of `crate::gear`.
//! - gRPC / WebTransport match paths are not implemented or reachable
//!   (DESIGN Phase 3); a gRPC upstream simply matches no routes.
//! - SSRF protection is a VPN-style feature; the MVP does not resolve the
//!   endpoint hostname, so when `ssrf_policy.enabled` the proxy fails closed
//!   with `502 UPSTREAM_UNSUPPORTED` (no DNS rebinding / private-IP checks).

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use dashmap::DashMap;
use http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, CONNECTION, CONTENT_LENGTH,
    HOST, HeaderMap, HeaderName, HeaderValue, ORIGIN, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE,
    TRAILER, TRANSFER_ENCODING, UPGRADE, VARY,
};

// `http` ≥ 1.0 no longer ships a `KEEP_ALIVE` constant (it was dropped with
// the HTTP/1.0 header fold); define it locally for hop-by-hop stripping.
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
use http::{Method, Request, StatusCode, Uri};
use hyper::body::Incoming;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::api::rest::error::{
    ERROR_SOURCE_GATEWAY, ERROR_SOURCE_UPSTREAM, HEADER_ERROR_SOURCE, OagwProblem, type_ids,
};
use crate::domain::models::{CorsConfig, Endpoint, HeaderOps, PathSuffixMode, Route, Upstream};
use crate::domain::plugin::{AuthContext, GuardContext, GuardError, GuardPhase, PluginError};
use crate::domain::service::{AliasResolution, ControlPlaneService};
use crate::domain::validation::{alias_is_valid_format, derive_alias, is_ip, normalize_alias};
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry};
use crate::infra::ratelimit::{RateLimitDecision, RateLimiter};
use toolkit_security::SecurityContext;

/// The routing header consumed by the data plane (ADR 0001).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Shared outbound HTTP client (plain HTTP, HTTP/1.1 upgrades enabled).
fn client() -> &'static Client<HttpConnector, Body> {
    static CLIENT: OnceLock<Client<HttpConnector, Body>> = OnceLock::new();
    CLIENT.get_or_init(|| Client::builder(TokioExecutor::new()).build(HttpConnector::new()))
}

/// Per-upstream round-robin counters.
fn rr_counters() -> &'static Arc<DashMap<Uuid, u64>> {
    static RR: OnceLock<Arc<DashMap<Uuid, u64>>> = OnceLock::new();
    RR.get_or_init(|| Arc::new(DashMap::new()))
}

/// The built-in guard-plugin registry (stateless; `required_headers.v1` per
/// ADR 0009). A process global rather than an injected dependency because it
/// carries no state.
fn guard_registry() -> &'static GuardPluginRegistry {
    static GUARDS: OnceLock<GuardPluginRegistry> = OnceLock::new();
    GUARDS.get_or_init(GuardPluginRegistry::with_builtins)
}

/// Hop-by-hop headers always stripped (unless an upgrade is in flight).
const HOP_BY_HOP: [HeaderName; 9] = [
    CONNECTION,
    KEEP_ALIVE,
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
    HOST,
];

/// The main proxy entry point, called by the REST handler.
///
/// `chain` is the tenant chain (descendant → root) used for alias
/// resolution. `rest` is the decoded path suffix after the alias plus any
/// raw query string (`path?query`), used for route matching and passthrough.
/// `client_upgrade` is the downstream `OnUpgrade` when the caller asked for a
/// protocol upgrade (WebSocket).
#[allow(clippy::too_many_arguments)]
pub async fn proxy_request(
    service: &Arc<ControlPlaneService>,
    chain: Vec<Uuid>,
    alias: String,
    rest: String,
    method: Method,
    inbound_headers: HeaderMap,
    body: Body,
    client_upgrade: Option<hyper::upgrade::OnUpgrade>,
    security_ctx: Option<&SecurityContext>,
    auth: Option<&AuthPluginRegistry>,
    rate: Option<&RateLimiter>,
) -> Response {
    let cfg = service.config();

    // 0. CORS preflight fast path (ADR 0004): an OPTIONS request carrying
    // `Origin` + `Access-Control-Request-Method` is answered locally with a
    // permissive 204 that echoes the requested origin/method/headers — no
    // upstream resolution, no tenant context, no auth. Origin/method
    // enforcement is deferred to the actual (non-preflight) request.
    if method == Method::OPTIONS && is_cors_preflight(&inbound_headers) {
        return cors_preflight_response(&inbound_headers);
    }

    // 1. Alias resolution (case-insensitive, descendant→root). A disabled
    // upstream in the owning tenant short-circuits the chain: proxies MUST
    // reject with 503 (PRD cpt-cf-oagw-fr-enable-disable), never fall through
    // to an ancestor's enabled copy.
    let upstream = match service.resolve_upstream_in_chain(&chain, &alias) {
        AliasResolution::Found(u) => *u,
        AliasResolution::NotFound => {
            return gateway(
                OagwProblem::new(
                    type_ids::ROUTE_NOT_FOUND,
                    "Route Not Found",
                    StatusCode::NOT_FOUND,
                )
                .detail(format!("no route matches alias '{alias}'"))
                .alias(alias),
            );
        }
        AliasResolution::Disabled => {
            return gateway(
                OagwProblem::new(
                    type_ids::LINK_UNAVAILABLE,
                    "Upstream Unavailable",
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                .detail(format!("upstream alias '{alias}' is disabled"))
                .alias(alias),
            );
        }
    };

    let proxy_ctx = ProxyContext {
        service,
        cfg,
        upstream,
        alias,
    };

    // 2+3. Route match + endpoint selection (+ target-host matrix).
    let route = match proxy_ctx.match_route(&chain, &method, &rest) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };

    // 2c. CORS enforcement on actual requests (ADR 0004): after upstream/
    // route resolution, before body buffering/auth/forwarding. A cross-origin
    // request (Origin present) must match the effective config's allowed
    // origins + methods; disallowed → 403 (`origin_not_allowed` /
    // `method_not_allowed`). Allowed requests carry CORS response headers on
    // the forwarded response.
    let cors = effective_cors(&proxy_ctx.upstream, &route.route);
    let cors_response: Option<(CorsConfig, String)> = match cors
        .as_ref()
        .map(|cfg| cors_prepare(cfg, &inbound_headers, &method))
    {
        Some(Ok(ok)) => ok,
        Some(Err(e)) => return e.into_response(),
        None => None,
    };

    // 5. Body validation + buffering.
    let body_bytes = match validate_and_buffer(&inbound_headers, body, cfg.body_limit_bytes).await {
        Ok(b) => b,
        Err(e) => return e,
    };

    // 4a. Auth plugins (credential injection, DESIGN execution order:
    // auth → rate limit → guards → transform). Executed after
    // matching/body validation, before the outbound request is built.
    let (auth_headers, auth_query) =
        match run_auth(security_ctx, auth, &proxy_ctx.upstream, &method).await {
            Ok(ok) => ok,
            Err(e) => return e,
        };

    // 4b. Rate limiting (ADR 0006 flow: resolve → auth → rate limit →
    // guards). DP-owned per-instance token buckets; effective limit is
    // `min(upstream, route)` — stricter always wins.
    if let Some(rate) = rate
        && let Some(plan) = RateLimiter::plan_for(
            &proxy_ctx.upstream,
            &route.route,
            security_ctx,
            &inbound_headers,
        )
    {
        let decision = rate.try_acquire(&plan);
        if !decision.allowed {
            warn!(
                alias = %proxy_ctx.alias,
                limit = decision.limit,
                retry_after_secs = decision.reset_after_secs,
                "proxy request rate-limited"
            );
            return rate_limit_response(&decision, &proxy_ctx.alias);
        }
    }

    // 4c. Request-phase guard plugins (ADR 0009), before the request is
    // built/forwarded.
    if let Err(e) = run_request_guards(security_ctx, &inbound_headers, &proxy_ctx.upstream) {
        return e.into_response();
    }

    // 4. Build the outbound request (headers + URI).
    let outbound = match proxy_ctx.build_request(
        &method,
        &rest,
        &route,
        &inbound_headers,
        body_bytes,
        &auth_headers,
        auth_query,
    ) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };

    // 6. Forward (streaming response incl. SSE; WebSocket upgrade bridged),
    // then attach CORS response headers to allowed cross-origin responses
    // (ADR 0004: `Access-Control-Allow-Origin` + `Vary: Origin`, plus
    // configured expose/credentials headers).
    let mut response = proxy_ctx
        .forward(outbound, client_upgrade, security_ctx)
        .await;
    if let Some((cfg, origin)) = &cors_response {
        add_cors_response_headers(&mut response, cfg, origin);
    }
    response
}

/// Everything the proxy needs after alias resolution.
struct ProxyContext<'a> {
    service: &'a Arc<ControlPlaneService>,
    cfg: &'a crate::config::OagwConfig,
    upstream: Upstream,
    alias: String,
}

/// The outcome of route matching: the matched route plus the effective
/// outbound path.
struct MatchedRoute {
    route: Route,
    outbound_path: String,
}

impl ProxyContext<'_> {
    /// Route-match against the upstream's routes found across the tenant
    /// chain, plus the target-host matrix.
    fn match_route(
        &self,
        chain: &[Uuid],
        method: &Method,
        rest: &str,
    ) -> Result<MatchedRoute, Box<Response>> {
        let routes = self
            .service
            .list_routes_for_upstream(chain, self.upstream.id);

        // Suffix path after the alias, normalized to a leading '/'.
        let suffix = if rest.is_empty() {
            "/".to_owned()
        } else if rest.starts_with('/') {
            rest.to_owned()
        } else {
            format!("/{rest}")
        };

        // Query params (parsed for the query-allowlist guard).
        let query_params: Vec<(String, String)> = extract_query_params(&suffix);
        let suffix_path = match suffix.split('?').next() {
            Some(p) => p.to_owned(),
            None => suffix.clone(),
        };

        // Longest path-prefix match among HTTP-method routes. Disabled routes
        // are excluded from matching (PRD cpt-cf-oagw-fr-enable-disable).
        let mut best: Option<(&Route, String)> = None;
        for r in &routes {
            if !r.enabled {
                continue;
            }
            let Some(m) = r.http_match() else { continue };
            if !m
                .methods
                .iter()
                .any(|x| x.eq_ignore_ascii_case(method.as_str()))
            {
                continue;
            }
            let prefix = normalize_route_path(&m.path);
            if suffix_path.starts_with(&prefix)
                && best
                    .as_ref()
                    .map_or(0, |(br, _)| br.http_match().map_or(0, |h| h.path.len()))
                    < m.path.len()
            {
                best = Some((r, prefix.clone()));
            }
        }
        let Some((route, prefix)) = best else {
            return Err(Box::new(gateway(
                OagwProblem::new(
                    type_ids::ROUTE_NOT_FOUND,
                    "Route Not Found",
                    StatusCode::NOT_FOUND,
                )
                .detail(format!(
                    "no route matches {} '{}' for alias '{}'",
                    method, suffix_path, self.alias
                ))
                .alias(self.alias.clone()),
            )));
        };
        // `route` borrows from `routes` (a Vec owned here) — clone out.
        let route = route.clone();

        let Some(m) = route.http_match() else {
            return Err(Box::new(gateway(
                OagwProblem::new(
                    type_ids::ROUTE_NOT_FOUND,
                    "Route Not Found",
                    StatusCode::NOT_FOUND,
                )
                .detail(format!(
                    "no route matches {} '{}' for alias '{}'",
                    method, suffix_path, self.alias
                ))
                .alias(self.alias.clone()),
            )));
        };
        let outbound_path = suffix_path.clone();
        // There is a path suffix beyond the matched prefix.
        if suffix_path.len() > prefix.len() && m.path_suffix_mode == PathSuffixMode::Disabled {
            return Err(Box::new(gateway(
                OagwProblem::validation(format!(
                    "path suffix '{}' is not allowed for route '{}' (path_suffix_mode: disabled)",
                    &suffix_path[prefix.len()..],
                    m.path
                ))
                .path(suffix_path),
            )));
        }
        // Append mode: the full suffix path (route.path + remainder) is
        // forwarded as-is.

        // Query allowlist guard.
        if !m.query_allowlist.is_empty() {
            let allowed: Vec<&str> = m.query_allowlist.iter().map(String::as_str).collect();
            if let Some(bad) = query_params
                .iter()
                .find(|(k, _)| !allowed.contains(&k.as_str()))
            {
                return Err(Box::new(gateway(
                    OagwProblem::validation(format!(
                        "query parameter '{}' is not allowed by this route",
                        bad.0
                    ))
                    .path(suffix_path),
                )));
            }
        }

        Ok(MatchedRoute {
            route,
            outbound_path,
        })
    }

    /// Resolve the concrete endpoint per the `X-OAGW-Target-Host` matrix
    /// (ADR 0001).
    fn select_endpoint(&self, headers: &HeaderMap) -> Result<Endpoint, Box<Response>> {
        let endpoints = &self.upstream.server.endpoints;
        let target = headers
            .get(TARGET_HOST_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(t) = target {
            if !valid_target_host(t) {
                return Err(Box::new(gateway(
                    OagwProblem::new(
                        type_ids::INVALID_TARGET_HOST,
                        "Invalid Target Host",
                        StatusCode::BAD_REQUEST,
                    )
                    .detail(format!(
                        "X-OAGW-Target-Host '{t}' is invalid (must be a hostname or IP, no port/path)"
                    ))
                    .invalid_value(t)
                    .host(join_endpoint_hosts(endpoints)),
                )));
            }
            let tn = normalize_alias(t);
            if let Some(e) = endpoints.iter().find(|e| normalize_alias(&e.host) == tn) {
                return Ok(e.clone());
            }
            return Err(Box::new(gateway(
                OagwProblem::new(
                    type_ids::UNKNOWN_TARGET_HOST,
                    "Unknown Target Host",
                    StatusCode::BAD_REQUEST,
                )
                .detail(format!(
                    "X-OAGW-Target-Host '{t}' does not match any configured endpoint"
                ))
                .invalid_value(t)
                .valid_hosts(endpoints.iter().map(|e| e.host.clone()).collect()),
            )));
        }

        if endpoints.len() == 1 {
            return Ok(endpoints[0].clone());
        }
        // Multi-endpoint pool: an alias that is a common derived suffix
        // mandates the routing header; an explicit multi-endpoint alias
        // round-robins.
        let is_common_suffix = matches!(
            derive_alias(&self.upstream),
            Ok(Some(d)) if normalize_alias(&d) == normalize_alias(&self.upstream.alias)
        );
        if is_common_suffix {
            return Err(Box::new(gateway(
                OagwProblem::new(
                    type_ids::MISSING_TARGET_HOST,
                    "Missing Target Host",
                    StatusCode::BAD_REQUEST,
                )
                .detail(
                    "X-OAGW-Target-Host is required for a multi-endpoint upstream with a common suffix alias",
                )
                .alias(self.alias.clone())
                .valid_hosts(endpoints.iter().map(|e| e.host.clone()).collect()),
            )));
        }
        // Explicit multi-endpoint alias: round-robin.
        let n = rr_counters()
            .entry(self.upstream.id)
            .and_modify(|c| *c = c.wrapping_add(1))
            .or_insert(0);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the round-robin counter is taken modulo the endpoint count first, so the value is bounded well below usize::MAX"
        )]
        let idx = (*n % endpoints.len() as u64) as usize;
        Ok(endpoints[idx].clone())
    }

    /// Build the outbound HTTP/1.1 request.
    #[allow(
        clippy::too_many_arguments,
        clippy::cognitive_complexity,
        reason = "build_request's parameter list mirrors the full proxy-flow inputs (method, path, matched route, inbound headers, buffered body, auth-injected headers/query); the complexity comes from the linear, well-commented header-transform pipeline branches"
    )]
    fn build_request(
        &self,
        method: &Method,
        rest: &str,
        matched: &MatchedRoute,
        inbound_headers: &HeaderMap,
        body: Bytes,
        auth_headers: &HeaderMap,
        auth_query: Vec<(String, String)>,
    ) -> Result<Request<Body>, Box<Response>> {
        let cfg = self.cfg;
        if !cfg.allow_http_upstream {
            return Err(Box::new(gateway(
                OagwProblem::new(
                    type_ids::UPSTREAM_UNSUPPORTED,
                    "Upstream Unsupported",
                    StatusCode::BAD_GATEWAY,
                )
                .detail("plain-HTTP upstream relay is disabled by configuration (allow_http_upstream: false)"),
            )));
        }
        if cfg.ssrf_policy.enabled {
            return Err(Box::new(gateway(
                OagwProblem::new(
                    type_ids::UPSTREAM_UNSUPPORTED,
                    "Upstream Unsupported",
                    StatusCode::BAD_GATEWAY,
                )
                .detail(
                    "SSRF protection is enabled; upstream relay is not permitted in this build",
                ),
            )));
        }

        let endpoint = self.select_endpoint(inbound_headers)?;
        debug!(
            route_id = %matched.route.id,
            outbound_path = %matched.outbound_path,
            endpoint = %format!("{}:{}", endpoint.host, endpoint.port),
            "resolved route -> endpoint"
        );

        // Query string passthrough + auth-injected query params (URL-encoded).
        let query = rest.split_once('?').map_or("", |(_, q)| q);
        let path = &matched.outbound_path;
        let mut query_parts: Vec<String> = Vec::new();
        if !query.is_empty() {
            query_parts.push(query.to_owned());
        }
        for (k, v) in auth_query {
            let ek: String = form_urlencoded::byte_serialize(k.as_bytes()).collect();
            let ev: String = form_urlencoded::byte_serialize(v.as_bytes()).collect();
            query_parts.push(format!("{ek}={ev}"));
        }
        let joined_query = query_parts.join("&");
        // One bracketing decision shared by the request-line URI authority and
        // the outbound `Host` header (Rr-003): an IPv6 endpoint literal like
        // `2001:db8::1` must be bracketed in both, never emitted as the
        // ambiguous `2001:db8::1:8080`.
        let authority = authority_for(&endpoint.host, endpoint.port);
        let out_uri = build_outbound_uri(&authority, path, &joined_query)?;

        let is_upgrade = inbound_headers
            .get(UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.is_empty());

        // Assemble the outbound headers in one deterministic pass (DESIGN
        // "Headers Transformation"): inbound passthrough appends, `set` rules
        // replace any earlier value, `add` rules append, and auth-injected
        // credentials use set semantics so exactly one `Authorization` reaches
        // the upstream (a caller Authorization that slipped through an
        // `All`/allowlist passthrough is replaced by the injected one).
        let uc = &self.upstream.headers.request;
        let mut headers = HeaderMap::new();
        // The single Content-Length comes from the buffered body (always set
        // after full buffering); the inbound value is never forwarded.
        headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
        let host_value = HeaderValue::from_str(&authority).map_err(|e| {
                gateway(OagwProblem::validation(format!(
                    "upstream endpoint host is not a valid header value: {e}"
                )))
            })?;
        headers.insert(HOST, host_value);

        // Inbound passthrough (hop-by-hop/protected + content-length skipped;
        // WebSocket handshake headers are relayed by the upgrade block so a
        // passthrough policy cannot drop them — RFC 6455 end-to-end).
        for (name, value) in inbound_headers {
            if is_protected_header(name) || name == CONTENT_LENGTH {
                continue;
            }
            if is_upgrade && is_websocket_handshake_header(name) {
                continue;
            }
            match uc.passthrough {
                crate::domain::models::PassthroughMode::None => continue,
                crate::domain::models::PassthroughMode::Allowlist => {
                    if !uc
                        .passthrough_allowlist
                        .iter()
                        .any(|a| a.as_str() == name.as_str())
                    {
                        continue;
                    }
                }
                crate::domain::models::PassthroughMode::All => {}
            }
            if uc.remove.iter().any(|r| r.as_str() == name.as_str()) {
                continue;
            }
            headers.append(name, value.clone());
        }
        // `set` rules replace any earlier emitted value for the name.
        for (name, value) in valid_rule_pairs(&uc.set) {
            headers.insert(name, value);
        }
        // `add` rules append after the set pass.
        for (name, value) in valid_rule_pairs(&uc.add) {
            headers.append(name, value);
        }
        // Re-attach the upgrade headers for WebSocket relay (not
        // passthrough-managed): Upgrade/Connection plus every inbound
        // `Sec-WebSocket-*` handshake header (RFC 6455 end-to-end).
        if is_upgrade {
            if let Some(v) = inbound_headers.get(UPGRADE) {
                headers.append(UPGRADE, v.clone());
            }
            if let Some(v) = inbound_headers.get(CONNECTION) {
                headers.append(CONNECTION, v.clone());
            }
            for (name, value) in inbound_headers {
                if is_websocket_handshake_header(name) {
                    // Append, never replace: a client that sends repeated
                    // `Sec-WebSocket-Protocol` / `Sec-WebSocket-Extensions`
                    // values (RFC 6455 allows multi-valued handshake headers)
                    // must have ALL of them relayed end-to-end, matching the
                    // passthrough ethos. Single-value `Key`/`Version`
                    // behavior is unchanged (one value appended after any
                    // transform-emitted value, as before).
                    headers.append(name, value.clone());
                }
            }
        }

        // Auth-injected credentials (set semantics) win over the transform
        // pipeline — a header `remove` rule must not strip credentials, and a
        // caller-supplied `authorization` must not leak alongside.
        for (name, value) in auth_headers {
            headers.insert(name, value.clone());
        }

        let mut req = Request::builder()
            .method(method.clone())
            .uri(out_uri)
            .body(Body::from(body))
            .map_err(|e| {
                gateway(OagwProblem::downstream(format!(
                    "cannot build upstream request: {e}"
                )))
            })?;
        *req.headers_mut() = headers;
        Ok(req)
    }

    /// Send the request and map the response.
    async fn forward(
        &self,
        request: Request<Body>,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
        security_ctx: Option<&SecurityContext>,
    ) -> Response {
        let timeout = Duration::from_secs(self.cfg.proxy_timeout_secs.max(1));
        let result = tokio::time::timeout(timeout, client().request(request)).await;

        let resp = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                // Walk the error chain: hyper surfaces connect failures as
                // `client error (Connect)` with the real cause (e.g. `Connection
                // refused`) as the source, so classification needs the full chain
                // (logged) while the client detail gets the single root cause.
                let full = error_chain_string(&e);
                let brief = root_cause_string(&e);
                error!(
                    alias = %self.upstream.alias,
                    host = %self.upstream_host(),
                    error = %full,
                    "upstream request failed",
                );
                return map_send_error(&full, &brief, false);
            }
            Err(_) => {
                error!(
                    alias = %self.upstream.alias,
                    host = %self.upstream_host(),
                    timeout_secs = self.cfg.proxy_timeout_secs,
                    "upstream request timed out",
                );
                return gateway(
                    OagwProblem::new(
                        type_ids::REQUEST_TIMEOUT,
                        "Request Timeout",
                        StatusCode::GATEWAY_TIMEOUT,
                    )
                    .detail(format!("upstream request timed out after {timeout:?}"))
                    .host(self.upstream_host()),
                );
            }
        };

        // WebSocket / protocol upgrade: bridge both sides.
        if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
            return self.handle_upgrade(client_upgrade, resp).await;
        }

        // Stream the upstream body through (SSE etc.).
        let (mut parts, incoming) = resp.into_parts();
        apply_header_ops_set_add_remove(&mut parts.headers, &self.upstream.headers.response);
        let mut response = Response::builder()
            .status(parts.status)
            .body(Body::new(incoming))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        for (name, value) in &parts.headers {
            if is_protected_header(name) {
                continue;
            }
            response.headers_mut().append(name.clone(), value.clone());
        }
        response.headers_mut().insert(
            HEADER_ERROR_SOURCE,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );

        // Response-phase guard plugins (ADR 0009) with set semantics over the
        // upstream-derived headers, before the response is returned.
        if let Err(e) = run_response_guards(security_ctx, &self.upstream, response.headers()) {
            return e.into_response();
        }
        response
    }

    fn upstream_host(&self) -> String {
        self.upstream
            .server
            .endpoints
            .first()
            .map(|e| e.host.clone())
            .unwrap_or_default()
    }

    /// Bridge a 101 response: return it to the client and relay bytes
    /// between the client- and upstream-side upgraded connections.
    async fn handle_upgrade(
        &self,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
        mut resp: http::Response<Incoming>,
    ) -> Response {
        // No further access to `self` beyond this point; `self` is only a
        // borrowed context (service/config/upstream), kept for the 101
        // construction below.
        let upstream_upgraded = match hyper::upgrade::on(&mut resp).await {
            Ok(u) => u,
            Err(e) => {
                return gateway(OagwProblem::downstream(format!(
                    "upstream upgrade failed: {e}"
                )));
            }
        };
        let mut out = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        for (name, value) in resp.headers() {
            if is_protected_header(name) {
                // Keep the protocol-upgrade headers on a 101 so the client sees
                // a well-formed upgrade response (the standard hop-by-hop
                // stripping rule excludes the in-flight upgrade case).
                if name == CONNECTION || name == UPGRADE {
                    out.headers_mut().append(name.clone(), value.clone());
                }
                continue;
            }
            out.headers_mut().append(name.clone(), value.clone());
        }
        out.headers_mut().insert(
            HEADER_ERROR_SOURCE,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );

        // Bridge the client- and upstream-side upgraded connections. The
        // client-side `OnUpgrade` resolves once this 101 response is written
        // by our server.
        let Some(client_upgrade) = client_upgrade else {
            // No downstream upgrade was requested (e.g. test harness without a
            // hyper server); the upstream side is consumed and dropped.
            return out;
        };
        tokio::spawn(async move {
            let client_upgraded = match client_upgrade.await {
                Ok(c) => c,
                Err(e) => {
                    warn!(err = %e, "downstream connection upgrade failed");
                    return;
                }
            };
            let mut a = HyperUpgradedIo::new(client_upgraded);
            let mut b = HyperUpgradedIo::new(upstream_upgraded);
            if let Err(e) = tokio::io::copy_bidirectional(&mut a, &mut b).await {
                debug!(err = %e, "websocket relay closed with error");
            }
        });
        out
    }
}

// ---------------------------------------------------------------------------
// Auth plugins
// ---------------------------------------------------------------------------

/// Execute the upstream's bound auth plugin (credential injection) and return
/// the collected outbound headers + query params.
///
/// Error mapping follows the DESIGN error table via [`PluginError`]:
///
/// | `PluginError` | HTTP | GTS type |
/// |---|---|---|
/// | `SecretNotFound` | 500 | `secret.not_found.v1` |
/// | `UnknownPlugin` | 503 | `plugin.not_found.v1` |
/// | `AuthenticationFailed` | 401 | `auth.failed.v1` |
/// | `Internal` | 503 | `link.unavailable.v1` |
async fn run_auth(
    security_ctx: Option<&SecurityContext>,
    auth: Option<&AuthPluginRegistry>,
    upstream: &Upstream,
    _method: &Method,
) -> Result<(HeaderMap, Vec<(String, String)>), Response> {
    if let (Some(ctx), Some(auth_cfg)) = (security_ctx, &upstream.auth) {
        let mut headers = HeaderMap::new();
        let mut query_params: Vec<(String, String)> = Vec::new();
        {
            let mut actx = AuthContext {
                security_context: ctx,
                config: &auth_cfg.config,
                headers: &mut headers,
                query_params: &mut query_params,
            };
            execute_auth_plugin(auth, &auth_cfg.r#type, &mut actx).await?;
        }
        Ok((headers, query_params))
    } else {
        Ok((HeaderMap::new(), Vec::new()))
    }
}

/// Resolve the plugin by GTS id and run it, mapping `PluginError`s to gateway
/// problems.
async fn execute_auth_plugin(
    registry: Option<&AuthPluginRegistry>,
    plugin_id: &str,
    actx: &mut AuthContext<'_>,
) -> Result<(), Response> {
    let Some(registry) = registry else {
        return Err(gateway(
            OagwProblem::new(
                type_ids::PLUGIN_NOT_FOUND,
                "Plugin Not Found",
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .detail(format!(
                "auth plugin '{plugin_id}' is not available (no auth plugin registry)"
            )),
        ));
    };
    let Some(plugin) = registry.resolve(plugin_id) else {
        return Err(gateway(
            OagwProblem::new(
                type_ids::PLUGIN_NOT_FOUND,
                "Plugin Not Found",
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .detail(format!("unknown auth plugin '{plugin_id}'")),
        ));
    };
    match plugin.authenticate(actx).await {
        Ok(()) => Ok(()),
        Err(PluginError::SecretNotFound(detail)) => Err(gateway(
            OagwProblem::new(
                type_ids::SECRET_NOT_FOUND,
                "Secret Not Found",
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .detail(detail),
        )),
        Err(PluginError::UnknownPlugin(detail)) => Err(gateway(
            OagwProblem::new(
                type_ids::PLUGIN_NOT_FOUND,
                "Plugin Not Found",
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .detail(detail),
        )),
        Err(PluginError::AuthenticationFailed(detail)) => Err(gateway(
            OagwProblem::new(
                type_ids::AUTH_FAILED,
                "Authentication Failed",
                StatusCode::UNAUTHORIZED,
            )
            .detail(detail),
        )),
        Err(PluginError::Internal(detail)) => Err(gateway(
            OagwProblem::new(
                type_ids::LINK_UNAVAILABLE,
                "Link Unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .detail(detail),
        )),
    }
}

// ---------------------------------------------------------------------------
// Guard plugins (ADR 0009)
// ---------------------------------------------------------------------------

/// Execute the request-phase guard plugins bound on the upstream
/// (`plugins.items`) against the inbound request headers.
///
/// Execution is fail-open per ADR 0009: plugins bound by GTS ID with no
/// matching builtin (custom UUIDs, catalog-only `timeout`/`cors`) are
/// skipped, and the MVP model's `plugins.items` carries no per-plugin config
/// object (the schema defines string IDs), so a bound `required_headers.v1`
/// runs with empty config — which is a no-op. The config-driven rejection
/// paths (400 request / 502 response) are exercised by the plugin unit tests.
fn run_request_guards(
    security_ctx: Option<&SecurityContext>,
    inbound_headers: &HeaderMap,
    upstream: &Upstream,
) -> Result<(), Box<Response>> {
    let registry = guard_registry();
    for id in &upstream.plugins.items {
        let Some(plugin) = registry.resolve(id) else {
            warn!(plugin_id = %id, "guard plugin not resolvable; failing open (skipping)");
            continue;
        };
        let ctx = GuardContext {
            security_context: security_ctx,
            config: &serde_json::Value::Null,
            headers: inbound_headers,
        };
        if let Err(e) = plugin.guard_request(&ctx) {
            warn!(plugin_id = %id, error = ?e, "request guard rejected the request");
            return Err(Box::new(map_guard_error(e)));
        }
    }
    Ok(())
}

/// Execute the response-phase guard plugins bound on the upstream against the
/// upstream-derived response headers (ADR 0009), before it is returned.
fn run_response_guards(
    security_ctx: Option<&SecurityContext>,
    upstream: &Upstream,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    let registry = guard_registry();
    for id in &upstream.plugins.items {
        let Some(plugin) = registry.resolve(id) else {
            warn!(plugin_id = %id, "response guard plugin not resolvable; failing open (skipping)");
            continue;
        };
        let ctx = GuardContext {
            security_context: security_ctx,
            config: &serde_json::Value::Null,
            headers,
        };
        if let Err(e) = plugin.guard_response(&ctx) {
            warn!(plugin_id = %id, error = ?e, "response guard rejected the response");
            return Err(Box::new(map_guard_error(e)));
        }
    }
    Ok(())
}

/// Map a [`GuardError`] to the DESIGN error table: request phase → 400,
/// response phase → 502, both `required_header.missing.v1` (ADR 0009).
fn map_guard_error(e: GuardError) -> Response {
    match e {
        GuardError::RequiredHeaderMissing { phase, header } => match phase {
            GuardPhase::Request => gateway(
                OagwProblem::new(
                    type_ids::REQUIRED_HEADER_MISSING,
                    "Required Header Missing",
                    StatusCode::BAD_REQUEST,
                )
                .detail(format!("missing required request header '{header}'"))
                .missing_headers(vec![header]),
            ),
            GuardPhase::Response => gateway(
                OagwProblem::new(
                    type_ids::REQUIRED_HEADER_MISSING,
                    "Required Header Missing",
                    StatusCode::BAD_GATEWAY,
                )
                .detail(format!("missing required response header '{header}'"))
                .missing_headers(vec![header]),
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// CORS (ADR 0004)
// ---------------------------------------------------------------------------

/// Detect a CORS preflight: `OPTIONS` + `Origin` + `Access-Control-Request-Method`.
fn is_cors_preflight(headers: &HeaderMap) -> bool {
    headers.contains_key(ORIGIN)
        && headers
            .get(ACCESS_CONTROL_REQUEST_METHOD)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.trim().is_empty())
}

/// Answer a preflight locally with a permissive 204 (ADR 0004 "Preflight
/// Request Handling"): echo the requested origin/method/headers and advertise
/// a one-day max-age, always with `Vary` so caches never serve a cross-client
/// CORS decision.
fn cors_preflight_response(inbound: &HeaderMap) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::NO_CONTENT.into_response());
    let origin = inbound
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");
    let requested_method = inbound
        .get(ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");
    response.headers_mut().insert(
        ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(origin).unwrap_or(HeaderValue::from_static("*")),
    );
    response.headers_mut().insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_str(requested_method).unwrap_or(HeaderValue::from_static("*")),
    );
    if let Some(v) = inbound.get(ACCESS_CONTROL_REQUEST_HEADERS) {
        response
            .headers_mut()
            .insert(ACCESS_CONTROL_ALLOW_HEADERS, v.clone());
    }
    response
        .headers_mut()
        .insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
    response.headers_mut().insert(
        VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    response.headers_mut().insert(
        HEADER_ERROR_SOURCE,
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// The effective CORS config for one proxied request.
///
/// # DESIGN-led deviation
///
/// Route-level CORS fully overrides upstream-level CORS in the MVP;
/// hierarchical union (`inherit`) / enforcement (`enforce`) across the tenant
/// hierarchy is out of scope (the models retain `sharing` for API
/// compatibility).
fn effective_cors(upstream: &Upstream, route: &Route) -> Option<CorsConfig> {
    route.cors.clone().or_else(|| upstream.cors.clone())
}

/// Validate an actual (non-preflight) cross-origin request against the
/// effective CORS config. Returns `None` when CORS is disabled or the request
/// carries no `Origin` (non-browser client — not applicable), and the concrete
/// `(config, origin)` pair to echo on the response when allowed.
fn cors_prepare(
    cfg: &CorsConfig,
    headers: &HeaderMap,
    method: &Method,
) -> Result<Option<(CorsConfig, String)>, Box<Response>> {
    if !cfg.enabled {
        return Ok(None);
    }
    let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(None); // no Origin → not a cross-origin request
    };
    let origin = origin.trim();

    // Origin must match exactly (protocol+host+port sensitive) or be `*`.
    let allowed = cfg.allowed_origins.iter().any(|o| o == "*" || o == origin);
    if !allowed {
        return Err(Box::new(gateway(
            OagwProblem::new(
                type_ids::CORS_ORIGIN_NOT_ALLOWED,
                "CORS Origin Not Allowed",
                StatusCode::FORBIDDEN,
            )
            .detail(format!("Origin '{origin}' not in allowed origins list"))
            .invalid_value(origin),
        )));
    }
    // Method must be in `allowed_methods` (designated cross-origin methods).
    if !cfg
        .allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(Box::new(gateway(
            OagwProblem::new(
                type_ids::CORS_METHOD_NOT_ALLOWED,
                "CORS Method Not Allowed",
                StatusCode::FORBIDDEN,
            )
            .detail(format!("Method '{method}' not in allowed methods list"))
            .invalid_value(method.as_str()),
        )));
    }
    Ok(Some((cfg.clone(), origin.to_owned())))
}

/// Add CORS response headers to an allowed cross-origin response (ADR 0004
/// "Actual Request Handling"), always with `Vary: Origin`.
fn add_cors_response_headers(response: &mut Response, cfg: &CorsConfig, origin: &str) {
    response.headers_mut().insert(
        ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(origin).unwrap_or(HeaderValue::from_static("*")),
    );
    if !cfg.expose_headers.is_empty() {
        response.headers_mut().insert(
            ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_str(&cfg.expose_headers.join(", "))
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        );
    }
    if cfg.allow_credentials {
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    // Append to any existing Vary (e.g. upstream-provided).
    let vary = response
        .headers_mut()
        .get(VARY)
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| "Origin".to_owned(), |s| format!("{s}, Origin"));
    response.headers_mut().insert(
        VARY,
        HeaderValue::from_str(&vary).unwrap_or(HeaderValue::from_static("Origin")),
    );
}

// ---------------------------------------------------------------------------
// Rate-limit response (ADR 0003 response headers)
// ---------------------------------------------------------------------------

/// Epoch seconds now (for `X-RateLimit-Reset`).
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Build the 429 response: RFC 6585 / draft-ietf-httpapi-ratelimit-headers
/// (`Retry-After`, `X-RateLimit-*`) plus the DESIGN problem body.
fn rate_limit_response(decision: &RateLimitDecision, alias: &str) -> Response {
    let mut response = OagwProblem::new(
        type_ids::RATE_LIMIT_EXCEEDED,
        "Rate Limit Exceeded",
        StatusCode::TOO_MANY_REQUESTS,
    )
    .detail(format!(
        "rate limit exceeded (limit {}, retry after {}s)",
        decision.limit, decision.reset_after_secs
    ))
    .retry_after_seconds(decision.reset_after_secs)
    .alias(alias)
    .into_response();

    response.headers_mut().insert(
        http::header::RETRY_AFTER,
        HeaderValue::from(decision.reset_after_secs.max(1)),
    );
    response
        .headers_mut()
        .insert("x-ratelimit-limit", HeaderValue::from(decision.limit));
    response.headers_mut().insert(
        "x-ratelimit-remaining",
        HeaderValue::from(decision.remaining),
    );
    response.headers_mut().insert(
        "x-ratelimit-reset",
        HeaderValue::from(now_epoch_secs() + decision.reset_after_secs),
    );
    response
}

// ---------------------------------------------------------------------------
// Gateway error helpers
// ---------------------------------------------------------------------------

/// Render an OAGW problem as a gateway error response.
fn gateway(problem: OagwProblem) -> Response {
    problem.into_response()
}

/// A 413 problem for the configurable body limit.
fn payload_too_large(limit: usize) -> OagwProblem {
    OagwProblem::new(
        type_ids::PAYLOAD_TOO_LARGE,
        "Payload Too Large",
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .detail(format!("request body exceeds the {limit}-byte limit"))
}

/// Join an error and its whole source chain into one message so classification
/// sees the root cause (hyper 1 wraps connect/io failures in a generic
/// `client error (Connect)` whose `source()` holds the real error). The full
/// chain is for gateway logging, NOT for the client: RFC 9457 details must not
/// leak long nested chains or raw OS error noise (Rf-015).
fn error_chain_string<E: std::error::Error>(e: &E) -> String {
    let mut out = e.to_string();
    let mut next: Option<&(dyn std::error::Error + 'static)> = e.source();
    while let Some(s) = next {
        out.push_str(": ");
        out.push_str(&s.to_string());
        next = s.source();
    }
    out
}

/// The deepest cause of an error (or the error itself when it has no source);
/// a short, single-level description safe to surface in a problem detail.
fn root_cause_string<E: std::error::Error + 'static>(e: &E) -> String {
    let mut deepest: &(dyn std::error::Error + 'static) = e;
    while let Some(s) = deepest.source() {
        deepest = s;
    }
    deepest.to_string()
}

/// Map a hyper client send error to a gateway response (DESIGN error table).
/// `full` is the whole error chain used for classification; `brief` is the
/// single root cause surfaced in the RFC 9457 detail (no nested chains).
fn map_send_error(full: &str, brief: &str, connect_timeout: bool) -> Response {
    let m = full.to_ascii_lowercase();
    let problem = if connect_timeout || m.contains("timed out") || m.contains("timeout") {
        OagwProblem::new(
            type_ids::CONNECTION_TIMEOUT,
            "Connection Timeout",
            StatusCode::GATEWAY_TIMEOUT,
        )
        .detail(format!("upstream connection timed out: {brief}"))
    } else if m.contains("refused") || m.contains("connection reset") {
        OagwProblem::new(
            type_ids::LINK_UNAVAILABLE,
            "Link Unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .detail(format!("upstream link unavailable: {brief}"))
    } else if m.contains("dns") || m.contains("resolve") || m.contains("cannot find") {
        OagwProblem::new(
            type_ids::DOWNSTREAM_ERROR,
            "Downstream Error",
            StatusCode::BAD_GATEWAY,
        )
        .detail(format!("could not resolve upstream host: {brief}"))
    } else if m.contains("protocol error") || m.contains("malformed") {
        OagwProblem::new(
            type_ids::PROTOCOL_ERROR,
            "Protocol Error",
            StatusCode::BAD_GATEWAY,
        )
        .detail(format!("upstream protocol error: {brief}"))
    } else if m.contains("connection closed") || m.contains("closed before message") {
        OagwProblem::new(
            type_ids::STREAM_ABORTED,
            "Stream Aborted",
            StatusCode::BAD_GATEWAY,
        )
        .detail(format!("upstream stream aborted: {brief}"))
    } else {
        OagwProblem::new(
            type_ids::DOWNSTREAM_ERROR,
            "Downstream Error",
            StatusCode::BAD_GATEWAY,
        )
        .detail(format!("upstream request failed: {brief}"))
    };
    gateway(problem)
}

// ---------------------------------------------------------------------------
// Header helpers
// ---------------------------------------------------------------------------

/// Headers the gateway manages and never forwards to the upstream: hop-by-hop
/// headers (HTTP spec) and the OAGW routing header (DESIGN "Routing Headers").
fn is_protected_header(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(name) || name.as_str().eq_ignore_ascii_case(TARGET_HOST_HEADER)
}

/// A `Sec-WebSocket-*` handshake header (RFC 6455 §4.3, e.g. Key/Version/
/// Protocol/Extension/Accept) relayed end-to-end on an upgrade,
/// case-insensitive on the name.
fn is_websocket_handshake_header(name: &HeaderName) -> bool {
    name.as_str()
        .get(..13)
        .is_some_and(|p| p.eq_ignore_ascii_case("sec-websocket"))
}

/// Collect `set`/`add` request-header rules into wire-ready header pairs
/// (skipping gateway-protected names and `Content-Length`, which the gateway
/// derives from the buffered body).
fn valid_rule_pairs(
    rules: &std::collections::BTreeMap<String, String>,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut pairs = Vec::new();
    for (name, value) in rules {
        let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        if is_protected_header(&n) || n == CONTENT_LENGTH {
            continue;
        }
        pairs.push((n, v));
    }
    pairs
}

/// Apply response-direction `set`/`add`/`remove` rules in place.
fn apply_header_ops_set_add_remove(headers: &mut HeaderMap, ops: &HeaderOps) {
    for name in &ops.remove {
        if let Ok(n) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&n);
        }
    }
    for (name, value) in &ops.set {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(n, v);
        }
    }
    for (name, value) in &ops.add {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(n, v);
        }
    }
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Ensure a route path has a leading '/'.
fn normalize_route_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// Parse the query string (the part after '?') from a suffix into decoded
/// `(name, value)` pairs.
fn extract_query_params(suffix: &str) -> Vec<(String, String)> {
    let q = suffix.split_once('?').map_or("", |(_, q)| q);
    form_urlencoded::parse(q.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Validate an `X-OAGW-Target-Host` value: a hostname or IP, no port/path or
/// scheme decorations (DESIGN error table: `InvalidTargetHost`).
fn valid_target_host(t: &str) -> bool {
    if t.is_empty() || t.len() > 253 {
        return false;
    }
    // Scheme/port/userinfo/path/fragment/whitespace decorations are rejected
    // before the IP check so an IPv6 literal ("2001:db8::1") — which contains
    // ':' but is a perfectly valid target — is not misclassified.
    if t.contains('/')
        || t.contains('?')
        || t.contains('#')
        || t.contains('@')
        || t.contains(' ')
    {
        return false;
    }
    if is_ip(t) {
        return true;
    }
    // A bare ':' at this point is a port decoration on a hostname → invalid.
    if t.contains(':') {
        return false;
    }
    // `alias_is_valid_format` requires lower-case; hosts may arrive mixed-case.
    alias_is_valid_format(&normalize_alias(t))
}

fn join_endpoint_hosts(endpoints: &[Endpoint]) -> String {
    endpoints
        .iter()
        .map(|e| e.host.clone())
        .collect::<Vec<_>>()
        .join(",")
}

/// The HTTP authority for an endpoint: `host:port`, with an IPv6 literal
/// bracketed (`[2001:db8::1]:8080`) — the same bracketing rule as RFC 3986
/// §3.2.2. Shared by the outbound request-line URI authority and the outbound
/// `Host` header (Rr-003) so the two can never drift (a raw IPv6 host would
/// otherwise read as `2001:db8::1:8080`, ambiguous/malformed).
fn authority_for(host: &str, port: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    format!("{host}:{port}")
}

/// Build the outbound request URI: an `http://authority` base (the host,
/// bracketed when an IPv6 literal, with the port — see [`authority_for`]), the
/// matched outbound path re-encoded segment-by-segment, and the joined query
/// string.
///
/// The path is percent-encoded per segment and `./`/`..` dot segments are
/// collapsed per RFC 3986 §5.2.4, so a client-supplied `..` traversal can
/// never climb out of the matched route into a sibling path, and raw decoded
/// bytes (spaces, non-ASCII UTF-8, control bytes) are never copied verbatim
/// onto the wire. Control bytes are rejected with 400 before the URI is
/// parsed because the `url` crate would otherwise percent-encode them into a
/// well-formed-but-ambiguous target (Rf-004).
fn build_outbound_uri(
    authority: &str,
    path: &str,
    query: &str,
) -> Result<Uri, Box<Response>> {
    if path
        .as_bytes()
        .iter()
        .copied()
        .find(|b| *b == 0 || *b < 0x20 || *b == 0x7f)
        .is_some()
    {
        return Err(Box::new(gateway(OagwProblem::validation(
            "request path contains an unsupported control byte",
        ))));
    }

    let base = format!("http://{authority}");
    let mut url = url::Url::parse(&base).map_err(|e| {
        gateway(OagwProblem::downstream(format!(
            "cannot build upstream authority '{authority}': {e}"
        )))
    })?;

    // Split on '/' below the leading slash and resolve dot segments manually
    // (PathSegmentsMut percent-encodes opaque values but ignores "."/".."
    // instead of resolving them, so pop-semantics are applied here).
    let mut segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut resolved: Vec<&str> = Vec::with_capacity(segments.len());
    for seg in segments.drain(..) {
        if seg == ".." {
            resolved.pop();
        } else {
            resolved.push(seg);
        }
    }

    let mut segmented = url.path_segments_mut().map_err(|()| {
        gateway(OagwProblem::downstream(
            "cannot override the upstream URI path (cannot-be-a-base URL)",
        ))
    })?;
    segmented.clear();
    for seg in resolved {
        segmented.push(seg);
    }
    // Preserve the intent of a trailing '/'.
    if path.ends_with('/') {
        segmented.push("");
    }
    drop(segmented);

    if !query.is_empty() {
        url.set_query(Some(query));
    }

    url.as_str().parse::<Uri>().map_err(|e| {
        Box::new(gateway(OagwProblem::downstream(format!(
            "cannot parse upstream URI: {e}"
        ))))
    })
}

// ---------------------------------------------------------------------------
// Body validation
// ---------------------------------------------------------------------------

/// Validate the request body per DESIGN "Body Validation Rules" and buffer it:
///
/// | Check | Rule | Error |
/// |---|---|---|
/// | Content-Length | Must be a valid integer if present; must match actual size | 400 |
/// | Max size | Hard limit 100 MiB; reject before buffering | 413 |
/// | Transfer-Encoding | Only `chunked` supported | 400 |
async fn validate_and_buffer(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<Bytes, Response> {
    // Transfer-Encoding: only chunked.
    if let Some(te) = headers.get(TRANSFER_ENCODING) {
        let te = te.to_str().unwrap_or("").to_ascii_lowercase();
        if te != "chunked" {
            return Err(gateway(OagwProblem::validation(format!(
                "unsupported Transfer-Encoding '{te}' (only 'chunked' is supported)"
            ))));
        }
    }

    // Content-Length: must be a valid integer.
    let declared: Option<u64> = match headers.get(CONTENT_LENGTH) {
        None => None,
        Some(v) => match v.to_str().ok().and_then(|s| s.trim().parse::<u64>().ok()) {
            Some(n) => Some(n),
            None => {
                return Err(gateway(OagwProblem::validation(
                    "Content-Length must be a valid non-negative integer",
                )));
            }
        },
    };
    // Reject before buffering when the declared size already exceeds the limit.
    if let Some(len) = declared
        && len > limit as u64
    {
        return Err(gateway(payload_too_large(limit)));
    }

    // Buffer with the hard limit.
    let bytes = match limited_body_bytes(body, limit).await {
        Ok(b) => b,
        Err(BodyReadError::OverLimit) => return Err(gateway(payload_too_large(limit))),
        Err(BodyReadError::Read) => {
            // A read/transport failure is not the client's fault: surface a
            // gateway-sourced 502 rather than blaming the payload (DESIGN
            // error table), distinct from an over-limit 413.
            error!("failed reading request body from client (transport/read error)");
            return Err(gateway(
                OagwProblem::downstream("failed to read the request body from the client"),
            ));
        }
    };

    // Content-Length must match the actual body size.
    if let Some(len) = declared
        && len != bytes.len() as u64
    {
        return Err(gateway(OagwProblem::validation(format!(
            "Content-Length {len} does not match actual body size {}",
            bytes.len()
        ))));
    }
    Ok(bytes)
}

/// Why body buffering failed (DESIGN error table mapping by the caller).
enum BodyReadError {
    /// The body exceeded the hard limit → 413.
    OverLimit,
    /// A read/transport failure occurred → 502 `DOWNSTREAM_ERROR`.
    Read,
}

/// Collect the body into memory, failing once more than `limit` bytes are
/// read (the body is buffered before forwarding per DESIGN).
async fn limited_body_bytes(mut body: Body, limit: usize) -> Result<Bytes, BodyReadError> {
    use futures_util::future::poll_fn;
    use hyper::body::Body as _;

    let body_pin = Pin::new(&mut body);
    let mut body = body_pin;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let polled = poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        let Some(frame) = polled else { break };
        let frame = frame.map_err(|_| BodyReadError::Read)?;
        if let Ok(data) = frame.into_data() {
            if buf.len().saturating_add(data.len()) > limit {
                return Err(BodyReadError::OverLimit);
            }
            buf.extend_from_slice(&data);
        }
        // Trailers are ignored for proxying.
    }
    Ok(Bytes::from(buf))
}

// ---------------------------------------------------------------------------
// Upgrade bridging
// ---------------------------------------------------------------------------

/// Adapts `hyper::upgrade::Upgraded` (hyper's `rt::{Read, Write}` IO) onto
/// `tokio::io::{AsyncRead, AsyncWrite}` so it can be used with
/// `tokio::io::copy_bidirectional`.
struct HyperUpgradedIo {
    inner: hyper::upgrade::Upgraded,
}

impl HyperUpgradedIo {
    fn new(inner: hyper::upgrade::Upgraded) -> Self {
        Self { inner }
    }
}

impl tokio::io::AsyncRead for HyperUpgradedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        tbuf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Fill only up to the space tokio has left.
        let (result, filled) = {
            // `initialize_unfilled` (safe) views the remaining capacity; the
            // hyper reader below fills it and reports how many bytes were
            // written, bounding `set_filled`.
            let unfilled = tbuf.initialize_unfilled();
            let mut hbuf = hyper::rt::ReadBuf::new(unfilled);
            let r = hyper::rt::Read::poll_read(Pin::new(&mut self.inner), cx, hbuf.unfilled());
            (r, hbuf.filled().len())
        };
        match result {
            Poll::Ready(Ok(())) => {
                // `filled` bytes were written by the hyper reader.
                tbuf.set_filled(filled);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl tokio::io::AsyncWrite for HyperUpgradedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        hyper::rt::Write::poll_write(Pin::new(&mut self.inner), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        hyper::rt::Write::poll_flush(Pin::new(&mut self.inner), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        hyper::rt::Write::poll_shutdown(Pin::new(&mut self.inner), cx)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn route_path_normalization() {
        assert_eq!(normalize_route_path("/v1/chat"), "/v1/chat");
        assert_eq!(normalize_route_path("v1/chat"), "/v1/chat");
        assert_eq!(normalize_route_path(""), "/");
    }

    #[test]
    fn query_params_are_decoded_pairs() {
        let params = extract_query_params("/v1/chat?model=gpt-4&x=a%20b");
        assert_eq!(
            params,
            vec![
                ("model".to_owned(), "gpt-4".to_owned()),
                ("x".to_owned(), "a b".to_owned()),
            ]
        );
        assert!(extract_query_params("/plain").is_empty());
    }

    #[test]
    fn target_host_validation() {
        assert!(valid_target_host("api.example.com"));
        assert!(valid_target_host("API.Example.COM"));
        assert!(valid_target_host("10.0.0.5"));
        // IPv6 literals are valid targets despite containing ':'.
        assert!(valid_target_host("2001:db8::1"));
        assert!(valid_target_host("::1"));
        assert!(!valid_target_host("api.example.com:443"));
        assert!(!valid_target_host("http://api.example.com"));
        assert!(!valid_target_host("api.example.com/path"));
        assert!(!valid_target_host("api.example.com?x=1"));
        assert!(!valid_target_host(""));
        assert!(!valid_target_host("a b"));
        assert!(!valid_target_host("proto://host"));
    }

    #[test]
    fn authority_for_brackets_ipv6_literals_only() {
        // IPv6 literals must be bracketed (Rr-003): a bare host would read as
        // `2001:db8::1:8080`, ambiguous/malformed.
        assert_eq!(authority_for("2001:db8::1", 8080), "[2001:db8::1]:8080");
        assert_eq!(authority_for("::1", 80), "[::1]:80");
        // Hostnames and IPv4 are untouched; already-bracketed hosts are not
        // double-bracketed.
        assert_eq!(authority_for("example.com", 443), "example.com:443");
        assert_eq!(authority_for("10.0.0.5", 80), "10.0.0.5:80");
        assert_eq!(authority_for("[::1]", 8080), "[::1]:8080");
    }

    #[test]
    fn build_outbound_uri_normalizes_dot_segments_and_encodes() {
        // Traversal is collapsed: no literal `..` escapes the matched path.
        let uri = build_outbound_uri("api.example.com:8080", "/v1/../..", "")
            .unwrap()
            .to_string();
        assert_eq!(uri, "http://api.example.com:8080/");
        // Encoded traversal (`%2e%2e`) stays an opaque re-encoded segment —
        // never decoded into a literal `..` and never a verbatim copy.
        let uri = build_outbound_uri("up:8080", "/a/%2e%2e/b", "")
            .unwrap()
            .to_string();
        assert_eq!(uri, "http://up:8080/a/%252e%252e/b");
        // Raw decoded bytes (space) are re-encoded, never copied verbatim.
        let uri = build_outbound_uri("up:8080", "/a b/c", "")
            .unwrap()
            .to_string();
        assert_eq!(uri, "http://up:8080/a%20b/c");
        // Unicode is percent-encoded.
        let uri = build_outbound_uri("up:8080", "/h\u{e9}llo", "")
            .unwrap()
            .to_string();
        assert_eq!(uri, "http://up:8080/h%C3%A9llo");
        // A bracketed IPv6 authority with port round-trips through the URI.
        let uri = build_outbound_uri("[2001:db8::1]:8080", "/v1", "")
            .unwrap()
            .to_string();
        assert_eq!(uri, "http://[2001:db8::1]:8080/v1");
        // Query passthrough is joined.
        let uri = build_outbound_uri("up:8080", "/v1", "a=1&b=2")
            .unwrap()
            .to_string();
        assert_eq!(uri, "http://up:8080/v1?a=1&b=2");
        // Trailing slash intent is preserved.
        let uri = build_outbound_uri("up:8080", "/v1/", "")
            .unwrap()
            .to_string();
        assert_eq!(uri, "http://up:8080/v1/");
        // Control bytes are rejected with a 400 validation.
        let err = build_outbound_uri("up:8080", "/a\u{0}b", "").unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn protected_headers_exclude_routing_and_hop_by_hop() {
        assert!(is_protected_header(&HeaderName::from_static("host")));
        assert!(is_protected_header(&HeaderName::from_static("connection")));
        assert!(is_protected_header(&HeaderName::from_static(
            "x-oagw-target-host"
        )));
        assert!(!is_protected_header(&HeaderName::from_static(
            "content-type"
        )));
        assert!(!is_protected_header(&HeaderName::from_static(
            "authorization"
        )));
    }

    #[test]
    fn map_send_error_classifies_by_message() {
        let refused = map_send_error("tcp connect error: connection refused", "connection refused", false);
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        let timeout = map_send_error("connection timed out", "connection timed out", true);
        assert_eq!(timeout.status(), StatusCode::GATEWAY_TIMEOUT);
        let dns = map_send_error("dns error: no such host", "no such host", false);
        assert_eq!(dns.status(), StatusCode::BAD_GATEWAY);
        let other = map_send_error("something else", "something else", false);
        assert_eq!(other.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn body_validation_rejects_oversize_and_bad_content_length() {
        let limit = 10usize;
        // Declared length beyond the limit → 413 before buffering.
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("999"));
        let resp = validate_and_buffer(&headers, Body::empty(), limit)
            .await
            .unwrap_err();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Declared length not matching the actual body → 400.
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("5"));
        let resp = validate_and_buffer(&headers, Body::from("hello!"), limit)
            .await
            .unwrap_err();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Actual body beyond the limit → 413.
        let resp = validate_and_buffer(&HeaderMap::new(), Body::from("0123456789X"), limit)
            .await
            .unwrap_err();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Unsupported transfer-encoding → 400.
        let mut headers = HeaderMap::new();
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("gzip"));
        let resp = validate_and_buffer(&headers, Body::empty(), limit)
            .await
            .unwrap_err();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Valid request under the limit buffers fine.
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("3"));
        let bytes = validate_and_buffer(&headers, Body::from("abc"), limit)
            .await
            .unwrap();
        assert_eq!(bytes, Bytes::from_static(b"abc"));
    }
}
