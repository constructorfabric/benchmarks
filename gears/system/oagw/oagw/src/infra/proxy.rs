//! Data-plane proxy engine.
//!
//! Resolves `/v1/proxy/{alias}/{suffix}` requests through the tenant
//! hierarchy (descendant → root, closest alias wins with enforced
//! ancestors never bypassed), matches a route (longest path prefix),
//! validates guard rules (method, query allow-list, path suffix, CORS,
//! rate limits), applies auth/transform plugins, and forwards the
//! request to the selected endpoint with a bounded budget.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method};
use axum::response::Response;
use bytes::Bytes;
use toolkit_http::{HttpClientBuilder, HttpClientConfig};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::error::{OagwError, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM};
use crate::domain::model::{
    Endpoint, PassThrough, RateLimit, RateScope, RouteHttpMatch, RouteRecord, UpstreamRecord,
};
use crate::domain::services::{SCOPE_PROXY_INVOKE, scope_allowed};
use crate::infra::cors;
use crate::infra::plugins::PluginEngine;
use crate::infra::ratelimit::RateLimiter;
use crate::infra::storage::MemoryStore;

/// Hard payload ceiling (DESIGN: 100 MB) enforced before buffering.
pub const MAX_BODY_SIZE: usize = 100 * 1024 * 1024;

/// Hop-by-hop headers stripped per DESIGN's header table.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The shared data-plane engine.
pub struct DataPlane {
    store: Arc<MemoryStore>,
    tenants: Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
    plugins: Arc<PluginEngine>,
    limiter: Arc<RateLimiter>,
    config: Arc<OagwConfig>,
    http: toolkit_http::HttpClient,
    rr: Arc<AtomicUsize>,
}

impl DataPlane {
    /// Build the data plane.
    ///
    /// # Errors
    /// Returns an [`OagwError`] when the outbound HTTP client cannot be
    /// constructed (config-level TLS constraints).
    pub fn new(
        store: Arc<MemoryStore>,
        tenants: Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        plugins: Arc<PluginEngine>,
        config: Arc<OagwConfig>,
    ) -> Result<Self, OagwError> {
        let mut builder = HttpClientBuilder::with_config(HttpClientConfig::minimal());
        builder = builder
            .timeout(config.connect_timeout())
            .total_timeout(config.proxy_timeout())
            .max_body_size(MAX_BODY_SIZE);
        if !config.allow_http_upstream {
            builder = builder.deny_insecure_http();
        }
        let http = builder.build().map_err(|e| {
            OagwError::protocol_error(format!("failed to build outbound HTTP client: {e}"))
        })?;
        Ok(Self {
            store,
            tenants,
            plugins,
            limiter: Arc::new(RateLimiter::new()),
            config,
            http,
            rr: Arc::new(AtomicUsize::new(0)),
        })
    }

    // ==================================================================
    // Entry point
    // ==================================================================

