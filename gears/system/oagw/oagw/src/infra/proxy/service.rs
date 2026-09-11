//! The data plane: alias resolution, route matching, endpoint selection and the plugin chain.
//!
//! Execution order follows ADR-0002: Auth → Guards → Transform(request) → upstream call →
//! Transform(response) → Guard(response). Upstream plugins run before route plugins.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use bytes::Bytes;
use toolkit_http::{HttpClient, TransportSecurity};
use uuid::Uuid;

use crate::domain::alias::{alias_matches_derivation, normalize_alias};
use crate::domain::error::{DomainError, ProblemMeta};
use crate::domain::gts_helpers;
use crate::domain::model::{
    CorsConfig, Endpoint, HttpMatch, PassthroughMode, PathSuffixMode, Route, Upstream,
};
use crate::domain::plugin::{PluginError, ProxyRequest, ProxyResponse, plugin_error};
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::infra::ratelimit::RateLimiter;

/// Hop-by-hop headers that never reach the upstream (RFC 9110 §7.6.1).
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Header the caller uses to pick an endpoint on a multi-endpoint upstream (ADR-0001).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Body framing checks of an inbound call (DESIGN §Transformation Rules).
///
/// The HTTP implementation in front of the gear refuses most malformed framing on its own; the
/// checks are restated here so the contract is the gear's and survives a change of transport.
/// Only `chunked` transfer encoding is supported.
pub fn validate_body(
    headers: &HeaderMap,
    body_len: usize,
    max_body_bytes: usize,
) -> Result<(), DomainError> {
    if let Some(encoding) = headers.get(header::TRANSFER_ENCODING) {
        let encoding = encoding.to_str().unwrap_or_default().to_ascii_lowercase();
        let chunked_only =
            encoding.split(',').all(|token| token.trim() == "chunked" && !token.trim().is_empty());
        if !chunked_only {
            return Err(DomainError::Validation(format!(
                "transfer encoding '{encoding}' is not supported, only 'chunked' is"
            )));
        }
    }
    if let Some(length) = headers.get(header::CONTENT_LENGTH) {
        let declared = length.to_str().unwrap_or_default();
        let Ok(declared) = declared.trim().parse::<u64>() else {
            return Err(DomainError::Validation(format!(
                "content-length '{declared}' is not a valid integer"
            )));
        };
        if declared != u64::try_from(body_len).unwrap_or(u64::MAX) {
            return Err(DomainError::Validation(format!(
                "content-length {declared} does not match the {body_len} bytes received"
            )));
        }
    }
    if body_len > max_body_bytes {
        return Err(DomainError::PayloadTooLarge(format!(
            "request body of {body_len} bytes exceeds the {max_body_bytes} byte limit"
        )));
    }
    Ok(())
}

/// Body ceiling before the request is refused (DESIGN `cpt-cf-oagw-constraint-body-limit`).
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Knobs the data plane is configured with.
#[derive(Debug, Clone)]
pub struct DataPlaneSettings {
    /// Upstream call timeout in seconds.
    pub proxy_timeout_secs: u64,
    /// Legalises `scheme: "http"` endpoints when set.
    pub allow_http_upstream: bool,
    /// Maximum buffered request body.
    pub max_body_bytes: usize,
}

impl Default for DataPlaneSettings {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            max_body_bytes: MAX_BODY_BYTES,
        }
    }
}

/// A resolved proxy call: everything known before the plugin chain runs.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Upstream the alias resolved to.
    pub upstream: Upstream,
    /// Route that matched.
    pub route: Option<Route>,
    /// Endpoint selected for this call.
    pub endpoint: Endpoint,
    /// Path sent upstream.
    pub path: String,
    /// Query string sent upstream (already allowlist-filtered).
    pub query: String,
    /// Effective rate limit, merged from upstream and route.
    pub limit: Option<crate::infra::ratelimit::EffectiveLimit>,
}

impl Resolved {
    /// Extension fields every problem for this call carries.
    #[must_use]
    pub fn meta(&self) -> ProblemMeta {
        let mut meta = ProblemMeta::new()
            .with_upstream_id(crate::domain::gts_helpers::upstream_gts(self.upstream.id))
            .with_alias(self.upstream.alias.clone())
            .with_path(self.path.clone());
        if let Some(limit) = &self.limit {
            meta = meta.with_retry_after(limit.retry_after_secs());
        }
        meta
    }

