//! The data plane: executes the full proxy flow described in DESIGN.md §
//! 3.5 (alias resolution → route matching → effective config → auth → guards
//! → transforms → rate limiting → forwarding → response processing) and
//! returns the streamed upstream response.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde_json::{json, Value};

use super::plugins::{
    classify_binding, ArcAuthPlugin, ArcGuardPlugin, ArcTransformPlugin, GuardDecision,
    RequestContext, ResponseContext,
};
use super::rate_limiter::{AcquireOutcome, RateLimiter};
use super::storage::Services;
use crate::domain::control_plane::{Caller, ControlPlaneService, UpstreamResolution};
use crate::domain::data_plane::{DataPlaneService, ProxyFailure, ProxyRequest, ProxyResponse};
use crate::domain::model::{
    AuthConfig, Endpoint, MatchConfig, PassthroughMode, PluginBinding, PluginKind, RateLimitConfig,
    RateLimitStrategy, SharingMode,
};
use crate::domain::plugins::PluginError;
use crate::error::{ErrorContext, OagwError};
use crate::gts_helpers;

/// Hop-by-hop and routing headers always stripped (DESIGN.md routing header
/// table).
const STRIP_HEADERS: &[&str] = &[
    "x-oagw-target-host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Wrap an error with the current request context.
fn fail(error: OagwError, ctx: ErrorContext) -> ProxyFailure {
    ProxyFailure::new(error, ctx)
}

/// Convert a header name string to a `HeaderName` (names are validated by
/// the control plane; fall back defensively in case an invalid one slips in).
fn header_name(name: &str) -> HeaderName {
    HeaderName::from_bytes(name.as_bytes())
        .unwrap_or_else(|_| HeaderName::from_static("x-oagw-invalid-header"))
}

#[async_trait]
impl DataPlaneService for Arc<Services> {
    async fn proxy_request(
        &self,
        caller: &Caller,
        alias: &str,
        request: ProxyRequest,
    ) -> Result<ProxyResponse, ProxyFailure> {
        let mut ctx = ErrorContext {
            alias: Some(alias.to_owned()),
            ..Default::default()
        };

        // 1. Proxy permission.
        if !caller.has_permission(gts_helpers::PERM_PROXY_INVOKE) {
            return Err(fail(
                OagwError::Forbidden {
                    detail: "missing permission gts.cf.core.oagw.proxy.v1~:invoke".into(),
                },
                ctx,
            ));
        }

        // 2. Alias resolution (404 / 503).
        let resolution = match self.resolve_alias(caller, alias).await {
            Ok(r) => r,
            Err(e) => return Err(fail(e, ctx)),
        };
        ctx.upstream_id = Some(resolution.upstream.id.to_string());

        // 3. CORS checks on actual (non-preflight) cross-origin requests.
        let effective_cors = self.effective_cors(&resolution);
        let request_origin = request
            .headers
            .get("origin")
            .and_then(|v| v.to_str().ok().map(str::to_owned));
        if effective_cors.enabled {
            if let Some(origin) = &request_origin {
                if let Err(e) = super::cors::check_origin(&effective_cors, origin) {
                    return Err(fail(e, ctx));
                }
                if let Err(e) = super::cors::check_method(&effective_cors, &request.method) {
                    return Err(fail(e, ctx));
                }
            }
        }

        // 4. Route matching.
        let query_keys: Vec<String> = request
            .raw_query
            .as_deref()
            .map(|q| {
                q.split('&')
                    .filter_map(|pair| pair.split_once('=').map(|(k, _)| k.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let route_res = match self
            .resolve_route(
                caller,
                &resolution,
                request.method.as_str(),
                &request.path_suffix,
                &query_keys,
            )
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(fail(
                    OagwError::not_found("no route matches this request"),
                    ctx,
                ));
            }
            Err(e) => return Err(fail(e, ctx)),
        };

        // 5. Effective configuration.
        let effective_auth = effective_auth(&resolution);
        let rate_limit = effective_rate(&resolution, route_res.route.rate_limit.as_ref());
        let merged_plugins = self.merged_plugins(&resolution, &route_res.route.plugins);

        // 6. Endpoint selection (round-robin or X-OAGW-Target-Host).
        let endpoints = &resolution.upstream.server.endpoints;
        let selected = match select_endpoint(self, &resolution, endpoints, &request.headers) {
            Ok(s) => s,
            Err(e) => return Err(fail(e, ctx)),
        };
        ctx.host = Some(selected.host.clone());

        // 7. Outbound request assembly (path + query).
        // Header transforms (passthrough / set / add / remove) are applied at
        // forward time — guards and transforms see the inbound request headers
        // (plus whatever the auth plugin injects), per DESIGN.md §3.5.
        let outbound_path = build_outbound_path(&request.path_suffix, &route_res.route.match_config);
        let outbound_query =
            build_outbound_query(request.raw_query.as_deref(), &route_res.query_allowlist);

        // 8. Auth (inject_credentials).
        let outbound_uri = {
            let mut uri = outbound_path.clone();
            if let Some(q) = &outbound_query {
                uri.push('?');
                uri.push_str(q);
            }
            uri
        };
        let mut pctx = RequestContext {
            tenant_id: caller.tenant_id,
            subject_id: caller.subject_id,
            scopes: caller.scopes.clone(),
            client_ip: request.client_ip,
            method: request.method.clone(),
            uri: outbound_uri,
            headers: request.headers.clone(),
            body: request.body.clone(),
            target_host: Some(selected.host.clone()),
            metadata: HashMap::new(),
            plugin_config: None,
        };
        if let Some(auth) = &effective_auth {
            let plugin = match resolve_auth_plugin(self, &auth.auth_type) {
                Ok(p) => p,
                Err(e) => return Err(fail(e, ctx)),
            };
            pctx.plugin_config = Some(auth.config.clone());
            if let Err(e) = plugin.authenticate(&mut pctx).await {
                return Err(fail(to_oagw(e), ctx));
            }
        }

        // 9. Guards (request phase).
        let guard_chain = match resolve_guards(&merged_plugins) {
            Ok(c) => c,
            Err(e) => return Err(fail(e, ctx)),
        };
        for (config, plugin) in guard_chain {
            let mut gctx = pctx.clone();
            gctx.plugin_config = config;
            match plugin.guard_request(&gctx) {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject(info)) => {
                    return Err(fail(
                        OagwError::GuardRejected {
                            status: info.status,
                            problem_type: info.problem_type,
                            detail: info.detail,
                        },
                        ctx,
                    ));
                }
                Err(e) => return Err(fail(to_oagw(e), ctx)),
            }
        }

        // 10. Transform (request phase). Transforms mutate headers/metadata,
        // so run them on a working copy and carry the accumulated changes
        // back into `pctx` — upstream transforms run before route transforms
        // and each sees the output of the previous one.
        let transform_chain = match resolve_transforms(&merged_plugins) {
            Ok(c) => c,
            Err(e) => return Err(fail(e, ctx)),
        };
        let mut tctx = pctx.clone();
        for (config, plugin) in transform_chain {
            tctx.plugin_config = config;
            if let Err(e) = plugin.transform_request(&mut tctx) {
                return Err(fail(to_oagw(e), ctx));
            }
        }
        pctx.headers = tctx.headers;
        pctx.metadata = tctx.metadata;

        // 11. Rate limiting (effective = min over the chain).
        if let Some(cfg) = &rate_limit {
            let scope_id = RateLimiter::scope_id(
                cfg,
                caller.tenant_id,
                caller.subject_id,
                request.client_ip,
                route_res.route.id,
            );
            let key = self
                .limiter
                .key_for("upstream", &resolution.upstream.id.to_string(), cfg, &scope_id);
            let outcome = self.limiter.try_acquire(&key, cfg, 1);
            if !outcome.allowed && cfg.strategy == RateLimitStrategy::Reject {
                let now = now_unix_secs();
                let reset = outcome.reset_unix_secs.max(now);
                return Err(fail(
                    OagwError::RateLimitExceeded {
                        detail: "rate limit exceeded".into(),
                        retry_after_seconds: reset.saturating_sub(now),
                        limit: outcome.limit,
                        remaining: outcome.remaining,
                        reset_epoch_secs: reset,
                    },
                    ctx,
                ));
            }
            if cfg.response_headers {
                record_acquire(&mut pctx.metadata, &outcome);
            }
        }

        // 12. Forward to upstream.
        // Header transforms run here (DESIGN.md §3.5): start from the
        // post-auth, post-transform headers, filter by passthrough mode, apply
        // set/add/remove, then strip routing headers and pin the Host.
        let req_headers = &resolution.upstream.headers.request;
        let mut outbound = HeaderMap::new();
        match req_headers.passthrough {
            PassthroughMode::None => {}
            PassthroughMode::Allowlist => {
                for name in &req_headers.passthrough_allowlist {
                    if let Some(v) = pctx.headers.get(name).and_then(|v| v.to_str().ok()) {
                        outbound.insert(header_name(name), header_value(v));
                    }
                }
            }
            PassthroughMode::All => {
                for (name, value) in pctx.headers.iter() {
                    if let Ok(s) = value.to_str() {
                        outbound.append(header_name(name.as_str()), header_value(s));
                    }
                }
            }
        }
        for (name, value) in req_headers.set.iter() {
            outbound.insert(header_name(name), header_value(value));
        }
        for (name, value) in req_headers.add.iter() {
            outbound.append(header_name(name), header_value(value));
        }
        for name in &req_headers.remove {
            outbound.remove(header_name(name));
        }
        strip_headers(&mut outbound);

        // Host header replaced by upstream authority.
        let authority = endpoint_authority(&selected.scheme, &selected.host, selected.port);
        outbound.insert("host", header_value(&authority));

        let url = build_url(&selected, &outbound_path, &outbound_query);
        let method = request.method.clone();
        let body = request.body.clone();
        let mut header_pairs: Vec<(String, String)> = Vec::new();
        for (name, value) in outbound.iter() {
            if let Ok(v) = value.to_str() {
                header_pairs.push((name.as_str().to_owned(), v.to_owned()));
            }
        }

        let mut builder = dispatch(&self.outgoing_client, &method, &url);
        builder = builder.headers(header_pairs);
        if !body.is_empty() {
            builder = builder.body_bytes(body);
        }

        let upstream_resp = match builder.send().await {
            Ok(r) => r,
            Err(e) => return Err(fail(map_http_error(e), ctx)),
        };

        let mut resp_headers: HeaderMap = upstream_resp.headers().clone();
        strip_headers(&mut resp_headers);

        // CORS response headers (actual cross-origin requests only).
        if effective_cors.enabled {
            super::cors::apply_response_headers(
                &effective_cors,
                request_origin.as_deref(),
                &mut resp_headers,
            );
        }

        // Response header transforms.
        let resp_cfg = &resolution.upstream.headers.response;
        for (name, value) in resp_cfg.set.iter() {
            resp_headers.insert(header_name(name), header_value(value));
        }
        for (name, value) in resp_cfg.add.iter() {
            resp_headers.append(header_name(name), header_value(value));
        }
        for name in &resp_cfg.remove {
            resp_headers.remove(header_name(name));
        }

        // 13. Guards + transforms (response phase).
        let mut rctx = ResponseContext {
            status: upstream_resp.status().as_u16(),
            headers: resp_headers,
            body_bytes: None,
            metadata: pctx.metadata,
            plugin_config: None,
        };
        let guard_chain = match resolve_guards(&merged_plugins) {
            Ok(c) => c,
            Err(e) => return Err(fail(e, ctx)),
        };
        for (config, plugin) in guard_chain {
            rctx.plugin_config = config;
            match plugin.guard_response(&rctx) {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject(info)) => {
                    return Err(fail(
                        OagwError::GuardRejected {
                            status: info.status,
                            problem_type: info.problem_type,
                            detail: info.detail,
                        },
                        ctx,
                    ));
                }
                Err(e) => return Err(fail(to_oagw(e), ctx)),
            }
        }
        let transform_chain = match resolve_transforms(&merged_plugins) {
            Ok(c) => c,
            Err(e) => return Err(fail(e, ctx)),
        };
        for (config, plugin) in transform_chain {
            rctx.plugin_config = config;
            if let Err(e) = plugin.transform_response(&mut rctx) {
                return Err(fail(to_oagw(e), ctx));
            }
        }

        // Response rate-limit headers from plugin scratch space (only when
        // the effective config asked for them).
        let emit_rate_headers = rate_limit
            .as_ref()
            .map(|c| c.response_headers)
            .unwrap_or(false);
        if emit_rate_headers {
            if let Some(limit) = json_str(&rctx.metadata, "rate_limit_limit") {
                rctx.headers.insert("x-ratelimit-limit", header_value(limit));
            }
            if let Some(remaining) = json_str(&rctx.metadata, "rate_limit_remaining") {
                rctx.headers
                    .insert("x-ratelimit-remaining", header_value(remaining));
            }
            if let Some(reset) = json_str(&rctx.metadata, "rate_limit_reset") {
                rctx.headers.insert("x-ratelimit-reset", header_value(reset));
            }
        }

        // 14. Passthrough with upstream error-source header.
        rctx.headers.insert(
            "x-oagw-error-source",
            HeaderValue::from_static("upstream"),
        );

        Ok(ProxyResponse {
            status: StatusCode::from_u16(rctx.status).unwrap_or(StatusCode::BAD_GATEWAY),
            headers: rctx.headers,
            body: upstream_resp.into_body(),
        })
    }
}

// ---------------------------------------------------------------------------
// Effective configuration
// ---------------------------------------------------------------------------

/// Effective auth: an ancestor with `sharing: enforce` always wins; otherwise
/// the selected upstream's own auth; otherwise the closest ancestor auth with
/// `sharing: inherit`.
fn effective_auth(resolution: &UpstreamResolution) -> Option<AuthConfig> {
    let mut enforced: Option<AuthConfig> = None;
    let mut inheritable: Option<AuthConfig> = None;
    // Ancestors arrive closest-first; iterate root-first so the closest
    // ancestor wins ties.
    for anc in resolution.ancestors.iter().rev() {
        if let Some(auth) = &anc.auth {
            match auth.sharing {
                SharingMode::Enforce => {
                    enforced = Some(auth.clone());
                }
                SharingMode::Inherit => {
                    if inheritable.is_none() {
                        inheritable = Some(auth.clone());
                    }
                }
                SharingMode::Private => {}
            }
        }
    }
    if let Some(e) = enforced {
        return Some(e);
    }
    if let Some(a) = &resolution.upstream.auth {
        return Some(a.clone());
    }
    inheritable
}

/// Effective rate limit: min per-second rate over [selected upstream,
/// non-private ancestors, route] (DESIGN.md: min always wins).
fn effective_rate<'a>(
    resolution: &'a UpstreamResolution,
    route_limit: Option<&'a RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let mut chain: Vec<&RateLimitConfig> = Vec::new();
    if let Some(l) = &resolution.upstream.rate_limit {
        chain.push(l);
    }
    for anc in &resolution.ancestors {
        if let Some(l) = &anc.rate_limit {
            if l.sharing != SharingMode::Private {
                chain.push(l);
            }
        }
    }
    if let Some(l) = route_limit {
        chain.push(l);
    }
    crate::domain::model::min_rate_limit(chain)
}