    /// Execute a proxied request end to end.
    ///
    /// `raw_path` is the full URI path (`/proxy/{alias}[/{suffix}]`),
    /// still percent-encoded as received; `query` is the raw query
    /// string fragment.
    #[allow(clippy::too_many_arguments)]
    pub async fn proxy(
        &self,
        ctx: &SecurityContext,
        method: &Method,
        raw_path: &str,
        query: Option<&str>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response, OagwError> {
        // 1. Permission gate.
        if !scope_allowed(ctx.token_scopes(), SCOPE_PROXY_INVOKE) {
            return Err(OagwError::permission_denied(format!(
                "required scope `{SCOPE_PROXY_INVOKE}` is not present on the token"
            )));
        }

        // 2. Parse alias + suffix from the raw path.
        let (alias_token, suffix) = split_proxy_path(raw_path)?;
        let alias_key = alias::normalize(alias_token);
        if alias_key.is_empty() {
            return Err(OagwError::validation("proxy alias must not be empty"));
        }

        // 3. Resolve the upstream along the tenant chain (descendant→root).
        let chain = self.tenant_chain(ctx).await?;
        let mut resolved = None;
        for tenant_id in &chain {
            if let Some(upstream) = self.store.upstream_by_alias(*tenant_id, &alias_key) {
                resolved = Some((*tenant_id, upstream));
                break;
            }
        }
        let (owner_tenant, upstream) = resolved.ok_or_else(|| {
            OagwError::route_not_found(format!("no upstream matches alias `{alias_key}`"))
        })?;
        if !upstream.enabled {
            return Err(OagwError::link_unavailable(format!(
                "upstream `{}` (alias `{alias_key}`) is disabled",
                upstream.id
            )));
        }

        // 4. Method + body pre-validation.
        validate_method(method)?;
        validate_body(&headers, body.len())?;

        // 5. Route matching (longest path prefix within the owner tenant).
        let (route, fwd_path) = self
            .match_route(owner_tenant, &upstream, &suffix, query, method)?;

        // 6. Guards (upstream plugins first, then route plugins).
        let upstream_refs = upstream.plugin_refs();
        let route_refs = route.plugin_refs();
        self.plugins.apply_guards(&upstream_refs, &route_refs, &headers, &HeaderMap::new())?;

        // 7. CORS actual-request validation.
        cors::validate_actual_request(upstream.cors.as_ref(), &headers, method)?;

        // 8. Rate limiting (min across selected upstream, route, enforced ancestors).
        self.enforce_rate_limit(ctx, &chain, &upstream, &route, &headers)?;

        // 9. Select endpoint (X-OAGW-Target-Host or round-robin).
        let target_host = header_value(&headers, "x-oagw-target-host");
        let endpoint = self.select_endpoint(&upstream, target_host.as_deref())?;

        // 10. SSRF policy gate.
        self.enforce_ssrf(&endpoint)?;

        // 11. Build the forwarded request + run auth & request transforms.
        let mut fwd_headers = self.build_request_headers(&upstream, &headers);
        self.plugins
            .apply_auth(ctx, upstream.auth.as_ref(), &mut fwd_headers)
            .await?;
        let all_refs: Vec<_> = upstream_refs
            .iter()
            .chain(route_refs.iter())
            .cloned()
            .collect();
        let request_id = self
            .plugins
            .apply_transform_request(&all_refs, &mut fwd_headers)?;

        // 12. Forward and assemble the response.
        let url = build_upstream_url(&endpoint, &fwd_path);
        let response = self.send_upstream(method, &url, &fwd_headers, body).await?;
        let request_origin = headers.get("origin").and_then(|v| v.to_str().ok());
        self.assemble_response(response, &upstream, request_id.as_deref(), request_origin)
            .await
    }

    // ==================================================================
    // Resolution helpers
    // ==================================================================

    /// Tenant chain for `ctx.subject_tenant_id()`, descendant → root.
    async fn tenant_chain(&self, ctx: &SecurityContext) -> Result<Vec<Uuid>, OagwError> {
        let response = self
            .tenants
            .get_ancestors(
                ctx,
                tenant_resolver_sdk::TenantId(ctx.subject_tenant_id()),
                &tenant_resolver_sdk::GetAncestorsOptions {
                    barrier_mode: tenant_resolver_sdk::BarrierMode::Respect,
                },
            )
            .await
            .map_err(|e| OagwError::protocol_error(format!("tenant resolution failed: {e}")))?;
        let mut chain = vec![response.tenant.id.0];
        chain.extend(response.ancestors.into_iter().map(|a| a.id.0));
        Ok(chain)
    }

    /// Find the best matching enabled route for the upstream and the
    /// exact path to forward upstream.
    fn match_route(
        &self,
        owner_tenant: Uuid,
        upstream: &UpstreamRecord,
        suffix: &str,
        query: Option<&str>,
        method: &Method,
    ) -> Result<(RouteRecord, String), OagwError> {
        let mut candidates: Vec<(RouteRecord, String)> = Vec::new();
        for route in self.store.route_list(owner_tenant) {
            if route.upstream_id != upstream.id || !route.enabled {
                continue;
            }
            let Some(http_match) = &route.match_.http else {
                continue; // gRPC routes are not proxiable (phase 3).
            };
            if !http_match.accepts_method(method.as_str()) {
                continue;
            }
            validate_query_allowlist(http_match, query)?;
            let fwd = forward_path(http_match, suffix)?;
            candidates.push((route, fwd));
        }
        // Longest path prefix wins.
        candidates.sort_by(|a, b| {
            a.0.match_.http
                .as_ref()
                .map(|m| m.path.len())
                .cmp(&b.0.match_.http.as_ref().map(|m| m.path.len()))
        });
        candidates.into_iter().next().ok_or_else(|| {
            OagwError::route_not_found(format!(
                "no route matches `{} {}` for upstream `{}`",
                method.as_str(),
                suffix,
                upstream.id
            ))
        })
    }

    /// Effective rate limit across selected upstream, matched route and
    /// enforced ancestors: the strictest (minimum tps) wins.
    fn enforce_rate_limit(
        &self,
        ctx: &SecurityContext,
        chain: &[Uuid],
        upstream: &UpstreamRecord,
        route: &RouteRecord,
        headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        let mut candidates: Vec<RateLimit> = Vec::new();
        if let Some(rate) = &route.rate_limit {
            candidates.push(rate.clone());
        }
        if let Some(rate) = &upstream.rate_limit {
            candidates.push(rate.clone());
        }
        for tenant_id in &chain[1..] {
            if let Some(ancestor) = self.store.upstream_by_alias(*tenant_id, &upstream.alias) {
                if let Some(rate) = &ancestor.rate_limit {
                    if rate.sharing == crate::domain::model::SharingMode::Enforce {
                        candidates.push(rate.clone());
                    }
                }
            }
        }
        if candidates.is_empty() {
            return Ok(());
        }
        let effective = candidates
            .into_iter()
            .min_by(|a, b| a.tps().partial_cmp(&b.tps()).unwrap_or(std::cmp::Ordering::Equal))
            .expect("candidates is non-empty");
        let scope_key = rate_scope_key(effective.scope, ctx, route.id, headers);
        let now = now_millis();
        if let Err(retry_after) = self.limiter.check_and_take(&scope_key, &effective, now) {
            return Err(OagwError::rate_limit_exceeded(
                format!("rate limit exceeded ({} tokens/s sustained)", effective.tps()),
                retry_after,
            )
            .with_ctx("upstream_id", upstream.id.to_string()));
        }
        Ok(())
    }

    /// Choose the endpoint. `X-OAGW-Target-Host` selects by host for
    /// pools; otherwise round-robin. Common-suffix (derived-alias)
    /// pools require the header (DESIGN error table).
    fn select_endpoint<'a>(
        &self,
        upstream: &'a UpstreamRecord,
        target_host: Option<&str>,
    ) -> Result<&'a Endpoint, OagwError> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(OagwError::validation("upstream has no endpoints"));
        }
        let derived = alias::derive(endpoints);

        if let Some(target) = target_host {
            if !alias::is_valid_hostname(target) {
                return Err(OagwError::invalid_target_host(format!(
                    "`{target}` must be a bare hostname or IP (no port/path)"
                )));
            }
            for endpoint in endpoints {
                if endpoint.host.eq_ignore_ascii_case(target) {
                    return Ok(endpoint);
                }
            }
            return Err(OagwError::unknown_target_host(format!(
                "`{target}` does not match any configured endpoint of upstream `{}`",
                upstream.id
            )));
        }

        if endpoints.len() > 1 && derived.is_some() {
            // Common-suffix alias pool — the header is mandatory so the
            // routing decision is unambiguous.
            return Err(OagwError::missing_target_host(format!(
                "upstream `{}` is a common-suffix pool; supply an `X-OAGW-Target-Host` header",
                upstream.id
            )));
        }

        let index = self.rr.fetch_add(1, Ordering::Relaxed) % endpoints.len();
        Ok(&endpoints[index])
    }

    /// Basic SSRF gate: when the policy is enabled and an allow-list is
    /// configured, hosts outside it are refused.
    fn enforce_ssrf(&self, endpoint: &Endpoint) -> Result<(), OagwError> {
        let policy = &self.config.ssrf_policy;
        if !policy.enabled {
            return Ok(());
        }
        let host = alias::normalize(&endpoint.host);
        if !policy.allowed_hosts.is_empty()
            && !policy.allowed_hosts.iter().any(|a| a.eq_ignore_ascii_case(&host))
        {
            return Err(OagwError::validation(format!(
                "ssrf_policy: host `{host}` is not in allowed_hosts"
            )));
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            for cidr in &policy.allowed_cidrs {
                if cidr_contains(cidr, ip) {
                    return Ok(());
                }
            }
            if !policy.allowed_cidrs.is_empty() {
                return Err(OagwError::validation(format!(
                    "ssrf_policy: IP `{ip}` is not in any allowed_cidr"
                )));
            }
        }
        Ok(())
    }

    // ==================================================================
    // Request/response plumbing
    // ==================================================================

    /// Build the outbound header set from the inbound request plus the
    /// configured passthrough/transform rules.
    fn build_request_headers(&self, upstream: &UpstreamRecord, inbound: &HeaderMap) -> HeaderMap {
        let transforms = upstream.headers.as_ref().and_then(|h| h.request.as_ref());
        let passthrough = transforms.and_then(|t| t.passthrough).unwrap_or(PassThrough::None);
        let allowlist: Vec<String> = transforms
            .map(|t| t.passthrough_allowlist.clone())
            .unwrap_or_default();

        let mut out = HeaderMap::new();
        match passthrough {
            PassThrough::None => {}
            PassThrough::Allowlist => {
                for (name, value) in inbound {
                    if allowlist.iter().any(|a| a.eq_ignore_ascii_case(name.as_str())) {
                        out.append(name.clone(), value.clone());
                    }
                }
            }
            PassThrough::All => {
                for (name, value) in inbound {
                    if !is_blocked_inbound(name.as_str()) {
                        out.append(name.clone(), value.clone());
                    }
                }
            }
        }

        if let Some(t) = transforms {
            for (name, value) in &t.set {
                set_header(&mut out, name, value);
            }
            for (name, value) in &t.add {
                add_header(&mut out, name, value);
            }
            for name in &t.remove {
                out.remove(name);
            }
        }
        out
    }

    /// Forward the request to the selected upstream.
    async fn send_upstream(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<toolkit_http::HttpResponse, OagwError> {
        let builder = match *method {
            Method::GET => self.http.get(url),
            Method::POST => self.http.post(url),
            Method::PUT => self.http.put(url),
            Method::PATCH => self.http.patch(url),
            Method::DELETE => self.http.delete(url),
            Method::HEAD => self.http.head(url),
            Method::OPTIONS => self.http.options(url),
            _ => {
                return Err(OagwError::validation(format!(
                    "method `{method}` is not proxiable"
                )));
            }
        };
        let mut builder = builder;
        for (name, value) in headers {
            if let Ok(value) = value.to_str() {
                builder = builder.header(name.as_str(), value);
            }
        }
        if !body.is_empty() {
            builder = builder.body_bytes(body);
        }
        builder.send().await.map_err(map_proxy_error)
    }

    /// Assemble the gateway response from the upstream response.
    async fn assemble_response(
        &self,
        response: toolkit_http::HttpResponse,
        upstream: &UpstreamRecord,
        request_id: Option<&str>,
        request_origin: Option<&str>,
    ) -> Result<Response, OagwError> {
        let status = response.status();
        let mut out_headers = HeaderMap::new();
        for (name, value) in response.headers() {
            if !is_blocked_inbound(name.as_str()) && name != header::CONTENT_LENGTH {
                out_headers.append(name.clone(), value.clone());
            }
        }
        out_headers.insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );

        // Response header transforms.
        if let Some(transforms) = upstream.headers.as_ref().and_then(|h| h.response.as_ref()) {
            for (name, value) in &transforms.set {
                set_header(&mut out_headers, name, value);
            }
            for (name, value) in &transforms.add {
                add_header(&mut out_headers, name, value);
            }
            for name in &transforms.remove {
                out_headers.remove(name);
            }
        }

        cors::apply_response_headers(upstream.cors.as_ref(), &mut out_headers, request_origin);
        self.plugins.apply_transform_response(&mut out_headers, request_id);

        let body = response.bytes().await.map_err(map_proxy_error)?;
        let mut builder = Response::builder().status(status);
        for (name, value) in &out_headers {
            builder = builder.header(name, value);
        }
        builder
            .body(axum::body::Body::from(body))
            .map_err(|e| OagwError::protocol_error(format!("failed to build proxy response: {e}")))
    }
}

