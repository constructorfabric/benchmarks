//! OAGW data plane: request routing, hierarchical config merge, plugins,
//! rate limiting, CORS, header transformation, and upstream proxying.
//!
//! Gateway-originated errors are emitted as RFC 9457 Problem responses with
//! OAGW-specific GTS `type` identifiers and `X-OAGW-Error-Source: gateway`.
//! Upstream responses (including upstream error bodies) pass through
//! unmodified with `X-OAGW-Error-Source: upstream`.

pub mod limits;
pub(crate) mod oauth;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use http::{HeaderValue, Method, StatusCode};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::gts;
use crate::model::{AuthConfig, CorsConfig, CustomPlugin, RateLimitConfig, Upstream};
use crate::state::OagwState;

use self::limits::{LimitOutcome, LimitSnapshot};

/// Hop-by-hop headers (RFC 9110) plus routing-only headers stripped before
/// forwarding to the upstream.
const HOP_BY_HOP: &[&str] = &[
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
    "content-length",
];

/// Effective, merged view of an upstream plus inherited ancestors and the
/// matched route.
struct EffectiveConfig {
    upstream: Upstream,
    /// Effective auth config (ancestor `enforce` wins).
    auth: Option<AuthConfig>,
    /// Effective ordered plugin bindings `(owner_tenant, plugin_ref)`.
    plugins: Vec<(Uuid, String)>,
    /// Effective CORS config (ancestors unioned / enforced).
    cors: Option<CorsConfig>,
    /// Effective rate limit (min across visible ancestors + selected).
    rate_limit: Option<RateLimitConfig>,
}

/// Resolved upstream target endpoint.
struct EndpointTarget {
    host: String,
    port: u16,
    scheme: String,
    /// `Some` for http/https proxying.
    url_scheme: Option<String>,
}

/// Execute one proxy request end-to-end.
///
/// `suffix` is the raw `{*rest}` capture after the alias (empty, or starting
/// with `/`).
pub async fn proxy_request(
    state: Arc<OagwState>,
    sec: &SecurityContext,
    alias: &str,
    suffix: &str,
    mut req: http::Request<Body>,
) -> http::Response<Body> {
    // CORS preflight: permissive 204 at the handler level, before any
    // upstream resolution or tenant context (ADR-0004).
    if let Some(preflight) = maybe_preflight(&req, suffix) {
        return preflight;
    }

    let chain = state.tenant.ancestors(sec.subject_tenant_id(), sec).await;

    let selected = match state.resolve_upstream(&chain, alias) {
        Some(u) => u,
        None => {
            return gateway_problem(
                StatusCode::NOT_FOUND,
                gts::ERR_ROUTE_NOT_FOUND,
                "Route Not Found",
                format!("no upstream or route matches alias {alias:?}"),
                suffix,
                None,
            );
        }
    };

    let effective = effective_config(state.as_ref(), &chain, &selected, alias);

    // Target-endpoint resolution (X-OAGW-Target-Host / round-robin).
    let target = match resolve_target(state.as_ref(), &effective, &req, suffix) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // Protocol classification (DESIGN §3.3): gRPC is catalog-only.
    if crate::alias::is_grpc_protocol(&effective.upstream.protocol) {
        return gateway_problem(
            StatusCode::BAD_GATEWAY,
            gts::ERR_PROTOCOL_ERROR,
            "Protocol Error",
            "gRPC proxying is not implemented; upstream is catalog-only",
            suffix,
            None,
        );
    }
    // Only http/https endpoints are proxiable.
    if target.url_scheme.is_none() {
        return gateway_problem(
            StatusCode::BAD_GATEWAY,
            gts::ERR_PROTOCOL_ERROR,
            "Protocol Error",
            format!("endpoint scheme {:?} is not proxiable", target.scheme),
            suffix,
            None,
        );
    }

    // Route resolution (longest path prefix + method allowlist).
    let matched = match resolve_route(
        state.as_ref(),
        &chain,
        effective.upstream.id,
        req.method(),
        suffix,
    ) {
        Ok(m) => m,
        Err(resp) => return resp,
    };
    let (route, upstream_path) = matched;
    let route_plugins: Vec<(Uuid, String)> = route
        .as_ref()
        .map(|r| {
            r.plugins
                .items
                .iter()
                .map(|p| (r.tenant_id, p.clone()))
                .collect()
        })
        .unwrap_or_default();

    // CORS origin/method enforcement on the actual request (post-resolution).
    if let Some(cors) = &effective.cors {
        if cors.enabled {
            if let Err(resp) = check_cors_actual(cors, &req, suffix) {
                return resp;
            }
        }
    }

    // Effective rate limit (min across chain + route).
    let merged_rate = merge_rate_limit(&effective, route.as_ref());

    // ---------------------------------------------------------------
    // Build the outbound request
    // ---------------------------------------------------------------
    let mut out_headers = build_outbound_headers(&effective, &req);

    // Request body (configurable max; per spec the hard cap is 100 MB).
    let body = match read_request_body(state.as_ref(), &mut req, suffix).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    // Query filtering against the route's allowlist.
    let query = match filter_query(req.uri().query(), route.as_ref(), suffix) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    // Rate limiting (before credential injection; rejected calls cost nothing).
    let rate_snapshot = if let Some(rl) = &merged_rate {
        let (outcome, snapshot) = check_rate_limit(state.as_ref(), sec, rl, route.as_ref(), &req);
        match outcome {
            LimitOutcome::Allowed => Some(snapshot),
            LimitOutcome::Rejected { wait_secs } => {
                return rate_limited_response(rl, snapshot, wait_secs, suffix);
            }
        }
    } else {
        None
    };

    // Auth plugins (credential injection).
    if let Some(auth) = &effective.auth {
        if let Err(resp) =
            inject_credentials(state.as_ref(), sec, auth, &mut out_headers, suffix).await
        {
            return resp;
        }
    }

    // Guard + transform order: Auth → Guards → Transform (DESIGN §3.6).
    // Route plugins run after upstream plugins (same GTS base).
    let mut chain_items: Vec<(Uuid, String)> = effective.plugins.clone();
    chain_items.extend(route_plugins);
    if let Err(resp) = run_guards_request(state.as_ref(), &chain_items, &out_headers, suffix).await
    {
        return resp;
    }
    if let Err(resp) =
        run_request_transforms(state.as_ref(), sec, &chain_items, &mut out_headers, suffix).await
    {
        return resp;
    }

    // ---------------------------------------------------------------
    // Forward to upstream
    // ---------------------------------------------------------------
    let mut upstream_response = match forward(
        state.as_ref(),
        &target,
        req.method(),
        &upstream_path,
        &query,
        &out_headers,
        &body,
        suffix,
    )
    .await
    {
        Ok(resp) => resp,
        Err(resp) => return resp,
    };
    upstream_response
        .headers_mut()
        .insert("x-oagw-error-source", HeaderValue::from_static("upstream"));

    // Response header transforms.
    if let Some(headers) = &effective.upstream.headers {
        apply_response_header_rules(&mut upstream_response, &headers.response);
    }

    // Guard response phase (required response headers).
    if let Err(resp) =
        run_guards_response(state.as_ref(), &chain_items, upstream_response.headers(), suffix).await
    {
        return resp;
    }

    // CORS response headers.
    if let Some(cors) = &effective.cors {
        if cors.enabled {
            add_cors_response_headers(cors, &req, &mut upstream_response);
        }
    }

    // Rate limit response headers.
    if let (Some(rl), Some(snapshot)) = (merged_rate.as_ref(), rate_snapshot.as_ref()) {
        if rl.include_response_headers() {
            add_rate_limit_response_headers(&mut upstream_response, snapshot);
        }
    }

    upstream_response
}

