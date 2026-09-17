//! Data plane: the OAGW proxy engine.
//!
//! Implements the DESIGN §3.3 proxy flow: CORS preflight short-circuit,
//! tenant-chain alias resolution, X-OAGW-Target-Host endpoint selection,
//! route matching (method + longest path prefix / gRPC service-method),
//! body validation, effective rate limiting, header transformation, the
//! Auth → Guard → Transform plugin pipeline, and upstream round-trips with
//! gateway/upstream error-source distinction.

pub mod headers;
pub mod problem;
pub mod ratelimit;

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, HttpBody, to_bytes};
use axum::http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient, TenantResolverError};
use toolkit_http::{HttpClient, HttpClientBuilder, HttpClientConfig, HttpError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::{derive_alias, normalize_alias, valid_hostname};
use crate::domain::models::{
    CorsConfig, Endpoint, PathSuffixMode, RateLimitConfig, RateLimitScope, Route, RouteMatch,
    SharingMode, Upstream,
};
use crate::domain::plugin::{
    AuthPlugin, BoundPlugin, GuardPlugin, PluginBindingRef, PluginRegistry, RequestContext,
    ResponseContext, TransformPlugin,
};
use crate::domain::repo::{RouteRepo, UpstreamRepo};
use crate::domain::service::DataPlaneService;
use crate::gts;
use crate::infra::plugins::PluginRegistryImpl;
use crate::infra::proxy::headers::{apply_response_rules, plan_request_headers};
use crate::infra::proxy::problem::append_vary_origin;
use crate::infra::proxy::ratelimit::{RateLimiter, RateLimitOutcome, effective_bucket};

/// Hard request-body limit (100MB) per the DESIGN body validation rules.
const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;

/// Data-plane service over the in-memory repositories and the plugin
/// registry.
pub struct DataPlaneServiceImpl {
    upstreams: Arc<dyn UpstreamRepo>,
    routes: Arc<dyn RouteRepo>,
    plugins: Arc<PluginRegistryImpl>,
    tenants: Arc<dyn TenantResolverClient>,
    http: HttpClient,
    /// Round-robin counter for multi-endpoint pools without a target host.
    rr: AtomicUsize,
    limiter: RateLimiter,
}

impl std::fmt::Debug for DataPlaneServiceImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneServiceImpl").finish_non_exhaustive()
    }
}

impl DataPlaneServiceImpl {
    /// Construct the data plane with its dependencies.
    ///
    /// # Errors
    ///
    /// Returns `HttpError` if the internal HTTP client cannot be built.
    pub fn new(
        upstreams: Arc<dyn UpstreamRepo>,
        routes: Arc<dyn RouteRepo>,
        plugins: Arc<PluginRegistryImpl>,
        tenants: Arc<dyn TenantResolverClient>,
        config: OagwConfig,
    ) -> Result<Self, HttpError> {
        let mut http_config = HttpClientConfig::proxy();
        http_config.request_timeout = Duration::from_secs(config.proxy_timeout_secs.max(1));
        let http = HttpClientBuilder::with_config(http_config).build()?;
        Ok(Self {
            upstreams,
            routes,
            plugins,
            tenants,
            http,
            rr: AtomicUsize::new(0),
            limiter: RateLimiter::new(),
        })
    }
}

/// Split the proxy request path into the alias and the remaining suffix.
///
/// The gear is mounted under `api-gateway`'s path prefix, so the request
/// path is `/…/oagw/v1/proxy/{alias}[/{suffix}]`. Returns `None` when the
/// `/proxy/` marker is absent or the alias is empty.
fn split_proxy_path(path: &str) -> Option<(String, String)> {
    let marker = "/proxy/";
    let idx = path.find(marker)? + marker.len();
    let rest = &path[idx..];
    let (alias, suffix) = match rest.split_once('/') {
        Some((a, s)) => (a, format!("/{s}")),
        None => (rest, String::new()),
    };
    if alias.is_empty() {
        return None;
    }
    Some((alias.to_owned(), suffix))
}