// =====================================================================
//                           Free helpers
// =====================================================================

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Split the proxy path into (alias, suffix). The suffix keeps its
/// leading slash when present.
fn split_proxy_path(raw_path: &str) -> Result<(&str, &str), OagwError> {
    let rest = raw_path
        .strip_prefix("/proxy/")
        .ok_or_else(|| OagwError::validation(format!("unexpected proxy path `{raw_path}`")))?;
    match rest.find('/') {
        Some(idx) => Ok((&rest[..idx], &rest[idx..])),
        None => Ok((rest, "")),
    }
}

fn validate_method(method: &Method) -> Result<(), OagwError> {
    const ALLOWED: [Method; 7] = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::HEAD,
        Method::OPTIONS,
    ];
    if ALLOWED.contains(method) {
        Ok(())
    } else {
        Err(OagwError::validation(format!(
            "method `{method}` is not supported by the proxy"
        )))
    }
}

/// Validate Content-Length / Transfer-Encoding / size rules (DESIGN
/// body validation table).
fn validate_body(headers: &HeaderMap, actual_len: usize) -> Result<(), OagwError> {
    let transfer_encoding = header_value(headers, header::TRANSFER_ENCODING.as_str());
    let content_length = header_value(headers, header::CONTENT_LENGTH.as_str());

    if let Some(te) = transfer_encoding {
        if !te.eq_ignore_ascii_case("chunked") {
            return Err(OagwError::validation(format!(
                "unsupported transfer encoding `{te}` (only chunked is supported)"
            )));
        }
        if content_length.is_some() {
            return Err(OagwError::validation(
                "request must not carry both Content-Length and Transfer-Encoding",
            ));
        }
    }
    if let Some(cl) = content_length {
        let Ok(size) = cl.parse::<u64>() else {
            return Err(OagwError::validation("invalid Content-Length header"));
        };
        if size > MAX_BODY_SIZE as u64 {
            return Err(OagwError::payload_too_large(format!(
                "request payload of {size} bytes exceeds the {MAX_BODY_SIZE}-byte limit"
            )));
        }
    }
    if actual_len > MAX_BODY_SIZE {
        return Err(OagwError::payload_too_large(format!(
            "request payload of {actual_len} bytes exceeds the {MAX_BODY_SIZE}-byte limit"
        )));
    }
    Ok(())
}