    /// The effective CORS configuration: route overrides upstream.
    #[must_use]
    pub fn cors(&self) -> Option<&CorsConfig> {
        self.route
            .as_ref()
            .and_then(|r| r.cors.as_ref())
            .or(self.upstream.cors.as_ref())
            .filter(|c| c.enabled)
    }
}

/// The data plane: resolves aliases and proxies requests.
pub struct DataPlane {
    stores: Arc<crate::infra::storage::memory::MemoryStores>,
    auth: AuthPluginRegistry,
    guards: GuardPluginRegistry,
    transforms: TransformPluginRegistry,
    http: HttpClient,
    rate_limiter: RateLimiter,
    settings: DataPlaneSettings,
    counter: AtomicUsize,
}

impl std::fmt::Debug for DataPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlane")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl DataPlane {
    /// Data plane over the given stores, plugin registries and settings.
    #[must_use]
    pub fn new(
        stores: Arc<crate::infra::storage::memory::MemoryStores>,
        auth: AuthPluginRegistry,
        guards: GuardPluginRegistry,
        transforms: TransformPluginRegistry,
        settings: DataPlaneSettings,
        _resolver: Option<crate::infra::credentials::SecretResolver>,
    ) -> Self {
        let transport = if settings.allow_http_upstream {
            TransportSecurity::AllowInsecureHttp
        } else {
            TransportSecurity::TlsOnly
        };
        let http = HttpClient::builder()
            .transport(transport)
            .build()
            .unwrap_or_else(|_| HttpClient::new().expect("toolkit-http default client"));
        Self {
            stores,
            auth,
            guards,
            transforms,
            http,
            rate_limiter: RateLimiter::new(),
            settings,
            counter: AtomicUsize::new(0),
        }
    }

    /// The plugin registries, for tests and for the management API's catalogue.
    #[must_use]
    pub fn registries(
        &self,
    ) -> (&AuthPluginRegistry, &GuardPluginRegistry, &TransformPluginRegistry) {
        (&self.auth, &self.guards, &self.transforms)
    }

    /// The settings the data plane is configured with.
    #[must_use]
    pub fn settings(&self) -> &DataPlaneSettings {
        &self.settings
    }

    /// The rate limit registry the data plane consumes.
    #[must_use]
    pub fn rate_limiter(&self) -> &RateLimiter {
        &self.rate_limiter
    }

    /// The upstream stores the data plane reads.
    #[must_use]
    pub fn stores(&self) -> &Arc<crate::infra::storage::memory::MemoryStores> {
        &self.stores
    }

    /// Resolve `alias` for a caller in `tenant_id`, walking the ancestor chain outward.
    #[must_use]
    pub fn resolve_alias(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        alias: &str,
    ) -> Option<Upstream> {
        let normalized = normalize_alias(alias);
        std::iter::once(tenant_id)
            .chain(ancestors.iter().copied())
            .find_map(|tenant| self.stores.upstreams.find_by_alias(tenant, &normalized))
    }

    /// Limits of the ancestor upstreams that share `alias` and enforce their configuration.
    ///
    /// Shadowing picks the routing target only: an ancestor whose rate limit carries
    /// `sharing: enforce` still applies to a descendant's calls, so its limit joins the merge
    /// (DESIGN §Hierarchical Configuration — `effective_rate = min(selected_rate, route_rate,
    /// all_ancestor_enforced_rates)`).
    #[must_use]
    pub fn enforced_ancestor_limits(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        alias: &str,
    ) -> Vec<crate::infra::ratelimit::EffectiveLimit> {
        let normalized = normalize_alias(alias);
        std::iter::once(tenant_id)
            .chain(ancestors.iter().copied())
            .skip(1)
            .filter_map(|tenant| self.stores.upstreams.find_by_alias(tenant, &normalized))
            .filter(|upstream| {
                upstream
                    .rate_limit
                    .as_ref()
                    .is_some_and(|limit| limit.sharing == Some(crate::domain::model::SharingMode::Enforce))
            })
            .filter_map(|upstream| upstream.rate_limit.as_ref().map(crate::infra::ratelimit::effective_limit))
            .collect()
    }

    /// Enabled routes of an upstream, longest-prefix matchable.
    #[must_use]
    pub fn routes_for(&self, upstream_id: Uuid) -> Vec<Route> {
        self.stores.routes.list_by_upstream(upstream_id)
    }

    /// Match a route for `method` and `path`, longest path prefix first.
    #[must_use]
    pub fn match_route<'a>(
        &self,
        routes: &'a [Route],
        method: &Method,
        path: &str,
    ) -> Option<&'a Route> {
        // The method is part of the match rule, but a path that matches with a disallowed method
        // has to be reported as a 400 rather than as an absent route (PRD §Proxy Endpoints), so
        // path-matching routes are still candidates.
        let mut best: Option<(&Route, usize)> = None;
        let mut by_path: Option<(&Route, usize)> = None;
        for route in routes {
            if !route.enabled {
                continue;
            }
            let Some(http) = route.route_match.as_http() else {
                continue;
            };
            let prefix = http.path.trim_end_matches('/');
            if !path_matches(path, prefix) {
                continue;
            }
            let depth = prefix.matches('/').count();
            if by_path.is_none_or(|(_, d)| depth > d) {
                by_path = Some((route, depth));
            }
            if http.methods.iter().any(|m| m.eq_ignore_ascii_case(method.as_str()))
                && best.is_none_or(|(_, d)| depth > d)
            {
                best = Some((route, depth));
            }
        }
        best.or(by_path).map(|(route, _)| route)
    }

    /// Validate `method` and `path` against the matched route's rules.
    pub fn validate_match(http: &HttpMatch, method: &Method, suffix: &str) -> Result<(), DomainError> {
        if !http
            .methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method.as_str()))
        {
            return Err(DomainError::Validation(format!(
                "method {} is not allowed by the matched route",
                method.as_str()
            )));
        }
        if !suffix.is_empty() && http.path_suffix_mode == PathSuffixMode::Disabled {
            return Err(DomainError::Validation(
                "this route does not accept a path suffix".to_string(),
            ));
        }
        Ok(())
    }

    /// Reject query parameters the route does not allow (PRD §Proxy Endpoints).
    ///
    /// An empty allowlist permits no query parameters at all.
    pub fn check_query(http: &HttpMatch, query: &str) -> Result<(), DomainError> {
        if query.is_empty() {
            return Ok(());
        }
        let allowed: Vec<String> =
            http.query_allowlist.iter().map(|name| name.trim().to_string()).collect();
        for (key, _) in form_urlencoded::parse(query.as_bytes()) {
            if !allowed.contains(&key.to_string()) {
                return Err(DomainError::Validation(format!(
                    "query parameter '{key}' is not in the route's query allowlist"
                )));
            }
        }
        Ok(())
    }

    /// Forward only the parameters the route's allowlist admits.
    pub fn filter_query(http: &HttpMatch, query: &str) -> String {
        if query.is_empty() || http.query_allowlist.is_empty() {
            return String::new();
        }
        let allowed: Vec<String> = http
            .query_allowlist
            .iter()
            .map(|name| name.trim().to_string())
            .collect();
        let kept: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
            .filter(|(key, _)| allowed.contains(&key.to_string()))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if kept.is_empty() {
            return String::new();
        }
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (key, value) in kept {
            serializer.append_pair(&key, &value);
        }
        serializer.finish()
    }

    /// Pick the endpoint for `upstream` honouring `X-OAGW-Target-Host` (ADR-0001).
    pub fn select_endpoint(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<Endpoint, DomainError> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(DomainError::Downstream("upstream has no endpoints".to_string()));
        }
        if endpoints.len() == 1 {
            return Ok(endpoints[0].clone());
        }
        let explicit_alias = !alias_matches_derivation(&upstream.alias, endpoints);
        if explicit_alias {
            return match target_host {
                Some(host) => find_endpoint(endpoints, host)
                    .ok_or_else(|| DomainError::TargetHostUnknown(host.to_string())),
                None => {
                    let index = self.counter.fetch_add(1, Ordering::Relaxed) % endpoints.len();
                    Ok(endpoints[index].clone())
                }
            };
        }
        let valid = valid_hosts(endpoints);
        let Some(host) = target_host else {
            return Err(DomainError::TargetHostRequired(format!(
                "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias. Valid hosts: [{}]",
                valid.join(", ")
            )));
        };
        if !crate::domain::validation::is_valid_host(host) {
            return Err(DomainError::TargetHostInvalid(host.to_string()));
        }
        find_endpoint(endpoints, host).ok_or_else(|| {
            DomainError::TargetHostUnknown(format!(
                "X-OAGW-Target-Host '{host}' does not match any configured endpoint. Valid hosts: [{}]",
                valid.join(", ")
            ))
        })
    }

    /// Consume one token from the merged limit, returning the retry delay on rejection.
    pub fn check_rate_limit(&self, resolved: &Resolved, scope_key: &str) -> Result<(), DomainError> {
        let Some(limit) = &resolved.limit else {
            return Ok(());
        };
        self.rate_limiter
            .check(scope_key, limit)
            .map_err(|_retry_after| {
            DomainError::RateLimited(format!(
                "rate limit of {} per {} exceeded",
                limit.rate,
                limit.window.as_secs_f64()
            ))
        })
    }

    /// Merge the upstream's and the route's limits, keeping the stricter of the two.
    #[must_use]
    pub fn effective_limit(&self, resolved: &Resolved) -> Option<crate::infra::ratelimit::EffectiveLimit> {
        let upstream = resolved
            .upstream
            .rate_limit
            .as_ref()
            .map(crate::infra::ratelimit::effective_limit);
        let route = resolved
            .route
            .as_ref()
            .and_then(|r| r.rate_limit.as_ref())
            .map(crate::infra::ratelimit::effective_limit);
        crate::infra::ratelimit::EffectiveLimit::merge(upstream.as_ref(), route.as_ref())
    }

    /// Validate a cross-origin actual request against the effective CORS configuration.
    pub fn check_cors(&self, resolved: &Resolved, origin: Option<&str>, method: &Method) -> Result<(), DomainError> {
        let Some(cors) = resolved.cors() else {
            return Ok(());
        };
        let Some(origin) = origin else {
            return Ok(());
        };
        if !cors.allowed_origins.iter().any(|o| o == "*" || o == origin) {
            return Err(DomainError::CorsOriginDenied(format!(
                "origin '{origin}' is not allowed"
            )));
        }
        if !cors
            .allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method.as_str()))
        {
            return Err(DomainError::CorsMethodDenied(format!(
                "method {} is not allowed",
                method.as_str()
            )));
        }
        Ok(())
    }

    /// The CORS response headers an allowed cross-origin call carries (ADR-0004).
    ///
    /// An empty result means the call is not CORS-affected and must stay untouched.
    #[must_use]
    pub fn cors_headers(resolved: &Resolved, origin: Option<&str>) -> Vec<(String, String)> {
        let Some(cors) = resolved.cors() else {
            return Vec::new();
        };
        let Some(origin) = origin else {
            return Vec::new();
        };
        if !cors.allowed_origins.iter().any(|o| o == "*" || o == origin) {
            return Vec::new();
        }
        let mut headers = vec![
            ("access-control-allow-origin".to_string(), origin.to_string()),
            ("vary".to_string(), "Origin".to_string()),
        ];
        if cors.allow_credentials {
            headers.push(("access-control-allow-credentials".to_string(), "true".to_string()));
        }
        headers
    }
    /// Build the outbound request: hop-by-hop stripping, passthrough policy and header rules.
    #[must_use]
    pub fn build_outbound(&self, resolved: &Resolved, inbound: &ProxyRequest) -> ProxyRequest {
        let mut request = ProxyRequest {
            method: inbound.method.clone(),
            path: resolved.path.clone(),
            query: resolved.query.clone(),
            headers: HeaderMap::new(),
            body: inbound.body.clone(),
            tenant_id: inbound.tenant_id,
            security: inbound.security.clone(),
        };
        let rules = resolved
            .upstream
            .headers
            .as_ref()
            .and_then(|h| h.request.as_ref());
        let (mode, allowlist) = rules
            .map(|r| {
                (
                    r.passthrough.unwrap_or_default(),
                    r.passthrough_allowlist.as_slice(),
                )
            })
            .unwrap_or((PassthroughMode::None, &[] as &[String]));

        for (name, value) in &inbound.headers {
            let lower = name.as_str().to_ascii_lowercase();
            if HOP_BY_HOP.contains(&lower.as_str()) || lower == TARGET_HOST_HEADER || lower == "host" {
                continue;
            }
            let forward = match mode {
                PassthroughMode::None => false,
                PassthroughMode::All => true,
                PassthroughMode::Allowlist => allowlist
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&lower)),
            };
            if forward {
                request.headers.insert(name.clone(), value.clone());
            }
        }
        if let Some(rules) = rules {
            apply_request_rules(&mut request.headers, rules);
        }
        request.set_header("host", &resolved.endpoint.authority());
        request.remove_header(TARGET_HOST_HEADER);
        request
    }

    /// Run the auth plugins: upstream-bound first, then route-bound.
    pub async fn authenticate(&self, request: &mut ProxyRequest, resolved: &Resolved) -> Result<(), PluginError> {
        let bindings = self.bindings(resolved);
        for binding in &bindings.auth {
            let (plugin, config) = binding;
            plugin.authenticate(request, config).await?;
        }
        Ok(())
    }

    /// Run the guard plugins over the request.
    pub async fn guard_request(&self, request: &ProxyRequest, resolved: &Resolved) -> Result<(), PluginError> {
        for (plugin, config) in &self.bindings(resolved).guards {
            plugin.guard_request(request, config).await?;
        }
        Ok(())
    }

    /// Run the guard plugins over the upstream response.
    pub async fn guard_response(
        &self,
        response: &ProxyResponse,
        resolved: &Resolved,
    ) -> Result<(), PluginError> {
        for (plugin, config) in &self.bindings(resolved).guards {
            plugin.guard_response(response, config).await?;
        }
        Ok(())
    }

    /// Run the transform plugins over the request.
    pub async fn transform_request(
        &self,
        request: &mut ProxyRequest,
        resolved: &Resolved,
    ) -> Result<(), PluginError> {
        for (plugin, config) in &self.bindings(resolved).transforms {
            plugin.transform_request(request, config).await?;
        }
        Ok(())
    }

    /// Run the transform plugins over the response.
    pub async fn transform_response(
        &self,
        response: &mut ProxyResponse,
        resolved: &Resolved,
    ) -> Result<(), PluginError> {
        for (plugin, config) in &self.bindings(resolved).transforms {
            plugin.transform_response(response, config).await?;
        }
        Ok(())
    }

    fn bindings(&self, resolved: &Resolved) -> Bindings {
        let upstream_refs = resolved
            .upstream
            .plugins
            .as_ref()
            .map(|p| p.items.as_slice())
            .unwrap_or_default();
        let route_refs = resolved
            .route
            .as_ref()
            .and_then(|r| r.plugins.as_ref())
            .map(|p| p.items.as_slice())
            .unwrap_or_default();
        let mut combined: Vec<crate::domain::model::PluginRef> = Vec::new();
        combined.extend(upstream_refs.iter().cloned());
        combined.extend(route_refs.iter().cloned());

        let mut auth = Vec::new();
        let mut guards = Vec::new();
        let mut transforms = Vec::new();
        for r in &combined {
            if let Some(found) = self.auth.resolve(r).transpose() {
                auth.extend(found);
            }
            if let Some(found) = self.guards.resolve(r).transpose() {
                guards.extend(found);
            }
            if let Some(found) = self.transforms.resolve(r).transpose() {
                transforms.extend(found);
            }
        }
        Bindings {
            auth,
            guards,
            transforms,
        }
    }

    /// Send the outbound request to the resolved endpoint.
    ///
    /// `text/event-stream` responses are returned as a streaming body so events reach the caller as
    /// they arrive; every other response is buffered.
    pub async fn send(
        &self,
        request: &ProxyRequest,
        endpoint: &Endpoint,
    ) -> Result<UpstreamResponse, DomainError> {
        let url = format!(
            "{}://{}{}{}",
            endpoint.scheme.as_str(),
            endpoint.authority(),
            request.path,
            if request.query.is_empty() {
                String::new()
            } else {
                format!("?{}", request.query)
            }
        );
        let builder = self.builder_for(request.method.clone(), &url);
        let mut builder = builder.header("host", &endpoint.authority());
        for (name, value) in &request.headers {
            // The `Host` of the call is the endpoint's authority; a second one would make the
            // upstream refuse the request outright.
            if name.as_str().eq_ignore_ascii_case("host") {
                continue;
            }
            let value = value.to_str().unwrap_or_default().to_string();
            builder = builder.header(name.as_str(), &value);
        }
        if !request.body.is_empty() || !matches!(request.method, Method::GET | Method::HEAD) {
            builder = builder.body_bytes(request.body.clone());
        }

        let timeout = std::time::Duration::from_secs(self.settings.proxy_timeout_secs.max(1));
        let outcome = tokio::time::timeout(timeout, builder.send()).await;
        let response = match outcome {
            Err(_) => {
                return Err(DomainError::Timeout(format!(
                    "upstream did not answer within {}s",
                    timeout.as_secs()
                )));
            }
            Ok(Err(e)) => {
                return Err(DomainError::Downstream(format!(
                    "upstream {} could not be reached: {e}",
                    endpoint.authority()
                )));
            }
            Ok(Ok(response)) => response,
        };

        let status = response.status();
        let headers = response.headers().clone();
        let streaming = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        if streaming {
            Ok(UpstreamResponse {
                status,
                headers,
                body: UpstreamBody::Streaming(response.into_body()),
            })
        } else {
            let body = response.bytes().await.map_err(|e| {
                DomainError::Downstream(format!("upstream body could not be read: {e}"))
            })?;
            Ok(UpstreamResponse {
                status,
                headers,
                body: UpstreamBody::Buffered(body),
            })
        }
    }

    fn builder_for(&self, method: Method, url: &str) -> toolkit_http::RequestBuilder {
        match method {
            Method::POST => self.http.post(url),
            Method::PUT => self.http.put(url),
            Method::PATCH => self.http.patch(url),
            Method::DELETE => self.http.delete(url),
            Method::HEAD => self.http.head(url),
            Method::OPTIONS => self.http.options(url),
            _ => self.http.get(url),
        }
    }
}

