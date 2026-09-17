//! Data-plane implementation: alias resolution → route matching → config
//! merge → plugin chain → forward → transform response (DESIGN §3.2,
//! ADR 0003/0004/0007/0008/0009).
//!
//! Pipeline (normative):
//!
//! ```text
//! chain → alias resolution (closest wins, disabled → 503)
//!     → route match (method + longest path prefix) → 404 if none
//!     → endpoint selection (multi-endpoint common-suffix → X-OAGW-Target-Host)
//!     → config merge (upstream < route < tenant)
//!     → authorizer (:invoke) → 403 if denied
//!     → rate limit (reject → 429 + Retry-After)
//!     → CORS (actual cross-origin) → 403 origin/method
//!     → Auth → Guards(request) → Transform(request)
//!     → forward (timeout → 504, transport error → 502)
//!     → Guards(response) → Transform(response) → header rules → CORS → return
//! ```

use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer};
use axum::body::Body;
use http::header::{HeaderName, HeaderValue, CONTENT_LENGTH, HOST};
use http::{HeaderMap, Method, Request, Response};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::{compute_derived_alias, is_valid_hostname, normalize_alias};
use crate::domain::dto::EffectiveHeaders;
use crate::domain::error::{DomainError, ProblemSpec};
use crate::domain::gts as g;
use crate::domain::model::{
    Endpoint, HeaderRules, PassthroughMode, PathSuffixMode, Route, Upstream,
};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, ProxyRequestView, ProxyResponseView, SecretResolver, TransformPlugin};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::services::data_plane::{DataPlaneService, ProxyInput};
use crate::domain::services::hierarchy::TenantHierarchy;
use crate::domain::services::merge::compute_effective;
use crate::gts::PROXY_RESOURCE;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::client::{OagwHttpClient, SharedHttpClient};
use crate::infra::proxy::cors::{apply_response_headers, check_actual, CorsDecision};
use crate::infra::proxy::ratelimit::{RateDecision, RateLimitManager};

/// Authorization action for the proxy endpoint.
const ACTION_INVOKE: &str = "invoke";

/// Headers never forwarded to (or from) upstreams.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "x-oagw-target-host",
];

/// Inbound request header used to select an endpoint of a multi-endpoint
/// common-suffix upstream.
const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// The data-plane service implementation.
pub struct DataPlaneServiceImpl {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    #[allow(dead_code)] // reserved for custom (Starlark) plugin resolution
    plugins: Arc<dyn PluginRepository>,
    hierarchy: Arc<dyn TenantHierarchy>,
    authz: PolicyEnforcer,
    auth_registry: Arc<AuthPluginRegistry>,
    guard_registry: Arc<GuardPluginRegistry>,
    transform_registry: Arc<TransformPluginRegistry>,
    secrets: Arc<dyn SecretResolver>,
    rate_limiter: Arc<RateLimitManager>,
    http_client: SharedHttpClient,
    config: OagwConfig,
}