/// Reject any query parameter outside the route's allow-list. An empty
/// allow-list permits all query parameters.
fn validate_query_allowlist(
    http_match: &RouteHttpMatch,
    query: Option<&str>,
) -> Result<(), OagwError> {
    let Some(query) = query else { return Ok(()) };
    if http_match.query_allowlist.is_empty() {
        return Ok(());
    }
    for (name, _) in form_urlencoded::parse(query.as_bytes()) {
        if !http_match
            .query_allowlist
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&name))
        {
            return Err(OagwError::validation(format!(
                "query parameter `{name}` is not in the route's allow-list"
            )));
        }
    }
    Ok(())
}

/// Compute the upstream path from a route match + suffix.
fn forward_path(http_match: &RouteHttpMatch, suffix: &str) -> Result<String, OagwError> {
    let base = http_match.path.trim_end_matches('/');
    match http_match.path_suffix_mode {
        crate::domain::model::PathSuffixMode::Disabled => {
            if !suffix.is_empty() {
                return Err(OagwError::validation(
                    "path suffix is disabled for this route (path_suffix_mode=disabled)",
                ));
            }
            let path = if base.is_empty() { "/" } else { base };
            Ok(path.to_owned())
        }
        crate::domain::model::PathSuffixMode::Append => Ok(if base.is_empty() {
            if suffix.is_empty() {
                "/".to_owned()
            } else {
                suffix.to_owned()
            }
        } else if suffix.is_empty() {
            base.to_owned()
        } else {
            format!("{base}{suffix}")
        }),
    }
}