// ---------------------------------------------------------------------------
// Endpoint selection
// ---------------------------------------------------------------------------

/// Select the routing endpoint. A multi-endpoint pool with a derived
/// (common-suffix) alias requires `X-OAGW-Target-Host`; all other pools
/// round-robin.
fn select_endpoint(
    svc: &Services,
    resolution: &UpstreamResolution,
    endpoints: &[Endpoint],
    headers: &HeaderMap,
) -> Result<Endpoint, OagwError> {
    let derived = crate::domain::alias::compute_derived_alias(endpoints);
    let multi = endpoints.len() > 1;
    if multi && derived.is_some() && derived.as_deref() == Some(resolution.upstream.alias.as_str()) {
        let valid_hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();
        let value = headers
            .get("x-oagw-target-host")
            .map(|v| v.to_str().unwrap_or("").trim().to_owned())
            .filter(|v| !v.is_empty());
        let value = match value {
            Some(v) => v,
            None => {
                return Err(OagwError::MissingTargetHost {
                    detail: format!(
                        "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias. Valid hosts: {valid_hosts:?}"
                    ),
                    valid_hosts,
                });
            }
        };
        let has_port_or_path = value.contains('/') || value.contains(':');
        if has_port_or_path && value.parse::<std::net::IpAddr>().is_err() {
            return Err(OagwError::InvalidTargetHost {
                detail: format!(
                    "X-OAGW-Target-Host `{value}` must be a bare hostname or IP (no port or path). Valid hosts: {valid_hosts:?}"
                ),
                invalid_value: value.clone(),
                valid_hosts,
            });
        }
        match endpoints.iter().find(|e| e.host == value) {
            Some(ep) => Ok(ep.clone()),
            None => Err(OagwError::UnknownTargetHost {
                detail: format!(
                    "X-OAGW-Target-Host `{value}` does not match any configured endpoint. Valid hosts: {valid_hosts:?}"
                ),
                invalid_value: value,
                valid_hosts,
            }),
        }
    } else {
        let idx = svc
            .round_robin
            .fetch_add(1, Ordering::Relaxed)
            % endpoints.len();
        Ok(endpoints[idx].clone())
    }
}