/// Whether the request is a CORS preflight (handled permissively).
fn is_cors_preflight(req: &axum::http::Request<Body>) -> bool {
    req.method() == Method::OPTIONS
        && req.headers().contains_key("origin")
        && req.headers().contains_key("access-control-request-method")
}

/// Permissive preflight response (ADR 0004): echoes the requested origin,
/// method, and headers with a 204 and a 24h max-age.
fn preflight_response(req: &axum::http::Request<Body>) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    let headers = resp.headers_mut();
    if let Some(o) = req.headers().get("origin") {
        headers.insert("access-control-allow-origin", o.clone());
    }
    if let Some(m) = req.headers().get("access-control-request-method") {
        headers.insert("access-control-allow-methods", m.clone());
    }
    if let Some(h) = req.headers().get("access-control-request-headers") {
        headers.insert("access-control-allow-headers", h.clone());
    }
    headers.insert(
        "access-control-max-age",
        HeaderValue::from_static("86400"),
    );
    headers.insert(
        "vary",
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    resp
}

/// Validate an `X-OAGW-Target-Host` value: hostname or IP address, with no
/// port, path, or special characters (ADR 0001 / DESIGN).
fn valid_target_host(value: &str) -> bool {
    if value.is_empty()
        || value
            .bytes()
            .any(|b| !b.is_ascii_alphanumeric() && b != b'.' && b != b'-')
    {
        return false;
    }
    valid_hostname(value) || value.parse::<Ipv4Addr>().is_ok()
}

/// Remainder of `suffix` beyond the route `path` prefix, with a path-segment
/// boundary guarantee. `None` when `suffix` does not start with `path`.
fn path_remainder<'a>(route_path: &str, suffix: &'a str) -> Option<&'a str> {
    if suffix == route_path {
        return Some("");
    }
    let rest = suffix.strip_prefix(route_path)?;
    if rest.starts_with('/') {
        Some(rest)
    } else {
        None
    }
}

/// Effective CORS config after merging ancestor-enforced origins.
#[derive(Debug, Clone, Default)]
struct EffectiveCors {
    allowed_origins: Vec<String>,
    allowed_methods: Vec<String>,
    expose_headers: Vec<String>,
    allow_credentials: bool,
    enabled: bool,
}

impl EffectiveCors {
    fn from_config(c: &CorsConfig) -> Self {
        Self {
            allowed_origins: c.allowed_origins.clone(),
            allowed_methods: c.allowed_methods.clone(),
            expose_headers: c.expose_headers.clone(),
            allow_credentials: c.allow_credentials,
            enabled: c.enabled,
        }
    }

    fn merge_enforced(&mut self, c: &CorsConfig) {
        if c.sharing != SharingMode::Enforce {
            return;
        }
        for o in &c.allowed_origins {
            if !self.allowed_origins.contains(o) {
                self.allowed_origins.push(o.clone());
            }
        }
        for m in &c.allowed_methods {
            if !self
                .allowed_methods
                .iter()
                .any(|x| x.eq_ignore_ascii_case(m))
            {
                self.allowed_methods.push(m.clone());
            }
        }
    }

    fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }

    fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// Planned plugin chain for one proxied request.
#[derive(Default)]
struct PlannedChain {
    auth: Option<(Box<dyn AuthPlugin>, serde_json::Value)>,
    guards: Vec<(Box<dyn GuardPlugin>, serde_json::Value)>,
    transforms: Vec<(Box<dyn TransformPlugin>, serde_json::Value)>,
}

#[async_trait]
impl DataPlaneService for DataPlaneServiceImpl {
    async fn proxy(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
        req: axum::http::Request<Body>,
        target_host_header: Option<String>,
    ) -> Response {
        self.proxy_inner(tenant_id, subject_id, req, target_host_header)
            .await
    }
}