fn build_upstream_url(endpoint: &Endpoint, fwd_path: &str) -> String {
    format!(
        "{}://{}:{}{}",
        endpoint.scheme,
        endpoint.host,
        endpoint.resolved_port(),
        fwd_path
    )
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Headers never forwarded to the upstream (DESIGN header table):
/// hop-by-hop + routing headers.
fn is_blocked_inbound(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
        || lower == "host"
        || lower == "content-length"
        || lower == "x-oagw-target-host"
}

fn set_header(headers: &mut HeaderMap, name: &str, value: &serde_json::Value) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::try_from(name),
        HeaderValue::from_str(&value.to_string()),
    ) {
        headers.insert(name, value);
    }
}

fn add_header(headers: &mut HeaderMap, name: &str, value: &serde_json::Value) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::try_from(name),
        HeaderValue::from_str(&value.to_string()),
    ) {
        headers.append(name, value);
    }
}

/// Map outbound HttpError to the gateway error model.
fn map_proxy_error(err: toolkit_http::HttpError) -> OagwError {
    match err {
        toolkit_http::HttpError::Timeout(_) | toolkit_http::HttpError::DeadlineExceeded(_) => {
            OagwError::timeout_request(format!("upstream request timed out: {err}"))
        }
        toolkit_http::HttpError::Transport(src) => {
            OagwError::link_unavailable(format!("upstream unreachable: {src}"))
        }
        toolkit_http::HttpError::Tls(_) => {
            OagwError::protocol_error(format!("TLS failure talking to upstream: {err}"))
        }
        toolkit_http::HttpError::BodyTooLarge { limit, .. } => OagwError::protocol_error(
            format!("upstream response exceeds the {limit}-byte limit"),
        ),
        _ => OagwError::protocol_error(format!("upstream request failed: {err}")),
    }
}