// ---------------------------------------------------------------------------
// Plugin chain resolution
// ---------------------------------------------------------------------------

/// Resolve an auth plugin binding (builtin or custom UUID) to a runnable
/// instance. Custom plugins have no Starlark runtime in this milestone,
/// so they surface as `PluginNotFound` (503) on the data plane.
fn resolve_auth_plugin(svc: &Services, gts: &str) -> Result<ArcAuthPlugin, OagwError> {
    match svc.auth_plugins.resolve(gts) {
        Ok(p) => Ok(p),
        Err(OagwError::PluginNotFound { .. }) => Err(OagwError::PluginNotFound {
            detail: format!("custom auth plugin `{gts}` has no runtime in this milestone"),
        }),
        Err(e) => Err(e),
    }
}

/// Resolve the bound guard plugins of the merged chain. Unresolvable bindings
/// (custom UUID plugins without a runtime, unknown builtins) fail with 503.
fn resolve_guards(
    merged: &[PluginBinding],
) -> Result<Vec<(Option<Value>, ArcGuardPlugin)>, OagwError> {
    let mut out = Vec::new();
    for binding in merged {
        if classify_binding(&binding.plugin_ref) == Some(PluginKind::Guard) {
            let plugin = super::plugins::GuardPluginRegistry::resolve(&binding.plugin_ref)?;
            out.push((binding.config.clone(), plugin));
        }
    }
    Ok(out)
}