// ---------------------------------------------------------------------------
// Preflight + problem responses
// ---------------------------------------------------------------------------

fn maybe_preflight(req: &http::Request<Body>, _suffix: &str) -> Option<http::Response<Body>> {
    if req.method() != Method::OPTIONS {
        return None;
    }
    let origin = req.headers().get("origin")?.to_str().ok()?.to_owned();
    let acrm = req
        .headers()
        .get("access-control-request-method")?
        .to_str()
        .ok()?
        .to_owned();
    let acrh = req
        .headers()
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*")
        .to_owned();

    let mut resp = http::Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    let headers = resp.headers_mut();
    headers.insert(
        "access-control-allow-origin",
        HeaderValue::from_str(&origin).unwrap_or_else(|_| HeaderValue::from_static("*")),
    );
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_str(&acrm).unwrap_or_else(|_| HeaderValue::from_static("*")),
    );
    headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_str(&acrh).unwrap(),
    );
    headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
    headers.insert(
        "vary",
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    Some(resp)
}

/// Build an RFC 9457 gateway error response.
pub(crate) fn gateway_problem(
    status: StatusCode,
    type_id: &str,
    title: &str,
    detail: impl Into<String>,
    suffix: &str,
    extra: Option<Vec<(&'static str, String)>>,
) -> http::Response<Body> {
    let mut error = serde_json::json!({
        "type": type_id,
        "title": title,
        "status": status.as_u16(),
        "detail": detail.into(),
        "instance": normalized_request_path(suffix),
    });
    if let Some(extra) = extra.as_ref() {
        for (k, v) in extra {
            if *k == "retry-after" {
                if let Ok(secs) = v.parse::<u64>() {
                    error["retry_after_seconds"] = serde_json::json!(secs);
                }
            }
        }
    }
    let bytes = serde_json::to_vec(&error).unwrap_or_else(|_| b"{}".to_vec());

    let mut resp = http::Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/problem+json"),
    );
    headers.insert("x-oagw-error-source", HeaderValue::from_static("gateway"));
    if let Some(extra) = extra {
        for (k, v) in extra {
            if let Ok(v) = HeaderValue::from_str(&v) {
                headers.insert(k, v);
            }
        }
    }
    resp
}

// ---------------------------------------------------------------------------
// Hierarchical effective config
// ---------------------------------------------------------------------------