impl DataPlaneServiceImpl {
    /// Build the data plane with its resolved dependencies.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        authz: PolicyEnforcer,
        auth_registry: Arc<AuthPluginRegistry>,
        guard_registry: Arc<GuardPluginRegistry>,
        transform_registry: Arc<TransformPluginRegistry>,
        secrets: Arc<dyn SecretResolver>,
        rate_limiter: Arc<RateLimitManager>,
        http_client: Arc<OagwHttpClient>,
        config: OagwConfig,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            hierarchy,
            authz,
            auth_registry,
            guard_registry,
            transform_registry,
            secrets,
            rate_limiter,
            http_client,
            config,
        }
    }

    /// Tenant chain (leaf → root).
    async fn chain(&self, tenant: Uuid) -> Result<Vec<Uuid>, ProblemSpec> {
        self.hierarchy
            .tenant_chain(tenant)
            .await
            .map_err(|e| e.to_problem())
    }

    /// Resolve the closest same-alias upstream across the tenant chain.
    /// Closest definition wins (shadowing); a disabled closest row is 503.
    async fn resolve_alias(
        &self,
        chain: &[Uuid],
        alias: &str,
    ) -> Result<(Arc<Upstream>, Vec<Arc<Upstream>>), ProblemSpec> {
        let mut selected: Option<Arc<Upstream>> = None;
        let mut ancestors: Vec<Arc<Upstream>> = Vec::new();
        for tenant in chain {
            if let Some(u) = self.upstreams.get_by_alias(*tenant, alias) {
                let u = Arc::new(u);
                match selected {
                    None => selected = Some(u),
                    Some(_) => ancestors.push(u),
                }
            }
        }
        let Some(selected) = selected else {
            return Err(DomainError::AliasNotFound(alias.to_owned()).to_problem());
        };
        if !selected.enabled {
            return Err(DomainError::UpstreamDisabled(alias.to_owned()).to_problem());
        }
        Ok((selected, ancestors))
    }

    /// Select the matching route: method allowlist + longest path prefix.
    async fn match_route(
        &self,
        chain: &[Uuid],
        upstream_id: Uuid,
        method: &Method,
        suffix: &str,
    ) -> Result<Option<Arc<Route>>, ProblemSpec> {
        let path = if suffix.is_empty() { "/" } else { suffix };
        let mut best: Option<(Arc<Route>, usize)> = None;
        for tenant in chain {
            for route in self.routes.list(*tenant) {
                if route.upstream_id != upstream_id || !route.enabled {
                    continue;
                }
                let Some(m) = route.match_.http.as_ref() else {
                    continue; // gRPC routes are rejected at creation.
                };
                let method_ok = m.methods.is_empty()
                    || m.methods
                        .iter()
                        .any(|wm| wm.eq_ignore_ascii_case(method.as_str()));
                if !method_ok {
                    continue;
                }
                let prefix_len = if m.path == "/" {
                    1
                } else if path.starts_with(m.path.as_str()) {
                    m.path.len()
                } else {
                    0
                };
                if prefix_len == 0 {
                    continue;
                }
                let candidate = Arc::new(route);
                let replace = match &best {
                    None => true,
                    Some((b, blen)) => {
                        prefix_len > *blen
                            || (prefix_len == *blen && candidate.priority > b.priority)
                    }
                };
                if replace {
                    best = Some((candidate, prefix_len));
                }
            }
        }
        Ok(best.map(|(r, _)| r))
    }

    /// Compute the upstream path for the route + caller path suffix.
    fn final_upstream_path(route: &Route, suffix: &str) -> Result<String, DomainError> {
        let Some(m) = route.match_.http.as_ref() else {
            return Ok("/".to_owned());
        };
        let base = m.path.clone();
        let canonical = if suffix.is_empty() { "/" } else { suffix };
        match m.path_suffix_mode {
            PathSuffixMode::Append => {
                let tail = canonical.strip_prefix(base.as_str()).unwrap_or(canonical);
                if tail.is_empty() {
                    Ok(base)
                } else if tail.starts_with('/') {
                    Ok(format!("{base}{tail}"))
                } else {
                    Ok(format!("{base}/{tail}"))
                }
            }
            PathSuffixMode::Disabled => {
                if suffix.is_empty() || suffix == "/" {
                    Ok(base)
                } else {
                    Err(DomainError::validation(
                        "path suffix is not allowed for this route (match.http.path_suffix_mode: disabled)",
                    ))
                }
            }
        }
    }

    /// Select the target endpoint, enforcing `X-OAGW-Target-Host` for
    /// multi-endpoint common-suffix upstreams (DESIGN §"Endpoint Selection").
    fn select_endpoint(
        upstream: &Upstream,
        alias: &str,
        request_headers: &HeaderMap,
    ) -> Result<Endpoint, DomainError> {
        let derived = compute_derived_alias(upstream.protocol, &upstream.server);
        let requires_target_host = upstream.server.endpoints.len() > 1
            && derived
                .as_ref()
                .is_some_and(|d| normalize_alias(d) == normalize_alias(alias));
        if !requires_target_host {
            return upstream
                .server
                .endpoints
                .first()
                .cloned()
                .ok_or_else(|| DomainError::Internal("upstream has no endpoints".to_owned()));
        }
        let raw = request_headers
            .get(TARGET_HOST_HEADER)
            .ok_or_else(|| missing_target_host(alias))?
            .to_str()
            .map_err(|_| invalid_target_host("target host header is not valid UTF-8"))?;

        let (host, port) = parse_authority(raw)
            .ok_or_else(|| invalid_target_host(format!("'{raw}' is not a valid host[:port]")))?;

        let matched = upstream.server.endpoints.iter().find(|e| {
            let host_eq = normalize_alias(&e.host) == normalize_alias(&host);
            match port {
                Some(p) => host_eq && e.port == p,
                None => host_eq,
            }
        });
        matched.cloned().ok_or_else(|| {
            unknown_target_host(
                upstream.alias.as_deref().unwrap_or(alias),
                format!("'{raw}' does not match any endpoint"),
            )
        })
    }

    /// Authorize `proxy:invoke`.
    async fn authorize_invoke(&self, ctx: &SecurityContext) -> Result<(), DomainError> {
        let req = AccessRequest::default().require_constraints(false);
        match self
            .authz
            .access_scope_with(ctx, &PROXY_RESOURCE, ACTION_INVOKE, None, &req)
            .await
        {
            Ok(_) => Ok(()),
            Err(EnforcerError::Denied { .. }) => Err(DomainError::AccessDenied(
                "proxy invoke denied".to_owned(),
            )),
            Err(e) => Err(DomainError::Internal(format!("authz evaluation failed: {e}"))),
        }
    }

    /// Resolve a guard binding to a runnable guard.
    fn resolve_guard(
        &self,
        binding: &crate::domain::dto::ActiveBinding,
    ) -> Result<Arc<dyn GuardPlugin>, ProblemSpec> {
        if binding.plugin_uuid.is_some() {
            return Err(custom_plugin_unavailable(&binding.plugin_ref));
        }
        self.guard_registry
            .get(&binding.plugin_ref)
            .ok_or_else(|| plugin_not_found(&binding.plugin_ref, "unknown or catalog-only guard"))
    }

    /// Resolve a transform binding to a runnable transform.
    fn resolve_transform(
        &self,
        binding: &crate::domain::dto::ActiveBinding,
    ) -> Result<Arc<dyn TransformPlugin>, ProblemSpec> {
        if binding.plugin_uuid.is_some() {
            return Err(custom_plugin_unavailable(&binding.plugin_ref));
        }
        self.transform_registry
            .get(&binding.plugin_ref)
            .ok_or_else(|| {
                plugin_not_found(&binding.plugin_ref, "unknown or catalog-only transform")
            })
    }

    /// Resolve the auth plugin named in the effective auth config. An empty
    /// auth type means "no credential injection" (noop).
    fn resolve_auth_plugin(&self, auth_type: &str) -> Result<Arc<dyn AuthPlugin>, ProblemSpec> {
        if auth_type.is_empty() {
            return self.auth_registry.get(g::AUTH_NOOP).ok_or_else(|| {
                plugin_not_found(g::AUTH_NOOP, "noop auth plugin not registered")
            });
        }
        self.auth_registry
            .get(auth_type)
            .ok_or_else(|| plugin_not_found(auth_type, "unknown or catalog-only auth plugin"))
    }

    /// Validate the endpoint scheme against the gear config.
    fn validate_endpoint_scheme(&self, endpoint: &Endpoint) -> Result<(), ProblemSpec> {
        if endpoint.scheme == "http" && !self.config.allow_http_upstream {
            return Err(DomainError::validation(
                "http upstreams are not allowed (oagw.allow_http_upstream: false)",
            )
            .to_problem());
        }
        if endpoint.scheme != "http" {
            return Err(ProblemSpec {
                gts_type: g::ERR_PROTOCOL_ERROR,
                status: 502,
                title: "Protocol Error",
                detail: format!(
                    "upstream scheme '{}' is not forwardable in this build (only http; TLS connectors are not wired)",
                    endpoint.scheme
                ),
                context: Vec::new(),
                retry_after_seconds: None,
            });
        }
        Ok(())
    }

    /// Enforce the SSRF policy for IP-literal hosts (hostname resolution is
    /// out of scope for this build — see deviations).
    fn enforce_ssrf(&self, endpoint: &Endpoint) -> Result<(), ProblemSpec> {
        if !self.config.ssrf_policy.enabled {
            return Ok(());
        }
        if let Ok(ip) = endpoint.host.parse::<IpAddr>() {
            if is_private_ip(ip) {
                return Err(DomainError::validation(format!(
                    "ssrf policy blocks requests to private address '{}'",
                    endpoint.host
                ))
                .to_problem());
            }
        } else if endpoint.host.eq_ignore_ascii_case("localhost") {
            return Err(DomainError::validation(
                "ssrf policy blocks requests to localhost",
            )
            .to_problem());
        }
        Ok(())
    }
}