/// Resolve the bound transform plugins of the merged chain.
fn resolve_transforms(
    merged: &[PluginBinding],
) -> Result<Vec<(Option<Value>, ArcTransformPlugin)>, OagwError> {
    let mut out = Vec::new();
    for binding in merged {
        if classify_binding(&binding.plugin_ref) == Some(PluginKind::Transform) {
            let plugin = super::plugins::TransformPluginRegistry::resolve(&binding.plugin_ref)?;
            out.push((binding.config.clone(), plugin));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Assembly helpers
// ---------------------------------------------------------------------------

fn header_value(v: &str) -> HeaderValue {
    HeaderValue::from_str(v).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn strip_headers(headers: &mut HeaderMap) {
    for name in STRIP_HEADERS {
        headers.remove(*name);
    }
}

fn endpoint_authority(scheme: &str, host: &str, port: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    match crate::domain::alias::standard_port(scheme) {
        Some(std) if std == port => host,
        _ => format!("{host}:{port}"),
    }
}

fn build_outbound_path(path_suffix: &str, match_config: &MatchConfig) -> String {
    let route_path = match match_config {
        MatchConfig::Http(h) => h.path.trim().to_owned(),
        MatchConfig::Grpc(_) => String::new(),
    };
    if path_suffix.trim().is_empty() {
        if route_path.is_empty() {
            "/".to_owned()
        } else {
            route_path
        }
    } else {
        format!(
            "{}/{}",
            route_path.trim_end_matches('/'),
            path_suffix.trim_start_matches('/')
        )
    }
}

fn build_outbound_query(raw_query: Option<&str>, allowlist: &[String]) -> Option<String> {
    let allowed: Vec<&str> = raw_query?
        .split('&')
        .filter(|pair| {
            pair.split_once('=')
                .map(|(k, _)| allowlist.iter().any(|a| a == k))
                .unwrap_or(false)
        })
        .collect();
    if allowed.is_empty() {
        return None;
    }
    Some(allowed.join("&"))
}

fn build_url(endpoint: &Endpoint, path: &str, query: &Option<String>) -> String {
    let host = if endpoint.host.contains(':') && !endpoint.host.starts_with('[') {
        format!("[{}]", endpoint.host)
    } else {
        endpoint.host.clone()
    };
    let mut url = format!("{}://{}:{}{}", endpoint.scheme, host, endpoint.port, path);
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    url
}

fn dispatch(client: &toolkit_http::HttpClient, method: &Method, url: &str) -> toolkit_http::RequestBuilder {
    match *method {
        Method::GET => client.get(url),
        Method::POST => client.post(url),
        Method::PUT => client.put(url),
        Method::PATCH => client.patch(url),
        Method::DELETE => client.delete(url),
        _ => client.post(url),
    }
}

fn record_acquire(metadata: &mut HashMap<String, Value>, outcome: &AcquireOutcome) {
    metadata.insert("rate_limit_limit".to_owned(), json!(outcome.limit.to_string()));
    metadata.insert("rate_limit_remaining".to_owned(), json!(outcome.remaining.to_string()));
    metadata.insert("rate_limit_reset".to_owned(), json!(outcome.reset_unix_secs.to_string()));
}

fn json_str<'a>(metadata: &'a HashMap<String, Value>, key: &str) -> Option<&'a str> {
    metadata.get(key).and_then(|v| v.as_str())
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn to_oagw(e: PluginError) -> OagwError {
    match e {
        PluginError::AuthFailed(detail) => OagwError::AuthFailed { detail },
        PluginError::Config(detail) => OagwError::validation(detail),
        PluginError::Upstream(detail) => OagwError::DownstreamError { detail },
    }
}

fn map_http_error(e: toolkit_http::HttpError) -> OagwError {
    match e {
        toolkit_http::HttpError::Timeout(_) | toolkit_http::HttpError::DeadlineExceeded(_) => {
            OagwError::RequestTimeout {
                detail: "request to upstream timed out".into(),
            }
        }
        toolkit_http::HttpError::Transport(err) => OagwError::DownstreamError {
            detail: format!("upstream transport error: {err}"),
        },
        toolkit_http::HttpError::Tls(err) => OagwError::DownstreamError {
            detail: format!("upstream TLS error: {err}"),
        },
        toolkit_http::HttpError::BodyTooLarge { .. } => OagwError::PayloadTooLarge {
            detail: "upstream response body exceeds limit".into(),
        },
        toolkit_http::HttpError::Overloaded | toolkit_http::HttpError::ServiceClosed => {
            OagwError::LinkUnavailable {
                detail: "outgoing HTTP client is overloaded or unavailable".into(),
            }
        }
        _ => OagwError::DownstreamError {
            detail: "upstream request failed".into(),
        },
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::domain::control_plane::{Caller, ControlPlaneService};
    use crate::domain::model::{
        AuthConfig, CorsConfig, Endpoint, GrpcMatchConfig, HeadersConfig, HttpMatchConfig,
        MatchConfig, PathSuffixMode, PluginChainConfig, Protocol, RateLimitAlgorithm,
        RateLimitConfig, RateLimitScope, RateLimitWindow, ServerConfig, SharingMode,
        SustainedRate,
    };
    use std::sync::Arc;
    use uuid::Uuid;

    fn tenant() -> Uuid {
        Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap()
    }

    fn caller() -> Caller {
        Caller {
            tenant_id: tenant(),
            subject_id: tenant(),
            scopes: vec!["*".to_owned()],
        }
    }

    fn svc() -> Arc<Services> {
        Arc::new(Services::empty(OagwConfig::default(), tenant()))
    }

    fn http(path: &str) -> MatchConfig {
        MatchConfig::Http(HttpMatchConfig {
            methods: vec!["GET".to_owned()],
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        })
    }

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn outbound_path_assembly() {
        assert_eq!(build_outbound_path("users", &http("/v1")), "/v1/users");
        assert_eq!(build_outbound_path("/users/", &http("/v1/")), "/v1/users/");
        assert_eq!(build_outbound_path("", &http("/v1")), "/v1");
        assert_eq!(build_outbound_path("", &http("/")), "/");
        // Grpc match has no path; suffix alone becomes the path.
        assert_eq!(
            build_outbound_path(
                "Say.Hello",
                &MatchConfig::Grpc(GrpcMatchConfig {
                    service: "greet".to_owned(),
                    method: "Hello".to_owned(),
                })
            ),
            "/Say.Hello"
        );
    }

    #[test]
    fn outbound_query_allowlist() {
        let allowed = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(
            build_outbound_query(Some("a=1&b=2&c=3"), &allowed).as_deref(),
            Some("a=1&b=2")
        );
        // Nothing allowed -> None.
        assert_eq!(build_outbound_query(Some("c=3&d=4"), &allowed), None);
        // No query -> None.
        assert_eq!(build_outbound_query(None, &allowed), None);
    }

    #[test]
    fn url_and_authority_building() {
        let e = ep("http", "127.0.0.1", 9080);
        assert_eq!(build_url(&e, "/api/x", &None), "http://127.0.0.1:9080/api/x");
        assert_eq!(
            build_url(&e, "/api/x", &Some("a=1".to_owned())),
            "http://127.0.0.1:9080/api/x?a=1"
        );
        // IPv6 literal gets bracketed.
        let v6 = ep("http", "::1", 9080);
        assert_eq!(build_url(&v6, "/", &None), "http://[::1]:9080/");
        assert_eq!(endpoint_authority("https", "api.com", 443), "api.com");
        assert_eq!(endpoint_authority("https", "api.com", 8443), "api.com:8443");
        assert_eq!(endpoint_authority("http", "::1", 8080), "[::1]:8080");
    }

    #[test]
    fn hop_by_hop_headers_stripped() {
        let mut h = HeaderMap::new();
        for name in ["connection", "keep-alive", "transfer-encoding", "te", "upgrade"] {
            h.insert(name, HeaderValue::from_static("x"));
        }
        h.insert("x-keep-me", HeaderValue::from_static("y"));
        strip_headers(&mut h);
        assert!(h.is_empty() || h.len() == 1);
        assert!(!h.contains_key("connection"));
        assert!(!h.contains_key("transfer-encoding"));
        assert_eq!(h.get("x-keep-me").unwrap(), "y");
    }

    #[test]
    fn effective_auth_enforce_wins_over_upstream() {
        let mut anc = upstream("a1", "eapi.com");
        anc.auth = Some(auth(SharingMode::Enforce, "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.api.v1".to_owned()));
        let mut res = UpstreamResolution {
            upstream: upstream("u1", "api.com"),
            ancestors: vec![anc],
        };
        res.upstream.auth = Some(auth(SharingMode::Private, "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1".to_owned()));
        let eff = effective_auth(&res).expect("auth effective");
        assert_eq!(eff.auth_type, "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.api.v1");
    }

    #[test]
    fn effective_rate_takes_min_over_chain() {
        // upstream 10/s, ancestor (inherit) 5/s, route 3/s -> min 3/s.
        let mut anc = upstream("a1", "api.com");
        anc.rate_limit = Some(rl(5, SharingMode::Inherit));
        let mut res = UpstreamResolution {
            upstream: upstream("u1", "api.com"),
            ancestors: vec![anc],
        };
        res.upstream.rate_limit = Some(rl(10, SharingMode::Private));
        let eff = effective_rate(&res, Some(&rl(3, SharingMode::Private))).expect("rate effective");
        assert_eq!(eff.sustained.rate, 3);
        // Private ancestor is excluded.
        let mut res2 = UpstreamResolution {
            upstream: upstream("u1", "api.com"),
            ancestors: Vec::new(),
        };
        res2.upstream.rate_limit = Some(rl(7, SharingMode::Private));
        let mut anc_private = upstream("a1", "api.com");
        anc_private.rate_limit = Some(rl(1, SharingMode::Private));
        res2.ancestors.push(anc_private);
        let eff = effective_rate(&res2, None).expect("rate effective");
        assert_eq!(eff.sustained.rate, 7);
    }

    #[tokio::test]
    async fn select_endpoint_round_robins_and_requires_target_host_for_suffix_pools() {
        let s = svc();
        // Single endpoint: round robin returns it.
        let eps = vec![ep("http", "127.0.0.1", 9080)];
        let res = UpstreamResolution {
            upstream: upstream("u1", "tint"), // explicit alias, not derived
            ancestors: Vec::new(),
        };
        let sel = select_endpoint(&s, &res, &eps, &HeaderMap::new()).unwrap();
        assert_eq!(sel.host, "127.0.0.1");

        // Common-suffix pool requires X-OAGW-Target-Host.
        let pool = vec![ep("https", "a.api.com", 443), ep("https", "b.api.com", 443)];
        let res = UpstreamResolution {
            upstream: upstream("u1", "api.com"), // derived common suffix
            ancestors: Vec::new(),
        };
        assert!(matches!(
            select_endpoint(&s, &res, &pool, &HeaderMap::new()),
            Err(OagwError::MissingTargetHost { .. })
        ));
        let mut hdrs = HeaderMap::new();
        hdrs.insert("x-oagw-target-host", HeaderValue::from_static("b.api.com"));
        let sel = select_endpoint(&s, &res, &pool, &hdrs).unwrap();
        assert_eq!(sel.host, "b.api.com");
        let mut bad = HeaderMap::new();
        bad.insert("x-oagw-target-host", HeaderValue::from_static("nope.com"));
        assert!(matches!(
            select_endpoint(&s, &res, &pool, &bad),
            Err(OagwError::UnknownTargetHost { .. })
        ));
        // Round-robin rotates for non-derived multi pools.
        let res = UpstreamResolution {
            upstream: upstream("u1", "pool-explicit"),
            ancestors: Vec::new(),
        };
        let h1 = select_endpoint(&s, &res, &pool, &HeaderMap::new()).unwrap();
        let h2 = select_endpoint(&s, &res, &pool, &HeaderMap::new()).unwrap();
        assert_ne!(h1.host, h2.host);
    }

    fn upstream(id: &str, alias: &str) -> crate::domain::model::Upstream {
        crate::domain::model::Upstream {
            id: uuid_of(id),
            tenant_id: tenant(),
            enabled: true,
            alias: alias.to_owned(),
            tags: Vec::new(),
            server: ServerConfig { endpoints: Vec::new() },
            protocol: Protocol::Http,
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginChainConfig::default(),
            rate_limit: None,
            cors: CorsConfig::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn uuid_of(tag: &str) -> Uuid {
        Uuid::new_v5(&Uuid::NAMESPACE_OID, tag.as_bytes())
    }

    fn auth(sharing: SharingMode, auth_type: String) -> AuthConfig {
        AuthConfig {
            sharing,
            auth_type,
            config: serde_json::Value::Null,
        }
    }

    fn rl(rate: u64, sharing: SharingMode) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: RateLimitAlgorithm::default(),
            sustained: SustainedRate {
                rate,
                window: RateLimitWindow::default(),
            },
            burst: None,
            scope: RateLimitScope::default(),
            strategy: crate::domain::model::RateLimitStrategy::Reject,
            cost: 1,
            response_headers: false,
        }
    }
}