/// Merge all visible (non-`private`) ancestor configs with the selected
/// upstream per the sharing modes (DESIGN §3.2).
fn effective_config(
    state: &OagwState,
    chain: &[Uuid],
    selected: &Upstream,
    alias: &str,
) -> EffectiveConfig {
    let bindings = state.upstream_bindings(chain, alias);
    // Order: root → descendant (ancestor-most first), selected last.
    let mut ordered: Vec<&Upstream> = bindings
        .iter()
        .rev()
        .map(|(_t, u)| u)
        .filter(|u| u.enabled && u.tenant_id != selected.tenant_id)
        .collect();
    ordered.push(selected);

    let mut auth = selected.auth.clone();
    let mut plugins: Vec<(Uuid, String)> = Vec::new();
    let mut cors = selected.cors.clone();
    let mut rates: Vec<&RateLimitConfig> = Vec::new();

    for u in &ordered {
        // Auth: enforce → ancestor wins regardless of descendant.
        if let Some(a) = &u.auth {
            match a.sharing {
                crate::model::SharingMode::Enforce => auth = Some(a.clone()),
                crate::model::SharingMode::Inherit => {
                    if auth.is_none() {
                        auth = Some(a.clone());
                    }
                }
                crate::model::SharingMode::Private => {}
            }
        }
        // Plugins: concatenate; private chains are invisible to descendants.
        if u.plugins.sharing != crate::model::SharingMode::Private {
            plugins.extend(u.plugins.items.iter().map(|p| (u.tenant_id, p.clone())));
        }
        // CORS: enforce replaces; inherit unions origins/methods.
        if let Some(c) = &u.cors {
            match c.sharing {
                crate::model::SharingMode::Enforce => cors = Some(c.clone()),
                crate::model::SharingMode::Inherit => match &mut cors {
                    Some(acc) => {
                        if acc.sharing != crate::model::SharingMode::Enforce {
                            acc.enabled = acc.enabled || c.enabled;
                            for o in &c.allowed_origins {
                                if !acc.allowed_origins.contains(o) {
                                    acc.allowed_origins.push(o.clone());
                                }
                            }
                            for m in &c.allowed_methods {
                                if !acc.allowed_methods.contains(m) {
                                    acc.allowed_methods.push(m.clone());
                                }
                            }
                        }
                    }
                    None => cors = Some(c.clone()),
                },
                crate::model::SharingMode::Private => {}
            }
        }
        // Rate limits: every visible one participates in `min()`.
        if let Some(r) = &u.rate_limit {
            if u.tenant_id == selected.tenant_id || r.sharing != crate::model::SharingMode::Private
            {
                rates.push(r);
            }
        }
    }

    // Deduplicate plugins preserving order.
    let mut seen: Vec<String> = Vec::new();
    plugins.retain(|(_t, p)| {
        if seen.contains(p) {
            false
        } else {
            seen.push(p.clone());
            true
        }
    });

    let rate_limit = if rates.is_empty() {
        None
    } else {
        Some(merge_rate_configs(rates))
    };

    EffectiveConfig {
        upstream: selected.clone(),
        auth,
        plugins,
        cors,
        rate_limit,
    }
}

fn per_second(rate: u64, window: crate::model::RateLimitWindow) -> f64 {
    rate as f64 / window.as_secs().max(1) as f64
}

fn merge_rate_configs(configs: Vec<&RateLimitConfig>) -> RateLimitConfig {
    let mut best = configs[0].clone();
    for c in &configs[1..] {
        if per_second(c.sustained.rate, c.sustained.window)
            < per_second(best.sustained.rate, best.sustained.window)
        {
            best.sustained = c.sustained.clone();
        }
        if c.burst_capacity() < best.burst_capacity() {
            best.burst = c.burst.clone();
        }
        best.cost = best.cost.min(c.cost);
    }
    best
}

/// Effective rate limit for this request, min-merged with the route's own.
fn merge_rate_limit(
    effective: &EffectiveConfig,
    route: Option<&crate::model::Route>,
) -> Option<RateLimitConfig> {
    let mut configs: Vec<&RateLimitConfig> = Vec::new();
    if let Some(rl) = &effective.rate_limit {
        configs.push(rl);
    }
    if let Some(route) = route {
        if let Some(rl) = &route.rate_limit {
            configs.push(rl);
        }
    }
    if configs.is_empty() {
        None
    } else {
        Some(merge_rate_configs(configs))
    }
}

// ---------------------------------------------------------------------------
// Target endpoint resolution
// ---------------------------------------------------------------------------

fn resolve_target(
    state: &OagwState,
    effective: &EffectiveConfig,
    req: &http::Request<Body>,
    suffix: &str,
) -> Result<EndpointTarget, http::Response<Body>> {
    let endpoints = &effective.upstream.server.endpoints;
    if endpoints.is_empty() {
        return Err(gateway_problem(
            StatusCode::BAD_GATEWAY,
            gts::ERR_PROTOCOL_ERROR,
            "Protocol Error",
            "upstream has no endpoints",
            suffix,
            None,
        ));
    }

    let header = req
        .headers()
        .get("x-oagw-target-host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if endpoints.len() == 1 {
        let e = &endpoints[0];
        if let Some(h) = header {
            if !valid_target_host(&h) {
                return Err(gateway_problem(
                    StatusCode::BAD_REQUEST,
                    gts::ERR_INVALID_TARGET_HOST,
                    "Invalid X-OAGW-Target-Host",
                    "must be a hostname or IP without port/path",
                    suffix,
                    None,
                ));
            }
            if lower(&h) != lower(&e.host) {
                return Err(gateway_problem(
                    StatusCode::BAD_REQUEST,
                    gts::ERR_UNKNOWN_TARGET_HOST,
                    "Unknown X-OAGW-Target-Host",
                    format!("{h:?} does not match the upstream endpoint"),
                    suffix,
                    None,
                ));
            }
        }
        return Ok(EndpointTarget {
            host: e.host.clone(),
            port: e.effective_port(),
            scheme: e.scheme.clone(),
            url_scheme: e.url_scheme().map(str::to_owned),
        });
    }

    // Multi-endpoint pool.
    let common_suffix = endpoints_appear_common_suffix(endpoints, &effective.upstream.alias);
    match header {
        Some(h) => {
            if !valid_target_host(&h) {
                return Err(gateway_problem(
                    StatusCode::BAD_REQUEST,
                    gts::ERR_INVALID_TARGET_HOST,
                    "Invalid X-OAGW-Target-Host",
                    "must be a hostname or IP without port/path",
                    suffix,
                    None,
                ));
            }
            match endpoints.iter().find(|e| lower(&e.host) == lower(&h)) {
                Some(e) => Ok(EndpointTarget {
                    host: e.host.clone(),
                    port: e.effective_port(),
                    scheme: e.scheme.clone(),
                    url_scheme: e.url_scheme().map(str::to_owned),
                }),
                None => Err(gateway_problem(
                    StatusCode::BAD_REQUEST,
                    gts::ERR_UNKNOWN_TARGET_HOST,
                    "Unknown X-OAGW-Target-Host",
                    format!("{h:?} does not match any configured endpoint"),
                    suffix,
                    None,
                )),
            }
        }
        None if common_suffix => Err(gateway_problem(
            StatusCode::BAD_REQUEST,
            gts::ERR_MISSING_TARGET_HOST,
            "Missing X-OAGW-Target-Host",
            "required for a multi-endpoint upstream with a common-suffix alias",
            suffix,
            None,
        )),
        None => {
            // Explicit alias: round-robin across the pool.
            let idx = round_robin_next(state, effective.upstream.id, endpoints.len());
            let e = &endpoints[idx % endpoints.len()];
            Ok(EndpointTarget {
                host: e.host.clone(),
                port: e.effective_port(),
                scheme: e.scheme.clone(),
                url_scheme: e.url_scheme().map(str::to_owned),
            })
        }
    }
}