fn missing_target_host(alias: &str) -> DomainError {
    DomainError::problem(ProblemSpec {
        gts_type: g::ERR_MISSING_TARGET_HOST,
        status: 400,
        title: "Missing Target Host",
        detail: format!(
            "multi-endpoint upstream '{alias}' requires the 'x-oagw-target-host' header"
        ),
        context: vec![("alias".to_owned(), alias.to_owned())],
        retry_after_seconds: None,
    })
}

fn invalid_target_host(detail: impl Into<String>) -> DomainError {
    DomainError::problem(ProblemSpec {
        gts_type: g::ERR_INVALID_TARGET_HOST,
        status: 400,
        title: "Invalid Target Host",
        detail: detail.into(),
        context: Vec::new(),
        retry_after_seconds: None,
    })
}

fn unknown_target_host(alias: &str, detail: impl Into<String>) -> DomainError {
    DomainError::problem(ProblemSpec {
        gts_type: g::ERR_UNKNOWN_TARGET_HOST,
        status: 400,
        title: "Unknown Target Host",
        detail: detail.into(),
        context: vec![("alias".to_owned(), alias.to_owned())],
        retry_after_seconds: None,
    })
}

fn custom_plugin_unavailable(plugin_ref: &str) -> ProblemSpec {
    ProblemSpec {
        gts_type: g::ERR_PLUGIN_NOT_FOUND,
        status: 503,
        title: "Plugin Not Found",
        detail: format!(
            "custom plugin '{plugin_ref}' is not executable in this build (Starlark sandbox not wired)"
        ),
        context: Vec::new(),
        retry_after_seconds: None,
    }
}

fn plugin_not_found(plugin_ref: &str, why: &str) -> ProblemSpec {
    ProblemSpec {
        gts_type: g::ERR_PLUGIN_NOT_FOUND,
        status: 503,
        title: "Plugin Not Found",
        detail: format!("{why}: '{plugin_ref}'"),
        context: Vec::new(),
        retry_after_seconds: None,
    }
}

fn map_transport_error(
    e: &crate::infra::proxy::client::HttpSendError,
    upstream: &Upstream,
    endpoint: &Endpoint,
) -> ProblemSpec {
    ProblemSpec {
        gts_type: g::ERR_DOWNSTREAM_ERROR,
        status: 502,
        title: "Bad Gateway",
        detail: format!(
            "error contacting upstream '{}' at {}:{}: {e}",
            upstream.alias.as_deref().unwrap_or("?"),
            endpoint.host,
            endpoint.port
        ),
        context: vec![("upstream_id".to_owned(), upstream.id.to_string())],
        retry_after_seconds: Some(1),
    }
}

/// Parse `host[:port]` (IPv6 `[::1]:80` tolerated).
fn parse_authority(value: &str) -> Option<(String, Option<u16>)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    // Bracketed IPv6: `[::1]` or `[::1]:80`.
    if let Some(rest) = value.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let host = host.to_owned();
        if !is_valid_hostname(&host) && host.parse::<IpAddr>().is_err() {
            return None;
        }
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None => None,
        };
        return Some((host, port));
    }
    // Raw IPv6 without brackets.
    if value.parse::<std::net::Ipv6Addr>().is_ok() {
        return Some((value.to_owned(), None));
    }
    // Plain host[:port].
    if value.contains(':') {
        let (h, p) = value.rsplit_once(':')?;
        let port = p.parse::<u16>().ok()?;
        return Some((h.to_owned(), Some(port)));
    }
    if !is_valid_hostname(value) && value.parse::<IpAddr>().is_err() {
        return None;
    }
    Some((value.to_owned(), None))
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || {
            // fc00::/7 (ULA) and fe80::/10 (link-local)
            let first = v6.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        },
    }
}

fn host_authority(endpoint: &Endpoint) -> String {
    // Bracketed IPv6 host for URI building.
    let host = if endpoint.host.contains(':') && !endpoint.host.starts_with('[') {
        format!("[{}]", endpoint.host)
    } else {
        endpoint.host.clone()
    };
    if endpoint.port == 80 || endpoint.port == 443 {
        host
    } else {
        format!("{host}:{}", endpoint.port)
    }
}