/// Body of an upstream response: buffered for the plugin chain, or streaming.
#[derive(Debug)]
pub enum UpstreamBody {
    /// Fully read body, the common case.
    Buffered(Bytes),
    /// Streaming body, used for `text/event-stream`.
    Streaming(toolkit_http::ResponseBody),
}

/// Response returned by the upstream.
#[derive(Debug)]
pub struct UpstreamResponse {
    /// Status of the upstream response.
    pub status: StatusCode,
    /// Headers of the upstream response.
    pub headers: HeaderMap,
    /// Body of the upstream response.
    pub body: UpstreamBody,
}

impl UpstreamResponse {
    /// The buffered body, or an empty one for streaming responses.
    #[must_use]
    pub fn buffered_body(&self) -> Bytes {
        match &self.body {
            UpstreamBody::Buffered(b) => b.clone(),
            UpstreamBody::Streaming(_) => Bytes::new(),
        }
    }

    /// True when the body streams.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        matches!(self.body, UpstreamBody::Streaming(_))
    }
}

struct Bindings {
    auth: Vec<(Arc<dyn crate::domain::plugin::AuthPlugin>, serde_json::Value)>,
    guards: Vec<(Arc<dyn crate::domain::plugin::GuardPlugin>, serde_json::Value)>,
    transforms: Vec<(Arc<dyn crate::domain::plugin::TransformPlugin>, serde_json::Value)>,
}