fn endpoints_appear_common_suffix(endpoints: &[crate::model::Endpoint], alias: &str) -> bool {
    let hosts: Vec<(String, String, Option<u16>)> = endpoints
        .iter()
        .map(|e| (e.scheme.clone(), e.host.clone(), e.port))
        .collect();
    let distinct: HashSet<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();
    match crate::alias::derive_alias(&hosts) {
        Some(derived) => derived == lower(alias) && distinct.len() > 1,
        None => false,
    }
}

fn lower(s: &str) -> String {
    s.trim_end_matches('.').to_ascii_lowercase()
}

fn valid_target_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    h.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

fn round_robin_next(state: &OagwState, upstream_id: Uuid, len: usize) -> usize {
    let counter = state
        .rr_counters
        .entry(upstream_id)
        .or_insert_with(|| AtomicU64::new(0));
    counter.fetch_add(1, Ordering::Relaxed) as usize % len.max(1)
}

// ---------------------------------------------------------------------------
// Route resolution
// ---------------------------------------------------------------------------

/// Longest-path-prefix route match (segment aware) with method allowlist.
/// Returns `(route, upstream_path)`.
fn resolve_route(
    state: &OagwState,
    chain: &[Uuid],
    upstream_id: Uuid,
    method: &Method,
    suffix: &str,
) -> Result<(Option<crate::model::Route>, String), http::Response<Body>> {
    let request_path = normalized_request_path(suffix);
    let routes = state.routes_for_upstream(chain, upstream_id);

    // Candidate routes whose match path is a segment-aware prefix.
    let mut candidates: Vec<(usize, crate::model::Route)> = Vec::new();
    for (_tenant, r) in &routes {
        if let Some(h) = &r.r#match.http {
            if path_prefix_matches(&request_path, &h.path) {
                candidates.push((h.path.len(), r.clone()));
            }
        }
    }
    if candidates.is_empty() {
        return Err(gateway_problem(
            StatusCode::NOT_FOUND,
            gts::ERR_ROUTE_NOT_FOUND,
            "Route Not Found",
            "no route matches this path",
            suffix,
            None,
        ));
    }
    // Longest prefix first; ties broken by creation order (older first).
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.created_at.cmp(&b.1.created_at)));
    let longest = candidates[0].0;
    let longest: Vec<crate::model::Route> = candidates
        .into_iter()
        .take_while(|(len, _)| *len == longest)
        .map(|(_l, r)| r)
        .collect();

    // Method allowlist among the longest-prefix candidates.
    let route = longest
        .into_iter()
        .find(|r| {
            r.r#match
                .http
                .as_ref()
                .is_some_and(|h| h.methods.iter().any(|m| m == method.as_str()))
        })
        .ok_or_else(|| {
            gateway_problem(
                StatusCode::BAD_REQUEST,
                gts::ERR_VALIDATION,
                "Validation Error",
                format!("method {:?} is not allowed on this route", method.as_str()),
                suffix,
                None,
            )
        })?;

    let h = route.r#match.http.as_ref().expect("http route");
    let base = h.path.clone();
    let rest = &request_path[base.len().min(request_path.len())..];
    match h.path_suffix_mode {
        crate::model::PathSuffixMode::Disabled if !rest.is_empty() => Err(gateway_problem(
            StatusCode::BAD_REQUEST,
            gts::ERR_VALIDATION,
            "Validation Error",
            "path suffix is disabled for this route",
            suffix,
            None,
        )),
        _ => {
            let upstream_path = if rest.is_empty() {
                base
            } else {
                format!("{base}{rest}")
            };
            Ok((Some(route), upstream_path))
        }
    }
}

/// Path remainder after the alias, treated as the request path.
fn normalized_request_path(suffix: &str) -> String {
    if suffix.is_empty() {
        "/".to_string()
    } else if suffix.starts_with('/') {
        suffix.to_string()
    } else {
        format!("/{suffix}")
    }
}

fn path_prefix_matches(request_path: &str, route_path: &str) -> bool {
    if route_path == "/" {
        return true;
    }
    if request_path == route_path {
        return true;
    }
    request_path.starts_with(route_path)
        && request_path.as_bytes().get(route_path.len()) == Some(&b'/')
}

// ---------------------------------------------------------------------------
// Body + query handling
// ---------------------------------------------------------------------------