/// Compute the rate-limiter scope key dimension. `Ip`-scoped counters
/// key on the left-most `X-Forwarded-For` hop.
fn rate_scope_key(
    scope: RateScope,
    ctx: &SecurityContext,
    route_id: Uuid,
    headers: &HeaderMap,
) -> String {
    match scope {
        RateScope::Global => "global".to_owned(),
        RateScope::Tenant => format!("tenant:{}", ctx.subject_tenant_id()),
        RateScope::User => format!("user:{}", ctx.subject_id()),
        RateScope::Ip => {
            let ip = header_value(headers, "x-forwarded-for")
                .and_then(|v| v.split(',').next().map(str::trim).map(str::to_owned));
            format!("ip:{}", ip.unwrap_or_else(|| "unknown".to_owned()))
        }
        RateScope::Route => format!("route:{route_id}"),
    }
}

/// `cidr` in `addr/prefix` form contains `ip` (IPv4 or IPv6). Pure std —
/// avoids pulling `ipnet` in as a direct dependency.
fn cidr_contains(cidr: &str, ip: IpAddr) -> bool {
    let (addr, prefix) = match cidr.split_once('/') {
        Some((addr, prefix)) => match prefix.parse::<u32>() {
            Ok(prefix) => (addr, prefix),
            Err(_) => return false,
        },
        None => (cidr, 32),
    };
    match (addr.parse::<IpAddr>(), ip) {
        (Ok(IpAddr::V4(net)), IpAddr::V4(ip)) => {
            let prefix = prefix.min(32);
            if prefix == 0 {
                return true;
            }
            let mask = u32::MAX << (32 - prefix);
            (u32::from(net) & mask) == (u32::from(ip) & mask)
        }
        (Ok(IpAddr::V6(net)), IpAddr::V6(ip)) => {
            let prefix = prefix.min(128);
            if prefix == 0 {
                return true;
            }
            let (net_a, net_b) = (u128::from(net), u128::from(ip));
            let shift = 128 - prefix;
            let mask = if shift >= 128 { 0 } else { u128::MAX << shift };
            (net_a & mask) == (net_b & mask)
        }
        _ => false,
    }
}