fn path_and_query(path: &str, query: &str) -> String {
    let path = if path.is_empty() {
        "/".to_owned()
    } else if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    }
}

/// Copy inbound headers per passthrough mode then apply set/add/remove.
fn build_outbound_headers(view: &ProxyRequestView, effective: &EffectiveHeaders) -> HeaderMap {
    let mut out = HeaderMap::new();
    match effective.passthrough {
        PassthroughMode::None => {}
        PassthroughMode::Allowlist => {
            for name in &effective.passthrough_allowlist {
                let Ok(hname) = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()) else {
                    continue;
                };
                for value in view.headers.get_all(&hname).iter() {
                    out.append(hname.clone(), value.clone());
                }
            }
        }
        PassthroughMode::All => {
            for (name, value) in view.headers.iter() {
                out.append(name.clone(), value.clone());
            }
        }
    }
    apply_rules(&mut out, &effective.request);
    for hop in HOP_BY_HOP {
        if let Ok(hname) = HeaderName::from_bytes(hop.as_bytes()) {
            out.remove(&hname);
        }
    }
    out
}

/// apply set/add/remove for one direction.
fn apply_rules(map: &mut HeaderMap, rules: &HeaderRules) {
    for name in &rules.remove {
        if let Ok(hname) = HeaderName::from_bytes(name.as_bytes()) {
            map.remove(&hname);
        }
    }
    for (name, value) in &rules.set {
        let (Ok(hname), Ok(hv)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        map.insert(hname, hv);
    }
    for (name, value) in &rules.add {
        let (Ok(hname), Ok(hv)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        map.append(hname, hv);
    }
}

/// Apply response header rules to the upstream response headers.
fn apply_response_rules(inbound: &HeaderMap, rules: &HeaderRules) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound.iter() {
        out.append(name.clone(), value.clone());
    }
    apply_rules(&mut out, rules);
    for hop in HOP_BY_HOP {
        if let Ok(hname) = HeaderName::from_bytes(hop.as_bytes()) {
            out.remove(&hname);
        }
    }
    out
}

/// Validate query params against the route allowlist ("reject unknown").
fn filter_query(route: &Route, view: &mut ProxyRequestView) -> Result<(), DomainError> {
    let Some(m) = route.match_.http.as_ref() else {
        return Ok(());
    };
    if m.query_allowlist.is_empty() {
        return if view.query.is_empty() {
            Ok(())
        } else {
            Err(DomainError::validation(
                "unknown query parameter: this route allows no query parameters",
            ))
        };
    }
    for part in view.query.split('&') {
        let key = part.split_once('=').map_or(part, |(k, _)| k);
        if key.is_empty() {
            continue;
        }
        if !m.query_allowlist.iter().any(|a| a == key) {
            return Err(DomainError::validation(format!(
                "unknown query parameter '{key}' (route allows only: {})",
                m.query_allowlist.join(", ")
            )));
        }
    }
    Ok(())
}

#[async_trait]
impl DataPlaneService for DataPlaneServiceImpl {
    async fn execute_proxy(
        &self,
        security_ctx: SecurityContext,
        input: ProxyInput,
    ) -> Result<Response<Body>, ProblemSpec> {
        let tenant = security_ctx.subject_tenant_id();
        let alias = normalize_alias(&input.alias);

        // The axum `{*path_suffix}` wildcard captures the suffix WITHOUT a
        // leading slash (e.g. "v1/ping" for /proxy/{alias}/v1/ping), while the
        // route matcher and upstream path builder expect a slash-leading path
        // (DESIGN: "Append to match.http.path" / "longest path prefix match").
        // Restore the separator slash here so the whole data plane speaks one
        // path convention.
        let mut input = input;
        if !input.path_suffix.is_empty() && !input.path_suffix.starts_with('/') {
            input.path_suffix = format!("/{}", input.path_suffix);
        }

        // Body size guard (413).
        if let Some(len) = input
            .request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
        {
            if len > self.config.body_limit_bytes {
                return Err(ProblemSpec {
                    gts_type: g::ERR_PAYLOAD_TOO_LARGE,
                    status: 413,
                    title: "Payload Too Large",
                    detail: format!(
                        "request body of {len} bytes exceeds the {}-byte limit",
                        self.config.body_limit_bytes
                    ),
                    context: Vec::new(),
                    retry_after_seconds: None,
                });
            }
        }

        // 1. Tenant chain.
        let chain = self.chain(tenant).await?;

        // 2. Alias resolution (closest wins; disabled → 503).
        let (selected, ancestors) = self.resolve_alias(&chain, &alias).await?;

        // 3. Route matching (method + longest path prefix; 404 if none).
        let route = self
            .match_route(&chain, selected.id, input.request.method(), &input.path_suffix)
            .await?;
        let Some(route) = route else {
            return Err(DomainError::RouteNotFound(
                alias.clone(),
                input.request.method().as_str().to_owned(),
                input.path_suffix.clone(),
            )
            .to_problem());
        };

        // 4. Endpoint selection (`X-OAGW-Target-Host`); scheme + SSRF policy.
        let endpoint =
            Self::select_endpoint(&selected, &alias, input.request.headers()).map_err(|e| e.to_problem())?;
        self.validate_endpoint_scheme(&endpoint)?;
        self.enforce_ssrf(&endpoint)?;

        // 5. Effective config (upstream < route < tenant).
        let effective = compute_effective(&selected, &ancestors, Some(&route));

        // 6. Authorizer: proxy `:invoke` required.
        self.authorize_invoke(&security_ctx)
            .await
            .map_err(|e| e.to_problem())?;

        // 7. Rate limiting (reject → 429 + Retry-After).
        let rate_limit = effective.rate_limit.clone();
        if let Some(rl) = &rate_limit {
            let ip_str = input.client_ip.map(|ip| ip.to_string());
            if let RateDecision::Rejected { retry_after_secs } =
                self.rate_limiter.check(rl, tenant, ip_str.as_deref())
            {
                return Err(ProblemSpec {
                    gts_type: g::ERR_RATE_LIMIT_EXCEEDED,
                    status: 429,
                    title: "Rate Limit Exceeded",
                    detail: "request exceeds the configured rate limit".to_owned(),
                    context: vec![
                        ("limit".to_owned(), rl.capacity.to_string()),
                        ("cost".to_owned(), rl.cost.to_string()),
                    ],
                    retry_after_seconds: Some(retry_after_secs),
                });
            }
        }

        // 8. CORS (actual cross-origin requests).
        if let Some(cors) = &effective.cors {
            if let CorsDecision::Rejected { status, gts_type, detail } =
                check_actual(cors, input.request.method(), input.request.headers())
            {
                return Err(ProblemSpec {
                    gts_type,
                    status,
                    title: "Forbidden",
                    detail,
                    context: Vec::new(),
                    retry_after_seconds: None,
                });
            }
        }

        // 9. Plugin chain — Auth → Guards(request) → Transform(request).
        let mut view = ProxyRequestView {
            method: input.request.method().clone(),
            path: Self::final_upstream_path(&route, &input.path_suffix)
                .map_err(|e| e.to_problem())?,
            query: input
                .request
                .uri()
                .query()
                .unwrap_or_default()
                .to_owned(),
            headers: input.request.headers().clone(),
            tenant_id: tenant,
            body_length_hint: input
                .request
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok()),
        };
        filter_query(&route, &mut view).map_err(|e| e.to_problem())?;

        let auth_plugin = self.resolve_auth_plugin(&effective.auth.auth_type)?;
        auth_plugin
            .inject_credentials(&mut view, &effective.auth.config, &*self.secrets)
            .await
            .map_err(ProblemSpec::from)?;

        for binding in &effective.guards {
            let guard = self.resolve_guard(binding)?;
            if !guard.checks_request() {
                continue;
            }
            guard
                .check_request(&view, &binding.config)
                .await
                .map_err(ProblemSpec::from)?;
        }

        for binding in &effective.transforms {
            let transform = self.resolve_transform(binding)?;
            transform
                .on_request(&mut view, &binding.config)
                .await
                .map_err(ProblemSpec::from)?;
        }

        // 10. Forward.
        let mut outbound_headers = build_outbound_headers(&view, &EffectiveHeaders::from(&effective));
        let authority = host_authority(&endpoint);
        if let Ok(v) = HeaderValue::from_str(&authority) {
            outbound_headers.insert(HOST, v);
        }
        let uri = format!(
            "http://{authority}{}",
            path_and_query(&view.path, &view.query)
        )
        .parse::<http::Uri>()
        .map_err(|e| DomainError::Internal(format!("invalid upstream URI: {e}")).to_problem())?;

        // Inbound headers retained for the response-phase CORS application;
        // the request body itself is moved into the outbound forward.
        let inbound_headers = input.request.headers().clone();
        let body = input.request.into_body();
        let mut outbound = Request::builder()
            .method(view.method.clone())
            .uri(uri)
            .body(body)
            .expect("request build");
        if let Some(len) = view.body_length_hint {
            outbound_headers.insert(CONTENT_LENGTH, HeaderValue::from_str(&len.to_string()).expect("len"));
        }
        *outbound.headers_mut() = outbound_headers.clone();
        let request_id = outbound_headers.get("x-request-id").cloned();

        let proxied = tokio::time::timeout(
            self.config.proxy_timeout(),
            self.http_client.send(outbound),
        )
        .await;

        let mut upstream_response = match proxied {
            Err(_elapsed) => {
                return Err(ProblemSpec {
                    gts_type: g::ERR_TIMEOUT_REQUEST,
                    status: 504,
                    title: "Gateway Timeout",
                    detail: format!(
                        "upstream did not respond within {:.0}s",
                        self.config.proxy_timeout_secs
                    ),
                    context: vec![("upstream_id".to_owned(), selected.id.to_string())],
                    retry_after_seconds: None,
                });
            }
            Ok(Err(e)) => return Err(map_transport_error(&e, &selected, &endpoint)),
            Ok(Ok(resp)) => resp,
        };

        // 11. Guards + transforms (response phase).
        let body_len = upstream_response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        {
            let mut view = ProxyResponseView {
                status: upstream_response.status(),
                headers: upstream_response.headers().clone(),
                body_len,
            };
            for binding in &effective.guards {
                let guard = self.resolve_guard(binding)?;
                if !guard.checks_response() {
                    continue;
                }
                guard
                    .check_response(&view, &binding.config)
                    .await
                    .map_err(ProblemSpec::from)?;
            }
            for binding in &effective.transforms {
                let transform = self.resolve_transform(binding)?;
                transform
                    .on_response(&mut view, &binding.config)
                    .await
                    .map_err(ProblemSpec::from)?;
            }
            let mut rebuilt = Response::builder()
                .status(view.status)
                .body(upstream_response.into_body())
                .expect("response rebuild");
            rebuilt.headers_mut().extend(view.headers);
            upstream_response = rebuilt;
        }

        // 12. Response header rules + CORS response headers + request id.
        let mut headers = apply_response_rules(
            upstream_response.headers(),
            &effective.headers.response,
        );
        if let Some(cors) = &effective.cors {
            let mut view = ProxyResponseView {
                status: upstream_response.status(),
                headers: headers.clone(),
                body_len,
            };
            apply_response_headers(cors, &inbound_headers, &mut view);
            headers = view.headers;
        }
        if let Some(rid) = request_id {
            headers
                .entry("x-request-id")
                .or_insert(rid);
        }
        if let Some(rl) = rate_limit.as_ref() {
            headers.insert(
                "x-ratelimit-limit",
                HeaderValue::from_str(&rl.capacity.to_string()).expect("numeric header"),
            );
            let remaining = (rl.capacity.max(0.0) as u64).max(0);
            headers.insert(
                "x-ratelimit-remaining",
                HeaderValue::from_str(&remaining.to_string()).expect("numeric header"),
            );
            headers.insert(
                "x-ratelimit-reset",
                HeaderValue::from_str(&rl.window_secs.to_string()).expect("numeric header"),
            );
        }

        let mut response = Response::builder()
            .status(upstream_response.status())
            .body(upstream_response.into_body())
            .expect("response rebuild");
        response.headers_mut().extend(headers);
        // ADR 0007: every proxied response (success or upstream-originated
        // error passthrough) is tagged with its source. Upstream responses
        // are forwarded unchanged, so they are marked `upstream`.
        response
            .headers_mut()
            .insert("x-oagw-error-source", HeaderValue::from_static("upstream"));
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::EffectiveHeaders;
    use crate::domain::model::{HttpMatch, PathSuffixMode, ServerConfig};
    use crate::domain::plugin::ProxyRequestView;

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: ServerConfig { endpoints },
            protocol: crate::domain::model::Protocol::Http,
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            bound: false,
        }
    }

    fn headers() -> HeaderMap {
        HeaderMap::new()
    }

    fn problem_of(err: DomainError) -> ProblemSpec {
        err.to_problem()
    }

    // -----------------------------------------------------------------------
    // select_endpoint: X-OAGW-Target-Host enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn single_endpoint_needs_no_target_host_header() {
        let u = upstream(
            "api.openai.com",
            vec![ep("https", "api.openai.com", 443)],
        );
        let e = DataPlaneServiceImpl::select_endpoint(&u, "api.openai.com", &headers()).unwrap();
        assert_eq!(e.host, "api.openai.com");
    }

    #[test]
    fn single_endpoint_alias_mismatch_still_selects_first() {
        // A single endpoint never requires the header, even when the alias
        // does not match the endpoint host (e.g. an explicit alias).
        let u = upstream("my-service", vec![ep("http", "10.0.1.1", 80)]);
        let e = DataPlaneServiceImpl::select_endpoint(&u, "my-service", &headers()).unwrap();
        assert_eq!(e.host, "10.0.1.1");
    }

    #[test]
    fn multi_endpoint_common_suffix_requires_target_host() {
        let u = upstream(
            "vendor.com",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        let err =
            DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &headers()).unwrap_err();
        let p = problem_of(err);
        assert_eq!(p.status, 400);
        assert_eq!(p.gts_type, g::ERR_MISSING_TARGET_HOST);
        assert!(p.detail.contains("vendor.com"));
    }

    #[test]
    fn multi_endpoint_target_host_matches_endpoint() {
        let u = upstream(
            "vendor.com",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        let mut h = headers();
        h.insert(TARGET_HOST_HEADER, HeaderValue::from_static("us.vendor.com"));
        let e = DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &h).unwrap();
        assert_eq!(e.host, "us.vendor.com");

        // Host + explicit port form selects the second endpoint.
        let mut h = headers();
        h.insert(
            TARGET_HOST_HEADER,
            HeaderValue::from_static("eu.vendor.com:443"),
        );
        let e = DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &h).unwrap();
        assert_eq!(e.host, "eu.vendor.com");
    }

    #[test]
    fn multi_endpoint_matching_is_case_insensitive() {
        let u = upstream(
            "vendor.com",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        let mut h = headers();
        h.insert(TARGET_HOST_HEADER, HeaderValue::from_static("US.VENDOR.COM."));
        let e = DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &h).unwrap();
        assert_eq!(e.host, "us.vendor.com");
    }

    #[test]
    fn multi_endpoint_invalid_target_host_rejected() {
        let u = upstream(
            "vendor.com",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        // Not a valid host[:port] (whitespace + slashes).
        let mut h = headers();
        h.insert(
            TARGET_HOST_HEADER,
            HeaderValue::from_static("us vendor.com/extra"),
        );
        let err =
            DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &h).unwrap_err();
        let p = problem_of(err);
        assert_eq!(p.status, 400);
        assert_eq!(p.gts_type, g::ERR_INVALID_TARGET_HOST);
    }

    #[test]
    fn multi_endpoint_non_utf8_target_host_rejected() {
        let u = upstream(
            "vendor.com",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        let mut h = HeaderMap::new();
        h.append(
            TARGET_HOST_HEADER,
            http::HeaderValue::from_bytes(&[0x70, 0xC8]).expect("obs-text value"),
        );
        let err =
            DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &h).unwrap_err();
        let p = problem_of(err);
        assert_eq!(p.status, 400);
        assert_eq!(p.gts_type, g::ERR_INVALID_TARGET_HOST);
    }

    #[test]
    fn multi_endpoint_unknown_target_host_rejected() {
        let u = upstream(
            "vendor.com",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        let mut h = headers();
        // Valid authority, but no matching endpoint.
        h.insert(
            TARGET_HOST_HEADER,
            HeaderValue::from_static("ap.vendor.com"),
        );
        let err =
            DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &h).unwrap_err();
        let p = problem_of(err);
        assert_eq!(p.status, 400);
        assert_eq!(p.gts_type, g::ERR_UNKNOWN_TARGET_HOST);
        assert!(p.detail.contains("ap.vendor.com"));
    }

    #[test]
    fn multi_endpoint_port_mismatch_is_unknown() {
        let u = upstream(
            "vendor.com",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        let mut h = headers();
        h.insert(
            TARGET_HOST_HEADER,
            HeaderValue::from_static("us.vendor.com:8443"),
        );
        let err =
            DataPlaneServiceImpl::select_endpoint(&u, "vendor.com", &h).unwrap_err();
        assert_eq!(problem_of(err).gts_type, g::ERR_UNKNOWN_TARGET_HOST);
    }

    #[test]
    fn ip_pool_selects_first_endpoint_without_header() {
        // Non-derivable (IP) pools never require X-OAGW-Target-Host — the
        // first endpoint is used (round-robin selection is not implemented
        // in this build).
        let u = upstream(
            "my-service",
            vec![ep("http", "10.0.1.1", 80), ep("http", "10.0.1.2", 80)],
        );
        let e = DataPlaneServiceImpl::select_endpoint(&u, "my-service", &headers()).unwrap();
        assert_eq!(e.host, "10.0.1.1");
    }

    #[test]
    fn explicit_alias_on_common_suffix_pool_does_not_require_header() {
        // An explicit (non-derived) alias is not the derived common-suffix,
        // so no target-host header is needed.
        let u = upstream(
            "my-pool",
            vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)],
        );
        let e = DataPlaneServiceImpl::select_endpoint(&u, "my-pool", &headers()).unwrap();
        assert_eq!(e.host, "us.vendor.com");
    }

    // -----------------------------------------------------------------------
    // parse_authority / host_authority
    // -----------------------------------------------------------------------

    #[test]
    fn parse_authority_host_and_port_forms() {
        assert_eq!(
            parse_authority("us.vendor.com"),
            Some(("us.vendor.com".to_owned(), None))
        );
        assert_eq!(
            parse_authority("us.vendor.com:8080"),
            Some(("us.vendor.com".to_owned(), Some(8080)))
        );
        assert_eq!(
            parse_authority("  us.vendor.com  "),
            Some(("us.vendor.com".to_owned(), None))
        );
    }

    #[test]
    fn parse_authority_ipv6_forms() {
        assert_eq!(
            parse_authority("[::1]:80"),
            Some(("::1".to_owned(), Some(80)))
        );
        assert_eq!(parse_authority("[::1]"), Some(("::1".to_owned(), None)));
        assert_eq!(parse_authority("::1"), Some(("::1".to_owned(), None)));
        assert_eq!(
            parse_authority("10.0.1.1"),
            Some(("10.0.1.1".to_owned(), None))
        );
    }

    #[test]
    fn parse_authority_rejects_garbage() {
        assert_eq!(parse_authority(""), None);
        assert_eq!(parse_authority("bad host"), None);
        assert_eq!(parse_authority("host:abc"), None);
        assert_eq!(parse_authority("host:"), None);
        assert_eq!(parse_authority("[::1"), None);
        assert_eq!(parse_authority("us.vendor.com:99999"), None);
    }

    #[test]
    fn host_authority_omits_standard_ports() {
        assert_eq!(
            host_authority(&ep("https", "api.example.com", 443)),
            "api.example.com"
        );
        assert_eq!(
            host_authority(&ep("http", "api.example.com", 80)),
            "api.example.com"
        );
        assert_eq!(
            host_authority(&ep("https", "api.example.com", 8443)),
            "api.example.com:8443"
        );
        assert_eq!(
            host_authority(&ep("http", "::1", 8080)),
            "[::1]:8080"
        );
    }

    // -----------------------------------------------------------------------
    // final_upstream_path
    // -----------------------------------------------------------------------

    fn http_route(path_suffix_mode: PathSuffixMode) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            tags: Vec::new(),
            priority: 0,
            match_: crate::domain::model::MatchConfig {
                http: Some(HttpMatch {
                    methods: Vec::new(),
                    path: "/v1".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode,
                }),
                grpc: None,
            },
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn final_path_append_mode() {
        let route = http_route(PathSuffixMode::Append);
        // Empty suffix is canonicalised to "/" then appended → trailing slash.
        assert_eq!(DataPlaneServiceImpl::final_upstream_path(&route, "").unwrap(), "/v1/");
        assert_eq!(DataPlaneServiceImpl::final_upstream_path(&route, "/v1").unwrap(), "/v1");
        assert_eq!(
            DataPlaneServiceImpl::final_upstream_path(&route, "/v1/users").unwrap(),
            "/v1/users"
        );
        assert_eq!(
            DataPlaneServiceImpl::final_upstream_path(&route, "users").unwrap(),
            "/v1/users"
        );
        // A suffix that does not share the base prefix is still appended.
        assert_eq!(
            DataPlaneServiceImpl::final_upstream_path(&route, "/other").unwrap(),
            "/v1/other"
        );
    }

    #[test]
    fn final_path_disabled_mode() {
        let route = http_route(PathSuffixMode::Disabled);
        assert_eq!(DataPlaneServiceImpl::final_upstream_path(&route, "").unwrap(), "/v1");
        assert_eq!(DataPlaneServiceImpl::final_upstream_path(&route, "/").unwrap(), "/v1");
        assert!(DataPlaneServiceImpl::final_upstream_path(&route, "/users").is_err());
        assert!(DataPlaneServiceImpl::final_upstream_path(&route, "users").is_err());
    }

    // -----------------------------------------------------------------------
    // build_outbound_headers / apply_rules / apply_response_rules
    // -----------------------------------------------------------------------

    fn request_view(mut inbound: HeaderMap) -> ProxyRequestView {
        inbound.insert("x-oagw-target-host", HeaderValue::from_static("ep1"));
        inbound.insert("connection", HeaderValue::from_static("keep-alive"));
        ProxyRequestView {
            method: Method::GET,
            path: "/".into(),
            query: String::new(),
            headers: inbound,
            tenant_id: Uuid::new_v4(),
            body_length_hint: None,
        }
    }

    fn effective(passthrough: PassthroughMode, allowlist: &[&str]) -> EffectiveHeaders {
        EffectiveHeaders {
            request: HeaderRules {
                set: vec![("x-set-by-gw".to_owned(), "1".to_owned())],
                add: vec![("x-added".to_owned(), "a".to_owned())],
                remove: vec!["x-removed".to_owned()],
                passthrough,
                passthrough_allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
            },
            response: HeaderRules::default(),
            passthrough,
            passthrough_allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn outbound_headers_no_passthrough() {
        let mut inbound = HeaderMap::new();
        inbound.insert("x-custom", HeaderValue::from_static("v"));
        inbound.insert("x-removed", HeaderValue::from_static("gone"));
        let view = request_view(inbound);
        let out = build_outbound_headers(&view, &effective(PassthroughMode::None, &[]));
        assert!(out.get("x-custom").is_none());
        assert!(out.get("x-removed").is_none());
        assert_eq!(out.get("x-set-by-gw").unwrap(), "1");
        assert_eq!(out.get("x-added").unwrap(), "a");
        // Routing + hop-by-hop headers are never forwarded.
        assert!(out.get("x-oagw-target-host").is_none());
        assert!(out.get("connection").is_none());
    }

    #[test]
    fn outbound_headers_allowlist_passthrough() {
        let mut inbound = HeaderMap::new();
        inbound.insert("x-custom", HeaderValue::from_static("v"));
        let view = request_view(inbound);
        let out = build_outbound_headers(&view, &effective(PassthroughMode::Allowlist, &["x-custom"]));
        assert_eq!(out.get("x-custom").unwrap(), "v");
        // Hop-by-hop still stripped even when allowlisted.
        assert!(out.get("x-oagw-target-host").is_none());
    }

    #[test]
    fn outbound_headers_all_passthrough_strips_hop_by_hop() {
        let view = request_view(HeaderMap::new());
        let out = build_outbound_headers(&view, &effective(PassthroughMode::All, &[]));
        assert!(out.get("x-oagw-target-host").is_none(), "target-host is routing-only");
        assert!(out.get("connection").is_none(), "connection is hop-by-hop");
        assert!(out.contains_key("x-added"), "rules still applied");
    }

    #[test]
    fn apply_rules_set_overwrites_and_remove_wins() {
        let mut map = HeaderMap::new();
        map.insert("x-a", HeaderValue::from_static("old"));
        map.insert("x-removed", HeaderValue::from_static("gone"));
        let rules = HeaderRules {
            set: vec![("x-a".to_owned(), "new".to_owned())],
            add: vec![("x-multi".to_owned(), "1".to_owned())],
            remove: vec!["x-removed".to_owned()],
            passthrough: PassthroughMode::None,
            passthrough_allowlist: Vec::new(),
        };
        apply_rules(&mut map, &rules);
        assert_eq!(map.get("x-a").unwrap(), "new");
        assert!(map.get("x-removed").is_none());
        let multi: Vec<_> = map.get_all("x-multi").iter().collect();
        assert_eq!(multi.len(), 1);
    }

    #[test]
    fn response_rules_apply_and_strip_hop_by_hop() {
        let mut inbound = HeaderMap::new();
        inbound.insert("content-type", HeaderValue::from_static("application/json"));
        inbound.insert("x-oagw-target-host", HeaderValue::from_static("ep1"));
        inbound.insert("x-strip", HeaderValue::from_static("yes"));
        let rules = HeaderRules {
            set: vec![("x-resp".to_owned(), "ok".to_owned())],
            add: Vec::new(),
            remove: vec!["x-strip".to_owned()],
            passthrough: PassthroughMode::None,
            passthrough_allowlist: Vec::new(),
        };
        let out = apply_response_rules(&inbound, &rules);
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("x-resp").unwrap(), "ok");
        assert!(out.get("x-strip").is_none());
        assert!(out.get("x-oagw-target-host").is_none());
    }

    // -----------------------------------------------------------------------
    // filter_query
    // -----------------------------------------------------------------------

    fn route_with_query_allowlist(allowlist: &[&str]) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            tags: Vec::new(),
            priority: 0,
            match_: crate::domain::model::MatchConfig {
                http: Some(HttpMatch {
                    methods: Vec::new(),
                    path: "/".to_owned(),
                    query_allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: crate::domain::model::PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn view_with_query(query: &str) -> ProxyRequestView {
        ProxyRequestView {
            method: Method::GET,
            path: "/".into(),
            query: query.to_owned(),
            headers: HeaderMap::new(),
            tenant_id: Uuid::new_v4(),
            body_length_hint: None,
        }
    }

    #[test]
    fn query_allowlist_empty_rejects_any_query() {
        let route = route_with_query_allowlist(&[]);
        assert!(filter_query(&route, &mut view_with_query("")).is_ok());
        assert!(filter_query(&route, &mut view_with_query("a=1")).is_err());
    }

    #[test]
    fn query_allowlist_accepts_allowed_rejects_unknown() {
        let route = route_with_query_allowlist(&["apiKey", "page"]);
        assert!(filter_query(&route, &mut view_with_query("")).is_ok());
        assert!(filter_query(&route, &mut view_with_query("apiKey=x&page=2")).is_ok());
        let err = filter_query(&route, &mut view_with_query("bogus=1")).unwrap_err();
        let p = problem_of(err);
        assert_eq!(p.status, 400);
        assert_eq!(p.gts_type, g::ERR_VALIDATION);
        assert!(p.detail.contains("bogus"));
    }
}