async fn read_request_body(
    state: &OagwState,
    req: &mut http::Request<Body>,
    suffix: &str,
) -> Result<axum::body::Bytes, http::Response<Body>> {
    // Content-Length validation.
    if let Some(cl) = req.headers().get("content-length") {
        let s = cl.to_str().unwrap_or_default();
        let parsed: Result<u64, _> = s.parse();
        let parsed = match parsed {
            Ok(v) => v,
            Err(_) => {
                return Err(gateway_problem(
                    StatusCode::BAD_REQUEST,
                    gts::ERR_VALIDATION,
                    "Validation Error",
                    "invalid Content-Length",
                    suffix,
                    None,
                ));
            }
        };
        if parsed > state.config.max_request_body() as u64 {
            return Err(gateway_problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                gts::ERR_PAYLOAD_TOO_LARGE,
                "Payload Too Large",
                "request body exceeds the configured limit",
                suffix,
                None,
            ));
        }
    }

    let limit = state.config.max_request_body();
    match axum::body::to_bytes(
        std::mem::replace(req.body_mut(), axum::body::Body::empty()),
        limit + 1,
    )
    .await
    {
        Ok(bytes) if bytes.len() as u64 <= limit as u64 => Ok(bytes),
        Ok(_) => Err(gateway_problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            gts::ERR_PAYLOAD_TOO_LARGE,
            "Payload Too Large",
            "request body exceeds the configured limit",
            suffix,
            None,
        )),
        Err(_) => Err(gateway_problem(
            StatusCode::BAD_REQUEST,
            gts::ERR_VALIDATION,
            "Validation Error",
            "failed to read request body",
            suffix,
            None,
        )),
    }
}