fn path_matches(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

pub fn valid_hosts(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints.iter().map(|e| e.host.clone()).collect()
}

fn find_endpoint(endpoints: &[Endpoint], host: &str) -> Option<Endpoint> {
    let wanted = host.trim().to_ascii_lowercase();
    let (bare, port) = match wanted.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (h.to_string(), p.parse::<u16>().ok()),
        _ => (wanted, None),
    };
    endpoints
        .iter()
        .find(|e| {
            let normalized = e.host.trim_end_matches('.').to_ascii_lowercase();
            normalized == bare && port.is_none_or(|p| p == e.effective_port())
        })
        .cloned()
}

fn apply_request_rules(headers: &mut HeaderMap, rules: &crate::domain::model::RequestHeaderRules) {
    for name in &rules.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Apply the upstream's response header rules to a response.
pub fn apply_response_rules(headers: &mut HeaderMap, rules: &crate::domain::model::ResponseHeaderRules) {
    for name in &rules.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Map a plugin error onto the domain error the proxy surfaces.
#[must_use]
pub fn map_plugin_error(e: PluginError) -> DomainError {
    plugin_error(e)
}

/// The protocol constant, re-exported for the handlers.
pub const HTTP_PROTOCOL: &str = gts_helpers::PROTOCOL_HTTP;

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