impl DataPlaneServiceImpl {
    async fn proxy_inner(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
        req: axum::http::Request<Body>,
        target_host_header: Option<String>,
    ) -> Response {
        // 1. CORS preflight: permissive 204, no upstream resolution.
        if is_cors_preflight(&req) {
            return preflight_response(&req);
        }

        // 2. Parse alias + suffix from the request path.
        let (alias, suffix) = match split_proxy_path(req.uri().path()) {
            Some(v) => v,
            None => return problem::route_not_found("proxy path is missing an alias"),
        };

        // Owned snapshots survive the body being moved out of `req` later.
        let inbound_headers = req.headers().clone();
        let query = req.uri().query().map(str::to_owned);
        let method = req.method().clone();
        let origin = req
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        // 3. Build the caller's security context (secret resolution uses the
        //    authenticated subject).
        let sec = match SecurityContext::builder()
            .subject_id(subject_id)
            .subject_tenant_id(tenant_id)
            .build()
        {
            Ok(s) => s,
            Err(e) => return problem::validation(format!("invalid security context: {e}")),
        };

        // 4. Tenant chain (self → root): shadowing, closest match wins.
        let chain = match self.tenant_chain(&sec, tenant_id).await {
            Ok(c) => c,
            Err(resp) => return resp,
        };

        // 5. Alias resolution across the chain.
        let alias_norm = normalize_alias(&alias);
        let mut owner_tenant: Option<Uuid> = None;
        let selected: Option<Arc<Upstream>> = chain
            .iter()
            .find_map(|tid| {
                let found = self
                    .upstreams
                    .list(*tid)
                    .into_iter()
                    .find(|u| normalize_alias(&u.alias) == alias_norm);
                if found.is_some() {
                    owner_tenant = Some(*tid);
                }
                found
            });
        let Some(upstream) = selected else {
            return problem::route_not_found(format!("no upstream found for alias '{alias}'"));
        };
        let owner_tenant = owner_tenant.expect("owner set when upstream found");
        if !upstream.enabled {
            return problem::link_unavailable(format!("upstream '{alias}' is disabled"));
        }

        // 6. Endpoint selection (X-OAGW-Target-Host matrix, ADR 0001).
        let target_host = target_host_header.or_else(|| {
            inbound_headers
                .get("x-oagw-target-host")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        });
        let endpoint = match self.select_endpoint(&upstream, target_host.as_deref()) {
            Ok(e) => e,
            Err(resp) => return resp,
        };

        // 7. Route resolution (protocol-scoped).
        let up_id = upstream.id.unwrap_or(Uuid::nil());
        let mut candidates: Vec<Arc<Route>> = Vec::new();
        for tid in &chain {
            for r in self.routes.list(*tid) {
                if r.upstream_id == up_id && r.enabled {
                    candidates.push(r);
                }
            }
        }
        let route = match self.select_route(&upstream, &candidates, &method, &query, &suffix) {
            Ok(r) => r,
            Err(resp) => return resp,
        };

        // 8. Effective rate limiting (route + upstream + enforced ancestors).
        let scope = route
            .rate_limit
            .as_ref()
            .or(upstream.rate_limit.as_ref())
            .map(|c| c.scope)
            .unwrap_or(RateLimitScope::Tenant);
        let scope_key = match scope {
            RateLimitScope::Global => "global".to_owned(),
            RateLimitScope::Tenant => format!("tenant:{tenant_id}"),
            RateLimitScope::User => format!("user:{subject_id}"),
            RateLimitScope::Ip => format!("ip:{}", client_ip(&inbound_headers)),
            RateLimitScope::Route => format!("route:{}", route.id.unwrap_or(Uuid::nil())),
        };
        // Effective (rate, capacity) = min over selected + enforced ancestors.
        let mut eff_rate: Option<f64> = None;
        let mut eff_capacity: Option<f64> = None;
        let mut cost: u64 = 1;
        let mut merge = |cfg: &RateLimitConfig, is_selected: bool| {
            let (r, c) = effective_bucket(cfg);
            eff_rate = Some(eff_rate.map_or(r, |x: f64| x.min(r)));
            eff_capacity = Some(eff_capacity.map_or(c, |x: f64| x.min(c)));
            if is_selected {
                cost = u64::from(cfg.cost.max(1));
            }
        };
        if let Some(c) = route.rate_limit.as_ref() {
            merge(c, upstream.rate_limit.is_none());
        }
        if let Some(c) = upstream.rate_limit.as_ref() {
            merge(c, true);
        }
        // Enforced ancestors (same alias, `sharing: enforce`).
        for tid in &chain {
            if *tid == owner_tenant {
                break;
            }
            for u in self.upstreams.list(*tid) {
                if normalize_alias(&u.alias) != alias_norm {
                    continue;
                }
                if let Some(rc) = u.rate_limit.as_ref() {
                    if rc.sharing == SharingMode::Enforce {
                        merge(rc, false);
                    }
                }
            }
        }
        if let (Some(rate), Some(capacity)) = (eff_rate, eff_capacity) {
            match self
                .limiter
                .check(&format!("oagw:ratelimit:{scope_key}"), rate, capacity, cost)
            {
                RateLimitOutcome::Limited {
                    limit,
                    retry_after_secs,
                    reset_epoch,
                } => {
                    return problem::rate_limited(
                        format!("rate limit exceeded for scope '{scope_key}'"),
                        retry_after_secs,
                        limit,
                        reset_epoch,
                    );
                }
                RateLimitOutcome::Allowed { .. } => {}
            }
        }

        // 9. CORS actual-request check (upstream/route config, enabled).
        let mut effective_cors: Option<EffectiveCors> = None;
        if origin.is_some() {
            let selected_cors = route.cors.as_ref().or(upstream.cors.as_ref());
            if let Some(c) = selected_cors {
                let mut eff = EffectiveCors::from_config(c);
                for tid in &chain {
                    if *tid == owner_tenant {
                        break;
                    }
                    for u in self.upstreams.list(*tid) {
                        if normalize_alias(&u.alias) == alias_norm {
                            if let Some(uc) = u.cors.as_ref() {
                                eff.merge_enforced(uc);
                            }
                        }
                    }
                }
                if eff.enabled {
                    let origin = origin.clone().unwrap_or_default();
                    if !eff.allows_origin(&origin) {
                        return problem::Problem::response(
                            StatusCode::FORBIDDEN,
                            gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
                            "CORS Origin Not Allowed",
                            format!("origin '{origin}' not in allowed origins list"),
                        );
                    }
                    if !eff.allows_method(method.as_str()) {
                        return problem::Problem::response(
                            StatusCode::FORBIDDEN,
                            gts::ERR_CORS_METHOD_NOT_ALLOWED,
                            "CORS Method Not Allowed",
                            format!("method '{method}' not in allowed methods list"),
                        );
                    }
                    effective_cors = Some(eff);
                }
            }
        }

        // 10. Plugin request phase (Auth → Guards → Transforms).
        let bound = match self.plan_plugins(&upstream, &route, owner_tenant).await {
            Ok(b) => b,
            Err(resp) => return resp,
        };
        let plugin_outbound = {
            let mut rctx = RequestContext {
                headers: &inbound_headers,
                outbound: HeaderMap::new(),
                config: &serde_json::Value::Null,
                method: method.clone(),
                path: suffix.clone(),
                security: &sec,
                tenant_id: owner_tenant,
            };
            if let Some((auth, cfg)) = &bound.auth {
                rctx.config = cfg;
                if let Err(e) = auth.authenticate(&mut rctx).await {
                    return problem::auth_error(&e);
                }
            }
            for (guard, cfg) in &bound.guards {
                rctx.config = cfg;
                if let Err(e) = guard.guard_request(&rctx).await {
                    return problem::guard_error(&e);
                }
            }
            for (transform, cfg) in &bound.transforms {
                rctx.config = cfg;
                if let Err(e) = transform.transform_request(&mut rctx).await {
                    return problem::transform_error(&e);
                }
            }
            rctx.outbound
        };

        // 11. Body validation + buffering (100MB cap, content-length match).
        if let Some(te) = inbound_headers.get(TRANSFER_ENCODING) {
            if let Ok(v) = te.to_str() {
                if !v.eq_ignore_ascii_case("chunked") {
                    return problem::validation(format!(
                        "unsupported transfer-encoding '{v}' (only chunked is supported)"
                    ));
                }
            }
        }
        let declared_length = inbound_headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        // Pre-check the declared size hint so an oversized body is rejected
        // with 413 before buffering.
        if req
            .body()
            .size_hint()
            .exact()
            .is_some_and(|n| n as usize > MAX_REQUEST_BODY)
        {
            return problem::payload_too_large("request body exceeds the 100MB limit");
        }
        let body = match to_bytes(req.into_body(), MAX_REQUEST_BODY).await {
            Ok(b) => b,
            Err(e) => return problem::validation(format!("failed to read request body: {e}")),
        };
        if let Some(d) = declared_length {
            if d as usize != body.len() {
                return problem::validation(format!(
                    "content-length {d} does not match actual body size {}",
                    body.len()
                ));
            }
        }

        // 12. Outbound header plan: passthrough + rules, then plugin headers,
        //     with an accurate content-length for the re-buffered body.
        let mut outbound = plan_request_headers(
            &inbound_headers,
            &upstream.headers,
            &endpoint.authority(),
        );
        for (name, value) in &plugin_outbound {
            outbound.insert(name.clone(), value.clone());
        }
        outbound.remove(TRANSFER_ENCODING);
        outbound.remove(CONTENT_LENGTH);
        if !body.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&body.len().to_string()) {
                outbound.insert(CONTENT_LENGTH, v);
            }
        }

        // 13. Forward to the upstream.
        let upstream_path = if suffix.is_empty() { "/" } else { suffix.as_str() };
        let mut url = format!("{}{}", endpoint.base_url(), upstream_path);
        if let Some(q) = &query {
            url.push('?');
            url.push_str(q);
        }
        let http_resp = match self.forward(&url, &method, &outbound, &body).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };

        // 14. Response processing: strip hop-by-hop, apply rules + response
        //     plugins, add CORS headers, mark upstream source.
        let status = http_resp.status();
        let mut resp_headers = http_resp.headers().clone();
        strip_response_headers(&mut resp_headers);
        apply_response_rules(&mut resp_headers, &upstream.headers.response);
        let empty = serde_json::Value::Null;
        {
            let mut rctx = ResponseContext {
                headers: &mut resp_headers,
                status,
                config: &empty,
            };
            for (guard, cfg) in &bound.guards {
                rctx.config = cfg;
                if let Err(e) = guard.guard_response(&mut rctx).await {
                    return problem::guard_error(&e);
                }
            }
            for (transform, cfg) in &bound.transforms {
                rctx.config = cfg;
                if let Err(e) = transform.transform_response(&mut rctx).await {
                    return problem::transform_error(&e);
                }
            }
        }
        if let (Some(cors), Some(origin)) = (&effective_cors, &origin) {
            add_cors_response_headers(&mut resp_headers, cors, origin);
        }
        let limited = http_resp.into_limited_body();
        let mut response = Response::new(Body::new(limited));
        *response.status_mut() = status;
        *response.headers_mut() = resp_headers;
        problem::with_upstream_source(response)
    }

    async fn tenant_chain(
        &self,
        sec: &SecurityContext,
        tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, Response> {
        match self
            .tenants
            .get_ancestors(sec, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
        {
            Ok(resp) => {
                let mut chain = vec![resp.tenant.id.0];
                for a in resp.ancestors {
                    chain.push(a.id.0);
                }
                Ok(chain)
            }
            Err(TenantResolverError::TenantNotFound { .. }) => {
                Err(problem::route_not_found("the calling tenant does not exist"))
            }
            Err(e) => Err(problem::Problem::response(
                StatusCode::BAD_GATEWAY,
                gts::ERR_PROTOCOL_ERROR,
                "Protocol Error",
                format!("tenant resolution failed: {e}"),
            )),
        }
    }

    fn select_endpoint(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<Endpoint, Response> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(problem::route_not_found("upstream has no configured endpoints"));
        }
        let info = derive_alias(endpoints);
        match target_host {
            Some(h) => {
                if !valid_target_host(h) {
                    return Err(problem::invalid_target_host());
                }
                let h = h.trim_end_matches('.').to_ascii_lowercase();
                for e in endpoints {
                    if e.normalized_host() == h || e.authority() == h {
                        return Ok(e.clone());
                    }
                }
                Err(problem::unknown_target_host(format!(
                    "target host '{h}' does not match any configured endpoint"
                )))
            }
            None => {
                if endpoints.len() == 1 {
                    return Ok(endpoints[0].clone());
                }
                if info.target_host_required {
                    return Err(problem::missing_target_host());
                }
                // Multi-endpoint pool (explicit alias): round-robin.
                let idx = self.rr.fetch_add(1, Ordering::Relaxed) % endpoints.len();
                Ok(endpoints[idx].clone())
            }
        }
    }

    fn select_route(
        &self,
        upstream: &Upstream,
        candidates: &[Arc<Route>],
        method: &Method,
        query: &Option<String>,
        suffix: &str,
    ) -> Result<Arc<Route>, Response> {
        let is_grpc = upstream.protocol.as_str() == gts::PROTOCOL_GRPC;
        if !is_grpc {
            let method = method.as_str();
            let mut best: Option<(usize, Arc<Route>)> = None;
            for r in candidates {
                let Some(m) = r.match_.as_http() else {
                    continue;
                };
                if !m.methods.iter().any(|x| x == method) {
                    continue;
                }
                let Some(remainder) = path_remainder(&m.path, suffix) else {
                    continue;
                };
                if m.path_suffix_mode == PathSuffixMode::Disabled && !remainder.is_empty() {
                    continue;
                }
                let len = m.path.len();
                if best.as_ref().map(|(bl, _)| len > *bl).unwrap_or(true) {
                    best = Some((len, r.clone()));
                }
            }
            match best {
                Some((_, route)) => {
                    // Query allowlist enforcement (empty = allow none).
                    let m = route.match_.as_http().expect("http route");
                    if let Some(q) = query {
                        for kv in q.split('&') {
                            let name = kv.split('=').next().unwrap_or("");
                            if !m.query_allowlist.iter().any(|a| a == name) {
                                return Err(problem::validation(format!(
                                    "query parameter '{name}' is not allowed by this route"
                                )));
                            }
                        }
                    }
                    Ok(route)
                }
                None => Err(problem::route_not_found(format!(
                    "no route matches method {method} and path '{suffix}'"
                ))),
            }
        } else {
            let trimmed = suffix.trim_start_matches('/');
            let mut segs = trimmed.splitn(2, '/');
            let (Some(service), Some(rpc_method)) = (segs.next(), segs.next()) else {
                return Err(problem::route_not_found(format!(
                    "gRPC path '{suffix}' must be /{{service}}/{{method}}"
                )));
            };
            for r in candidates {
                if let RouteMatch::Grpc(m) = &r.match_ {
                    if m.service == service && m.method == rpc_method {
                        return Ok(r.clone());
                    }
                }
            }
            Err(problem::route_not_found(format!(
                "no gRPC route matches {service}/{rpc_method}"
            )))
        }
    }

    async fn plan_plugins(
        &self,
        upstream: &Upstream,
        route: &Route,
        owner_tenant: Uuid,
    ) -> Result<PlannedChain, Response> {
        let mut chain = PlannedChain::default();
        // Upstream auth plugin.
        if let Some(plugin_type) = &upstream.auth.plugin_type {
            let binding = PluginBindingRef {
                plugin_ref: plugin_type.clone(),
                config: upstream.auth.config.clone(),
                tenant_id: owner_tenant,
            };
            match self.resolve_any(&binding).await {
                Ok(BoundPlugin::Auth(a, c)) => chain.auth = Some((a, c)),
                Ok(_) => {
                    return Err(problem::plugin_not_found(format!(
                        "auth plugin '{plugin_type}' is not an auth plugin"
                    )))
                }
                Err(detail) => return Err(problem::plugin_not_found(detail)),
            };
        }
        // Upstream guards + transforms, then route ones.
        for item in upstream.plugins.items.iter().chain(route.plugins.items.iter()) {
            let binding = PluginBindingRef {
                plugin_ref: item.plugin_ref().to_owned(),
                config: item.config(),
                tenant_id: owner_tenant,
            };
            match self.resolve_any(&binding).await {
                Ok(BoundPlugin::Guard(g, c)) => chain.guards.push((g, c)),
                Ok(BoundPlugin::Transform(t, c)) => chain.transforms.push((t, c)),
                Ok(BoundPlugin::Auth(..)) => {
                    // Auth can only be configured via `upstream.auth`.
                }
                Err(detail) => return Err(problem::plugin_not_found(detail)),
            }
        }
        Ok(chain)
    }

    async fn resolve_any(
        &self,
        binding: &PluginBindingRef,
    ) -> Result<BoundPlugin, String> {
        self.plugins.resolve(binding).await
    }

    async fn forward(
        &self,
        url: &str,
        method: &Method,
        outbound: &HeaderMap,
        body: &Bytes,
    ) -> Result<toolkit_http::HttpResponse, Response> {
        let method_owned = method.clone();
        let mut builder = match method_owned {
            Method::GET => self.http.get(url),
            Method::POST => self.http.post(url),
            Method::PUT => self.http.put(url),
            Method::PATCH => self.http.patch(url),
            Method::DELETE => self.http.delete(url),
            Method::HEAD => self.http.head(url),
            Method::OPTIONS => self.http.options(url),
            other => {
                return Err(problem::validation(format!(
                    "method '{other}' is not supported by the gateway"
                )))
            }
        };
        for (name, value) in outbound {
            if let Ok(v) = value.to_str() {
                builder = builder.header(name.as_str(), v);
            }
        }
        builder
            .body_bytes(body.clone())
            .send()
            .await
            .map_err(|e| problem::transport_error(&e))
    }
}

fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Strip hop-by-hop headers from an upstream response before it reaches the
/// client (including `content-encoding` since the client may decompress).
fn strip_response_headers(headers: &mut HeaderMap) {
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-encoding",
    ] {
        headers.remove(name);
    }
}

/// Add CORS response headers for an allowed actual request (ADR 0004).
fn add_cors_response_headers(headers: &mut HeaderMap, cors: &EffectiveCors, origin: &str) {
    let allow_origin = if cors.allowed_origins.iter().any(|o| o == "*") {
        "*"
    } else {
        origin
    };
    if let Ok(v) = HeaderValue::from_str(allow_origin) {
        headers.insert("access-control-allow-origin", v);
    }
    if cors.allow_credentials {
        headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    if !cors.expose_headers.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&cors.expose_headers.join(", ")) {
            headers.insert("access-control-expose-headers", v);
        }
    }
    append_vary_origin(headers);
}