fn filter_query(
    query: Option<&str>,
    route: Option<&crate::model::Route>,
    suffix: &str,
) -> Result<String, http::Response<Body>> {
    let allowlist: Vec<String> = route
        .and_then(|r| r.r#match.http.as_ref())
        .map(|h| h.query_allowlist.clone())
        .unwrap_or_default();

    let pairs: Vec<(String, String)> = match query {
        None => Vec::new(),
        Some(q) => form_urlencoded::parse(q.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
    };

    if allowlist.is_empty() || pairs.is_empty() {
        return Ok(match query {
            None => String::new(),
            Some(q) => q.to_string(),
        });
    }

    let mut kept: Vec<String> = Vec::new();
    for (k, v) in pairs {
        if !allowlist.contains(&k) {
            return Err(gateway_problem(
                StatusCode::BAD_REQUEST,
                gts::ERR_VALIDATION,
                "Validation Error",
                format!("query parameter {k:?} is not in the route's allowlist"),
                suffix,
                None,
            ));
        }
        kept.push(format!("{k}={v}"));
    }
    Ok(if kept.is_empty() {
        String::new()
    } else {
        kept.join("&")
    })
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

fn rate_limit_key(
    rl: &RateLimitConfig,
    sec: &SecurityContext,
    route: Option<&crate::model::Route>,
    client_ip: Option<&str>,
) -> String {
    let scope = match rl.scope {
        crate::model::RateLimitScope::Global => "global".to_string(),
        crate::model::RateLimitScope::Tenant => sec.subject_tenant_id().to_string(),
        crate::model::RateLimitScope::User => sec.subject_id().to_string(),
        crate::model::RateLimitScope::Ip => client_ip.unwrap_or("unknown").to_string(),
        crate::model::RateLimitScope::Route => route
            .map(|r| r.id.to_string())
            .unwrap_or_else(|| "noroute".to_string()),
    };
    let cfg = format!(
        "{:?}:{:?}:{}:{}",
        rl.sustained.window,
        rl.algorithm,
        rl.sustained.rate,
        rl.burst_capacity()
    );
    format!("{scope}:{cfg}")
}

fn check_rate_limit(
    state: &OagwState,
    sec: &SecurityContext,
    rl: &RateLimitConfig,
    route: Option<&crate::model::Route>,
    req: &http::Request<Body>,
) -> (LimitOutcome, LimitSnapshot) {
    let client_ip = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim);
    let key = rate_limit_key(rl, sec, route, client_ip);
    let refill = per_second(rl.sustained.rate, rl.sustained.window);
    let capacity = rl.burst_capacity();
    let cost = rl.cost.max(1);
    state.rate_limiter.check(&key, refill, capacity, cost)
}

fn rate_limited_response(
    rl: &RateLimitConfig,
    snapshot: LimitSnapshot,
    wait_secs: u64,
    suffix: &str,
) -> http::Response<Body> {
    let mut extra: Vec<(&'static str, String)> = Vec::new();
    extra.push(("retry-after", wait_secs.to_string()));
    let mut resp = gateway_problem(
        StatusCode::TOO_MANY_REQUESTS,
        gts::ERR_RATE_LIMIT_EXCEEDED,
        "Rate Limit Exceeded",
        "request rate exceeds the configured limit",
        suffix,
        Some(extra),
    );
    if rl.include_response_headers() {
        add_rate_limit_response_headers(&mut resp, &snapshot);
    }
    resp
}

fn add_rate_limit_response_headers(resp: &mut http::Response<Body>, snapshot: &LimitSnapshot) {
    let headers = resp.headers_mut();
    headers.insert(
        "x-rate-limit-limit",
        HeaderValue::from_str(&snapshot.limit.to_string()).unwrap(),
    );
    headers.insert(
        "x-rate-limit-remaining",
        HeaderValue::from_str(&snapshot.remaining.to_string()).unwrap(),
    );
    headers.insert(
        "x-rate-limit-reset",
        HeaderValue::from_str(&snapshot.reset_in.to_string()).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// CORS (actual requests)
// ---------------------------------------------------------------------------

fn check_cors_actual(
    cors: &CorsConfig,
    req: &http::Request<Body>,
    suffix: &str,
) -> Result<(), http::Response<Body>> {
    let origin = match req.headers().get("origin").and_then(|v| v.to_str().ok()) {
        Some(o) if !o.is_empty() => o,
        _ => return Ok(()), // non-browser request: no CORS constraint
    };
    let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
    let origin_ok = wildcard || cors.allowed_origins.iter().any(|o| o == origin);
    if !origin_ok {
        return Err(gateway_problem(
            StatusCode::FORBIDDEN,
            gts::ERR_VALIDATION,
            "CORS origin not allowed",
            format!("origin {origin:?} is not in the allowlist"),
            suffix,
            None,
        ));
    }
    let method_ok = cors.allowed_methods.iter().any(|m| m == req.method().as_str());
    if !method_ok {
        return Err(gateway_problem(
            StatusCode::FORBIDDEN,
            gts::ERR_VALIDATION,
            "CORS method not allowed",
            format!("method {:?} is not in the allowlist", req.method().as_str()),
            suffix,
            None,
        ));
    }
    Ok(())
}

fn add_cors_response_headers(
    cors: &CorsConfig,
    req: &http::Request<Body>,
    resp: &mut http::Response<Body>,
) {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*")
        .to_string();
    let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
    let headers = resp.headers_mut();
    let allow_origin = if wildcard && !cors.allow_credentials {
        "*".to_string()
    } else {
        origin
    };
    headers.insert(
        "access-control-allow-origin",
        HeaderValue::from_str(&allow_origin).unwrap_or_else(|_| HeaderValue::from_static("*")),
    );
    if !cors.expose_headers.is_empty() {
        headers.insert(
            "access-control-expose-headers",
            HeaderValue::from_str(&cors.expose_headers.join(", ")).unwrap(),
        );
    }
    if cors.allow_credentials {
        headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    let existing_vary = headers
        .get("vary")
        .and_then(|v| v.to_str().ok())
        .map(|v| format!("{v}, Origin"))
        .unwrap_or_else(|| "Origin".to_string());
    headers.insert("vary", HeaderValue::from_str(&existing_vary).unwrap());
}

// ---------------------------------------------------------------------------
// Header transformation
// ---------------------------------------------------------------------------

fn build_outbound_headers(
    effective: &EffectiveConfig,
    req: &http::Request<Body>,
) -> Vec<(String, String)> {
    let passthrough = effective
        .upstream
        .headers
        .as_ref()
        .map(|h| h.request.passthrough)
        .unwrap_or(crate::model::HeaderPassthrough::None);

    let mut out: Vec<(String, String)> = Vec::new();
    let inbound_all: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            if HOP_BY_HOP.contains(&name.as_str()) {
                return None;
            }
            value.to_str().ok().map(|v| (name, v.to_string()))
        })
        .collect();

    match passthrough {
        crate::model::HeaderPassthrough::None => {}
        crate::model::HeaderPassthrough::Allowlist => {
            let allowlist = effective
                .upstream
                .headers
                .as_ref()
                .map(|h| {
                    h.request
                        .passthrough_allowlist
                        .iter()
                        .map(|s| s.to_ascii_lowercase())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for (name, value) in &inbound_all {
                if allowlist.contains(&name.to_ascii_lowercase()) {
                    out.push((name.clone(), value.clone()));
                }
            }
        }
        crate::model::HeaderPassthrough::All => out.extend(inbound_all),
    }

    // set/add/remove rules.
    if let Some(h) = &effective.upstream.headers {
        remove_headers(&mut out, &h.request.remove);
        for (k, v) in &h.request.set {
            set_header(&mut out, &k.to_ascii_lowercase(), v);
        }
        for (k, v) in &h.request.add {
            out.push((k.to_ascii_lowercase(), v.clone()));
        }
    }

    out
}

fn remove_headers(out: &mut Vec<(String, String)>, names: &[String]) {
    let lowered: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
    out.retain(|(name, _)| !lowered.contains(name));
}

fn set_header(out: &mut Vec<(String, String)>, name: &str, value: &str) {
    out.retain(|(n, _)| n != name);
    out.push((name.to_string(), value.to_string()));
}

fn apply_response_header_rules(
    resp: &mut http::Response<Body>,
    rules: &crate::model::ResponseHeaderRules,
) {
    for h in &rules.remove {
        resp.headers_mut().remove(h.as_str());
    }
    for (k, v) in &rules.set {
        if let (Ok(n), Ok(v)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            http::header::HeaderValue::from_str(v),
        ) {
            resp.headers_mut().insert(n, v);
        }
    }
    for (k, v) in &rules.add {
        if let (Ok(n), Ok(v)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            http::header::HeaderValue::from_str(v),
        ) {
            resp.headers_mut().append(n, v);
        }
    }
}

// ---------------------------------------------------------------------------
// Auth plugins
// ---------------------------------------------------------------------------

async fn inject_credentials(
    state: &OagwState,
    sec: &SecurityContext,
    auth: &AuthConfig,
    out_headers: &mut Vec<(String, String)>,
    suffix: &str,
) -> Result<(), http::Response<Body>> {
    let t = auth.auth_type.clone();
    match t.as_str() {
        gts::AUTH_NOOP => Ok(()),
        gts::AUTH_APIKEY => {
            let header = auth
                .config
                .get("header")
                .and_then(|v| v.as_str())
                .unwrap_or("x-api-key")
                .to_string();
            let value: Option<String> = auth
                .config
                .get("api_key")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    auth.config
                        .get("key")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                });
            let value = match value {
                Some(v) => v,
                None => {
                    let cred = auth
                        .config
                        .get("secret_ref")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    match resolve_secret(state, sec, cred.as_deref(), suffix).await? {
                        Some(v) => v,
                        None => {
                            return Err(gateway_problem(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                gts::ERR_SECRET_NOT_FOUND,
                                "Secret Not Found",
                                "referenced API key secret could not be resolved",
                                suffix,
                                None,
                            ));
                        }
                    }
                }
            };
            set_header(out_headers, &header.to_ascii_lowercase(), &value);
            Ok(())
        }
        gts::AUTH_OAUTH2_CC | gts::AUTH_OAUTH2_CC_BASIC => {
            let is_basic = t == gts::AUTH_OAUTH2_CC_BASIC;
            let token = oauth::token_for(state, sec, auth, is_basic, suffix).await?;
            set_header(out_headers, "authorization", &format!("Bearer {token}"));
            Ok(())
        }
        _ => Err(gateway_problem(
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_PLUGIN_NOT_FOUND,
            "Plugin Not Found",
            format!("auth plugin {t:?} has no backing implementation"),
            suffix,
            None,
        )),
    }
}

/// Resolve `cred://<name>` (or bare `<name>`) against the credential store.
pub(crate) async fn resolve_secret(
    state: &OagwState,
    sec: &SecurityContext,
    cred: Option<&str>,
    suffix: &str,
) -> Result<Option<String>, http::Response<Body>> {
    let ref_name = cred
        .and_then(|c| c.strip_prefix("cred://"))
        .or(cred)
        .map(str::to_string);
    let ref_name = match ref_name {
        Some(n) if !n.is_empty() => n,
        _ => {
            return Err(gateway_problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                gts::ERR_SECRET_NOT_FOUND,
                "Secret Not Found",
                "auth configuration references no secret",
                suffix,
                None,
            ));
        }
    };
    let store = match &state.credstore {
        Some(s) => s.clone(),
        None => {
            return Err(gateway_problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                gts::ERR_SECRET_NOT_FOUND,
                "Secret Not Found",
                "no credential store is available",
                suffix,
                None,
            ));
        }
    };
    let secret_ref = match credstore_sdk::SecretRef::new(ref_name.as_str()) {
        Ok(r) => r,
        Err(_) => {
            return Err(gateway_problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                gts::ERR_SECRET_NOT_FOUND,
                "Secret Not Found",
                "invalid secret reference format",
                suffix,
                None,
            ));
        }
    };
    match store.get(sec, &secret_ref).await {
        Ok(Some(resp)) => Ok(Some(
            String::from_utf8_lossy(resp.value.as_bytes()).into_owned(),
        )),
        Ok(None) => Ok(None),
        Err(_) => Err(gateway_problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            gts::ERR_SECRET_NOT_FOUND,
            "Secret Not Found",
            "credential store lookup failed",
            suffix,
            None,
        )),
    }
}

// ---------------------------------------------------------------------------
// Guard + transform plugins
// ---------------------------------------------------------------------------

/// Parse a plugin reference into a custom plugin lookup, if UUID-backed.
fn lookup_custom(state: &OagwState, owner_tenant: Uuid, item: &str) -> Option<CustomPlugin> {
    let uuid = gts::plugin_uuid(item)?;
    state.plugin_by_uuid(owner_tenant, uuid)
}

fn plugin_config(state: &OagwState, owner: Uuid, item: &str) -> Option<serde_json::Value> {
    match gts::plugin_uuid(item) {
        Some(uuid) => state.plugin_by_uuid(owner, uuid).map(|p| p.config.clone()),
        None => None, // builtin: no custom config → fail-open
    }
}

/// Guard request phase (required_headers): 400 on first missing header.
async fn run_guard_request(
    state: &OagwState,
    owner: Uuid,
    item: &str,
    config: Option<&serde_json::Value>,
    out_headers: &[(String, String)],
    suffix: &str,
) -> Result<(), http::Response<Body>> {
    let required = config
        .and_then(|c| c.get("required_request_headers"))
        .and_then(|v| v.as_str())
        .map(parse_header_list)
        .unwrap_or_default();
    if required.is_empty() {
        return Ok(()); // fail-open (ADR-0009): no config → no constraint
    }
    for name in required {
        let present = out_headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(&name));
        if !present {
            return Err(gateway_problem(
                StatusCode::BAD_REQUEST,
                gts::ERR_VALIDATION,
                "Validation Error",
                format!("required header {name:?} is missing from the request"),
                suffix,
                None,
            ));
        }
    }
    let _ = (state, owner, item);
    Ok(())
}

/// Guard response phase (required_headers): 502 on first missing header.
async fn run_guard_response(
    state: &OagwState,
    owner: Uuid,
    item: &str,
    config: Option<&serde_json::Value>,
    headers: &http::HeaderMap,
    suffix: &str,
) -> Result<(), http::Response<Body>> {
    let required = config
        .and_then(|c| c.get("required_response_headers"))
        .and_then(|v| v.as_str())
        .map(parse_header_list)
        .unwrap_or_default();
    if required.is_empty() {
        return Ok(());
    }
    for name in required {
        let present = headers.get(name.as_str()).is_some();
        if !present {
            return Err(gateway_problem(
                StatusCode::BAD_GATEWAY,
                gts::ERR_PROTOCOL_ERROR,
                "Protocol Error",
                format!("required header {name:?} is missing from the upstream response"),
                suffix,
                None,
            ));
        }
    }
    let _ = (state, owner, item);
    Ok(())
}

/// Run guard request-phase plugins in binding order.
async fn run_guards_request(
    state: &OagwState,
    items: &[(Uuid, String)],
    out_headers: &[(String, String)],
    suffix: &str,
) -> Result<(), http::Response<Body>> {
    for (owner, item) in items {
        if gts::plugin_base(item) == gts::GUARD_PLUGIN_TYPE {
            let config = plugin_config(state, *owner, item);
            run_guard_request(state, *owner, item, config.as_ref(), out_headers, suffix).await?;
        }
    }
    Ok(())
}

/// Run guard response-phase plugins in binding order.
async fn run_guards_response(
    state: &OagwState,
    items: &[(Uuid, String)],
    headers: &http::HeaderMap,
    suffix: &str,
) -> Result<(), http::Response<Body>> {
    for (owner, item) in items {
        if gts::plugin_base(item) == gts::GUARD_PLUGIN_TYPE {
            let config = plugin_config(state, *owner, item);
            run_guard_response(state, *owner, item, config.as_ref(), headers, suffix).await?;
        }
    }
    Ok(())
}

/// Request transforms (request-id propagation).
async fn run_request_transforms(
    state: &OagwState,
    _sec: &SecurityContext,
    items: &[(Uuid, String)],
    out_headers: &mut Vec<(String, String)>,
    suffix: &str,
) -> Result<(), http::Response<Body>> {
    for (owner, item) in items {
        match gts::plugin_base(item) {
            gts::TRANSFORM_PLUGIN_TYPE if item == gts::TRANSFORM_REQUEST_ID => {
                ensure_request_id(out_headers, None);
            }
            gts::TRANSFORM_PLUGIN_TYPE => {
                // Custom transform plugin (UUID-backed): same request-id
                // behavior, header name from its config.
                let plugin = match lookup_custom(state, *owner, item) {
                    Some(p) => p,
                    None => {
                        return Err(gateway_problem(
                            StatusCode::SERVICE_UNAVAILABLE,
                            gts::ERR_PLUGIN_NOT_FOUND,
                            "Plugin Not Found",
                            format!("custom transform plugin {item:?} is not registered"),
                            suffix,
                            None,
                        ));
                    }
                };
                let header = plugin
                    .config
                    .get("header")
                    .and_then(|v| v.as_str())
                    .unwrap_or("x-request-id");
                ensure_request_id(out_headers, Some(header));
            }
            _ => {} // guards handled in the guard pass
        }
    }
    Ok(())
}

fn ensure_request_id(out_headers: &mut Vec<(String, String)>, header: Option<&str>) {
    let name = header.unwrap_or("x-request-id").to_ascii_lowercase();
    if out_headers.iter().any(|(n, _)| n == &name) {
        return;
    }
    set_header(out_headers, &name, &Uuid::new_v4().to_string());
}

fn parse_header_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Forwarding
// ---------------------------------------------------------------------------

async fn forward(
    state: &OagwState,
    target: &EndpointTarget,
    method: &Method,
    path: &str,
    query: &str,
    out_headers: &[(String, String)],
    body: &axum::body::Bytes,
    suffix: &str,
) -> Result<http::Response<Body>, http::Response<Body>> {
    let scheme = target.url_scheme.as_deref().unwrap_or("https");
    let mut url = format!("{}://{}:{}{}", scheme, target.host, target.port, path);
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }

    let mut builder = match *method {
        Method::GET => state.http.get(&url),
        Method::POST => state.http.post(&url),
        Method::PUT => state.http.put(&url),
        Method::PATCH => state.http.patch(&url),
        Method::DELETE => state.http.delete(&url),
        Method::HEAD => state.http.head(&url),
        Method::OPTIONS => state.http.options(&url),
        _ => state.http.post(&url),
    };
    for (name, value) in out_headers {
        builder = builder.header(name, value);
    }
    let resp = builder
        .body_bytes(body.clone())
        .send()
        .await
        .map_err(|e| map_forward_error(&e, suffix))?;
    let (parts, box_body) = resp.into_inner().into_parts();
    let response_body = axum::body::Body::new(box_body);
    Ok(http::Response::from_parts(parts, response_body))
}

fn map_forward_error(e: &toolkit_http::HttpError, suffix: &str) -> http::Response<Body> {
    use toolkit_http::HttpError;
    match e {
        HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => gateway_problem(
            StatusCode::GATEWAY_TIMEOUT,
            gts::ERR_REQUEST_TIMEOUT,
            "Request Timeout",
            format!("upstream request timed out: {e}"),
            suffix,
            None,
        ),
        HttpError::Transport(_) | HttpError::Tls(_) => gateway_problem(
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_LINK_UNAVAILABLE,
            "Link Unavailable",
            format!("could not reach upstream: {e}"),
            suffix,
            None,
        ),
        HttpError::Overloaded | HttpError::ServiceClosed => gateway_problem(
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_LINK_UNAVAILABLE,
            "Link Unavailable",
            format!("upstream link unavailable: {e}"),
            suffix,
            None,
        ),
        HttpError::InvalidUri { .. } | HttpError::InvalidScheme { .. } => gateway_problem(
            StatusCode::BAD_GATEWAY,
            gts::ERR_PROTOCOL_ERROR,
            "Protocol Error",
            format!("invalid upstream URL: {e}"),
            suffix,
            None,
        ),
        _ => gateway_problem(
            StatusCode::BAD_GATEWAY,
            gts::ERR_DOWNSTREAM_ERROR,
            "Downstream Error",
            format!("upstream request failed: {e}"),
            suffix,
            None,
        ),
    }
}
