//! Data-plane service: the proxy execution pipeline (DESIGN §3.3).
//!
//! Request flow: alias resolution over the tenant chain → endpoint selection
//! (with plaintext policy) → route matching → plugin chain (auth → guard →
//! transform) → rate limiting → upstream call → response plugins → caller.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::error::OagwError;
use crate::domain::error::{ErrorSource, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::model::{
    CorsConfig, Endpoint, EndpointScheme, PathSuffixMode, PluginBinding, RateLimitConfig, RateScope,
    Route, Upstream,
};
use crate::domain::plugin::{ErrorContext, GuardDecision, RequestContext, ResponseContext};
use crate::domain::repo::{nil_id, PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::services::proxy::{
    DataPlaneService, ProxyRequest, ResolvedTarget, RouteMatch, TargetHostChoice, UpstreamRequest,
    UpstreamResponse,
};
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::infra::proxy::client::UpstreamClient;
use crate::infra::proxy::headers;
use crate::infra::proxy::upgrade;
use crate::infra::proxy::stream::UpstreamBodyStream;
use crate::infra::ratelimit::{self, RateLimiter};
use crate::infra::storage::memory::MemoryStore;
use crate::infra::tenant::TenantHierarchy;

/// Dependencies shared by the control plane and the data plane.
#[derive(Clone)]
pub struct DataPlaneDeps {
    /// In-memory configuration store.
    pub store: Arc<MemoryStore>,
    /// Tenant-chain resolver.
    pub tenants: TenantHierarchy,
    /// Credential store, for auth plugins that resolve secrets.
    pub credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    /// Gear configuration.
    pub config: OagwConfig,
}

/// The proxy engine.
#[derive(Clone)]
pub struct DataPlane {
    store: Arc<MemoryStore>,
    tenants: TenantHierarchy,
    auth_plugins: AuthPluginRegistry,
    guard_plugins: GuardPluginRegistry,
    transform_plugins: TransformPluginRegistry,
    limiter: Arc<RateLimiter>,
    client: Arc<UpstreamClient>,
    config: Arc<OagwConfig>,
    round_robin: Arc<AtomicUsize>,
}

impl std::fmt::Debug for DataPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlane")
            .field("allow_http_upstream", &self.config.allow_http_upstream)
            .field("proxy_timeout_secs", &self.config.proxy_timeout_secs)
            .finish_non_exhaustive()
    }
}

impl DataPlane {
    /// Builds the engine.
    ///
    /// # Errors
    ///
    /// [`OagwError::Internal`] when the upstream client cannot be built.
    pub fn new(deps: DataPlaneDeps) -> OagwResult<Self> {
        let token_cache = deps.config.token_cache.clone();
        Ok(Self {
            auth_plugins: AuthPluginRegistry::with_builtins(deps.credstore, token_cache),
            guard_plugins: GuardPluginRegistry::with_builtins(),
            transform_plugins: TransformPluginRegistry::with_builtins(),
            limiter: RateLimiter::new(),
            client: Arc::new(UpstreamClient::new(deps.config.proxy_timeout())?),
            config: Arc::new(deps.config),
            store: deps.store,
            tenants: deps.tenants,
            round_robin: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Store access for the management API.
    #[must_use]
    pub fn store(&self) -> &Arc<MemoryStore> {
        &self.store
    }

    /// Tenant hierarchy access for the management API.
    #[must_use]
    pub fn tenants(&self) -> &TenantHierarchy {
        &self.tenants
    }

    /// Every resolvable plugin id across the three registries.
    #[must_use]
    pub fn plugin_catalog(&self) -> Vec<String> {
        let mut ids = self.auth_plugins.ids();
        ids.extend(self.guard_plugins.ids());
        ids.extend(self.transform_plugins.ids());
        ids.sort();
        ids.dedup();
        ids
    }

    /// Resolves an alias against the caller's tenant chain (nearest first).
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`] when no tenant on the chain owns the
    /// alias, or every owner has it disabled.
    pub async fn resolve_target(
        &self,
        security_context: &toolkit_security::SecurityContext,
        alias: &str,
        choice: &TargetHostChoice,
    ) -> OagwResult<ResolvedTarget> {
        let tenant_id = security_context.subject_tenant_id();
        let chain = self.tenants.chain(security_context, tenant_id).await?;
        let upstream = self.resolve_alias(&chain.ids(), alias)?;
        let endpoint = self.select_endpoint(&upstream, choice)?;
        Ok(ResolvedTarget {
            upstream,
            endpoint,
            tenant_id,
            tenant_chain: chain.ids(),
        })
    }

    /// Alias lookup across the tenant chain.
    fn resolve_alias(&self, chain: &[uuid::Uuid], alias: &str) -> OagwResult<Upstream> {
        let normalized = alias::normalize(alias);
        for owner in chain {
            if let Some(upstream) = self.store.find_upstream_by_alias(*owner, &normalized)? {
                if !upstream.enabled {
                    continue;
                }
                return Ok(upstream);
            }
        }
        Err(OagwError::RouteNotFound(
            "alias is not owned by any tenant on the chain".to_owned(),
        ))
    }

    /// Picks the endpoint for this request.
    ///
    /// # Errors
    ///
    /// [`OagwError::InvalidTargetHost`] / [`OagwError::UnknownTargetHost`]
    /// for a bad pin, [`OagwError::LinkUnavailable`] when no endpoint
    /// satisfies the configured plaintext policy.
    pub fn select_endpoint(
        &self,
        upstream: &Upstream,
        choice: &TargetHostChoice,
    ) -> OagwResult<Endpoint> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(OagwError::LinkUnavailable(
                "upstream has no endpoints".to_owned(),
            ));
        }
        let index = match choice {
            TargetHostChoice::Auto => self.round_robin.fetch_add(1, Ordering::Relaxed) % endpoints.len(),
            TargetHostChoice::Pinned(requested) => upgrade::validate_target_host(endpoints, Some(requested))?
                .ok_or(OagwError::UnknownTargetHost)?,
        };
        let endpoint = endpoints[index].clone();
        if endpoint.scheme == EndpointScheme::Http && !self.config.allow_http_upstream {
            return Err(OagwError::LinkUnavailable(
                "plaintext upstream endpoints are disabled by allow_http_upstream=false".to_owned(),
            ));
        }
        Ok(endpoint)
    }

    /// Matches `method`/`path` against the upstream's routes.
    ///
    /// The longest matching path wins. An upstream with no routes acts as a
    /// catch-all proxy; an upstream with routes requires one to match.
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`] when the configured route set rejects the
    /// request, [`OagwError::ProtocolError`] when only the method differs.
    pub fn match_route(
        &self,
        upstream: &Upstream,
        method: &http::Method,
        path: &str,
    ) -> OagwResult<Option<RouteMatch>> {
        let routes = self.store.routes_for_upstream(upstream.id)?;
        if routes.is_empty() {
            return Ok(Some(RouteMatch {
                upstream_path: path.to_owned(),
                query: Vec::new(),
                route: Route {
                    id: nil_id(),
                    tenant_id: upstream.tenant_id,
                    tags: Vec::new(),
                    upstream_id: upstream.id,
                    r#match: crate::domain::model::MatchRule {
                        http: None,
                        grpc: None,
                    },
                    plugins: crate::domain::model::PluginListConfig::default(),
                    rate_limit: None,
                    created_at: 0,
                    updated_at: 0,
                },
            }));
        }

        let mut best: Option<(usize, &Route)> = None;
        let mut method_mismatch = false;
        for route in &routes {
            let Some(http_match) = &route.r#match.http else {
                continue;
            };
            if path_matches(&http_match.path, path).is_none() {
                continue;
            }
            if !http_match
                .methods
                .iter()
                .any(|candidate| candidate.as_method() == method.as_str())
            {
                method_mismatch = true;
                continue;
            }
            let specificity = http_match.path.len();
            if best.map_or(true, |(len, _)| specificity > len) {
                best = Some((specificity, route));
            }
        }

        match best {
            Some((_, route)) => Ok(Some(RouteMatch {
                upstream_path: path.to_owned(),
                query: Vec::new(),
                route: route.clone(),
            })),
            None if method_mismatch => {
                Err(OagwError::ProtocolError(
                    "method not allowed for the matched route path".to_owned(),
                ))
            }
            None => Err(OagwError::RouteNotFound(
                "no route matches this method and path".to_owned(),
            )),
        }
    }

    /// Effective rate limit for a resolved upstream + route pair.
    #[must_use]
    pub fn effective_limit(
        &self,
        upstream: &Upstream,
        route: Option<&Route>,
    ) -> Option<RateLimitConfig> {
        ratelimit::effective_rate_limit(&[upstream.rate_limit.clone(), route.and_then(|r| r.rate_limit.clone())])
    }

    /// CORS response headers for an upstream, or an empty map.
    #[must_use]
    pub fn cors_headers(&self, cors: Option<&CorsConfig>, request: &HeaderMap) -> HeaderMap {
        let Some(cors) = cors else {
            return HeaderMap::new();
        };
        let origin = request
            .get(http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !cors.enabled || origin.is_empty() {
            return HeaderMap::new();
        }
        if !origin_allowed(&cors.allowed_origins, origin) {
            return HeaderMap::new();
        }
        let mut out = HeaderMap::new();
        if cors.allowed_origins.iter().any(|o| o == "*") && !cors.allow_credentials {
            crate::infra::proxy::headers::insert_lossy(&mut out, "access-control-allow-origin", "*");
        } else {
            crate::infra::proxy::headers::insert_lossy(&mut out, "access-control-allow-origin", origin);
            crate::infra::proxy::headers::insert_lossy(&mut out, "vary", "Origin");
        }
        if cors.allow_credentials {
            crate::infra::proxy::headers::insert_lossy(&mut out, "access-control-allow-credentials", "true");
        }
        if !cors.expose_headers.is_empty() {
            crate::infra::proxy::headers::insert_lossy(
                &mut out,
                "access-control-expose-headers",
                &cors.expose_headers.join(", "),
            );
        }
        out
    }

    /// Merged plugin list for a resolved upstream + route.
    #[must_use]
    pub fn plugin_list_for(&self, upstream: &Upstream, route: Option<&Route>) -> Vec<PluginEntry> {
        plugin_list(&self.store.clone(), upstream, route)
    }

    /// Re-renders a registry miss, distinguishing "unknown id" from a custom
    /// plugin whose Starlark body has no engine in this build.
    fn registry_miss(&self, err: OagwError, id: &str) -> OagwError {
        if matches!(err, OagwError::PluginNotFound(_)) {
            unresolvable_plugin(&self.store, id)
        } else {
            err
        }
    }

    async fn run_request_plugins(
        &self,
        upstream: &Upstream,
        route: Option<&Route>,
        ctx: &mut RequestContext,
    ) -> OagwResult<()> {
        if let Some(auth) = &upstream.auth {
            let plugin = self.auth_plugins.get(&auth.plugin_type)?;
            ctx.config = auth.config.clone();
            plugin.authenticate(ctx).await?;
        }
        for entry in plugin_list(&self.store, upstream, route) {
            match gts_helpers::plugin_kind(&entry.id) {
                Some("guard") => {
                    let plugin = self
                        .guard_plugins
                        .get(&entry.id)
                        .map_err(|err| self.registry_miss(err, &entry.id))?;
                    ctx.config = entry.config;
                    if let GuardDecision::Reject(err) = plugin.guard_request(ctx).await? {
                        return Err(err);
                    }
                }
                Some("transform") => {
                    let plugin = self
                        .transform_plugins
                        .get(&entry.id)
                        .map_err(|err| self.registry_miss(err, &entry.id))?;
                    ctx.config = entry.config;
                    plugin.transform_request(ctx).await?;
                }
                Some("auth") => {
                    let plugin = self
                        .auth_plugins
                        .get(&entry.id)
                        .map_err(|err| self.registry_miss(err, &entry.id))?;
                    ctx.config = entry.config;
                    plugin.authenticate(ctx).await?;
                }
                _ => return Err(unresolvable_plugin(&self.store, &entry.id)),
            }
        }
        Ok(())
    }

    async fn run_response_plugins(
        &self,
        plugins: &[PluginEntry],
        ctx: &mut ResponseContext,
    ) -> OagwResult<()> {
        for entry in plugins {
            match gts_helpers::plugin_kind(&entry.id) {
                Some("guard") => {
                    let plugin = self
                        .guard_plugins
                        .get(&entry.id)
                        .map_err(|err| self.registry_miss(err, &entry.id))?;
                    ctx.config = entry.config.clone();
                    if let GuardDecision::Reject(err) = plugin.guard_response(ctx).await? {
                        return Err(err);
                    }
                }
                Some("transform") => {
                    let plugin = self
                        .transform_plugins
                        .get(&entry.id)
                        .map_err(|err| self.registry_miss(err, &entry.id))?;
                    ctx.config = entry.config.clone();
                    plugin.transform_response(ctx).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn run_error_plugins(&self, plugins: &[PluginEntry], ctx: &mut ErrorContext) {
        for entry in plugins {
            if gts_helpers::plugin_kind(&entry.id) == Some("transform") {
                if let Ok(plugin) = self.transform_plugins.get(&entry.id) {
                    let _ = plugin.transform_error(ctx).await;
                }
            }
        }
    }

    async fn enforce_rate_limit(
        &self,
        upstream: &Upstream,
        route: Option<&Route>,
        request: &ProxyRequest,
    ) -> OagwResult<()> {
        let Some(config) = self.effective_limit(upstream, route) else {
            return Ok(());
        };
        let scope_value = match config.scope {
            RateScope::Global => "global".to_owned(),
            RateScope::Tenant => request.security_context.subject_tenant_id().to_string(),
            RateScope::User => request.security_context.subject_id().to_string(),
            RateScope::Ip => client_ip(&request.headers),
            RateScope::Route => route.map_or_else(|| upstream.id.to_string(), |r| r.id.to_string()),
        };
        let key = ratelimit::scope_key(&upstream.id.to_string(), &config, &[&scope_value]);
        let decision = self.limiter.try_acquire(&config, &key, config.cost.max(1));
        if decision.allowed {
            return Ok(());
        }
        Err(ratelimit::rejection_error(&decision))
    }

    async fn report_failure(&self, plugins: &[PluginEntry], err: OagwError) -> OagwError {
        let mut ctx = ErrorContext {
            error: err,
            headers: HeaderMap::new(),
        };
        self.run_error_plugins(plugins, &mut ctx).await;
        ctx.error
    }
}

#[async_trait::async_trait]
impl DataPlaneService for DataPlane {
    async fn proxy(&self, request: ProxyRequest) -> OagwResult<UpstreamResponse> {
        if request.body.len() > self.config.max_body_bytes {
            return Err(OagwError::PayloadTooLarge);
        }
        let tenant_id = request.security_context.subject_tenant_id();
        let chain = self.tenants.chain(&request.security_context, tenant_id).await?;
        let upstream = self.resolve_alias(&chain.ids(), &request.alias)?;
        let endpoint = self.select_endpoint(&upstream, &request.target_host)?;
        // ADR-0004: the actual cross-origin request is the enforcement point —
        // preflights are answered permissively, so a disallowed origin or
        // method is rejected here, before the upstream is contacted.
        enforce_cors(upstream.cors.as_ref(), &request.headers, &request.method)?;

        // Path after the alias, normalised to start with `/`.
        let mut suffix = request.path_suffix.clone();
        if !suffix.is_empty() && !suffix.starts_with('/') {
            suffix.insert(0, '/');
        }
        let routes = self.store.routes_for_upstream(upstream.id)?;
        let matched = if routes.is_empty() {
            None
        } else {
            self.match_route(&upstream, &request.method, &suffix)?
        };
        let route = matched.as_ref().map(|m| &m.route);
        let (upstream_path, query) = match &matched {
            Some(matched) => {
                let http_match = matched.route.r#match.http.clone().unwrap_or_default();
                let consumed = path_matches(&http_match.path, &suffix).unwrap_or(0);
                let remainder = &suffix[consumed.min(suffix.len())..];
                // A trailing `/*`/`{suffix}` is a match marker, never a literal
                // path segment, so joining always happens from the base.
                let base = route_base_path(&http_match.path);
                let path = match http_match.path_suffix_mode {
                    PathSuffixMode::Append => join_path(base, remainder),
                    PathSuffixMode::Disabled => base.to_owned(),
                };
                (path, filter_query(&http_match.query_allowlist, &request.query))
            }
            None => (suffix.clone(), request.query.clone()),
        };

        self.enforce_rate_limit(&upstream, route, &request).await?;

        let mut inbound = request.headers.clone();
        // The caller's `Host` is captured before the gateway's routing headers
        // are stripped, so it can be reported upstream as `X-Forwarded-Host`
        // while the forwarded request carries the upstream host instead.
        let client_host = inbound
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        headers::strip_gateway_headers(&mut inbound);
        let request_rules = upstream.headers.as_ref().and_then(|h| h.request.as_ref());
        let mut upstream_headers = headers::apply_request_rules(&inbound, request_rules);
        headers::add_forwarding_headers(&mut upstream_headers, client_host.as_deref());

        let plugins = plugin_list(&self.store, &upstream, route);
        let mut ctx = RequestContext {
            security_context: request.security_context.clone(),
            tenant_id: chain.self_id,
            upstream_id: upstream.id,
            route_id: route.map(|r| r.id),
            method: request.method.clone(),
            path: upstream_path.clone(),
            query: query.clone(),
            headers: upstream_headers,
            body: request.body.clone(),
            config: serde_json::Value::Null,
        };
        if let Err(err) = self.run_request_plugins(&upstream, route, &mut ctx).await {
            return Err(self.report_failure(&plugins, err).await);
        }

        let upstream_request = UpstreamRequest {
            method: ctx.method.to_string(),
            path: ctx.path.clone(),
            query: ctx.query.clone(),
            headers: ctx.headers.clone(),
            body: ctx.body.clone(),
            is_upgrade: request.is_upgrade(),
            endpoint: endpoint.clone(),
        };

        let response = self.client.send(&endpoint, &upstream_request).await;
        let mut response = match response {
            Ok(response) => response,
            Err(err) => return Err(self.report_failure(&plugins, err).await),
        };

        if let Some(upgraded) = response.upgraded.take() {
            // A 101 carries no body; response plugins do not run. The two
            // upgraded sides are spliced here so frames flow both ways.
            let response_headers = headers::apply_response_rules(
                &response.headers,
                upstream.headers.as_ref().and_then(|h| h.response.as_ref()),
                true,
            );
            if let Some(client_upgrade) = request.on_upgrade {
                crate::infra::proxy::upgrade::splice_upgraded(client_upgrade, upgraded);
            } else {
                tracing::debug!("upstream upgraded but the inbound request carries no upgrade");
            }
            return Ok(UpstreamResponse {
                status: response.status,
                headers: response_headers,
                body: Bytes::new(),
                stream: None,
                upgraded: None,
                source: ErrorSource::Upstream,
            });
        }

        // Response plugins need the body in memory; a plugin-free proxy stays
        // streaming so SSE and chunked payloads pass through untouched.
        let mut body = Bytes::new();
        let mut stream = response.stream.take();
        if !plugins.is_empty() {
            if let Some(incoming) = stream.take() {
                body = collect(incoming, self.config.max_body_bytes).await?;
            }
        }

        let mut response_headers = headers::apply_response_rules(
            &response.headers,
            upstream.headers.as_ref().and_then(|h| h.response.as_ref()),
            response.status.is_success(),
        );
        let cors_headers = self.cors_headers(upstream.cors.as_ref(), &request.headers);
        for (name, value) in &cors_headers {
            response_headers.append(name, value.clone());
        }

        let mut response_ctx = ResponseContext {
            status: response.status,
            headers: response_headers,
            body: body.clone(),
            is_error: !response.status.is_success(),
            config: serde_json::Value::Null,
        };
        if let Err(err) = self.run_response_plugins(&plugins, &mut response_ctx).await {
            return Err(self.report_failure(&plugins, err).await);
        }

        Ok(UpstreamResponse {
            status: response_ctx.status,
            headers: response_ctx.headers,
            body,
            stream,
            upgraded: None,
            source: ErrorSource::Upstream,
        })
    }

    async fn preflight(&self, request: ProxyRequest) -> OagwResult<http::Response<Bytes>> {
        // ADR-0004: browser preflights carry no credentials, so there is no
        // tenant context and no upstream resolution. The answer is permissive
        // and echoes what the browser asked for; origin/method enforcement
        // happens on the actual request, after upstream resolution.
        let mut response_headers = HeaderMap::new();
        let mut echo = |source: &http::HeaderName, target: http::HeaderName| {
            if let Some(value) = request.headers.get(source) {
                response_headers.append(target, value.clone());
            }
        };
        echo(
            &http::header::ORIGIN,
            http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        );
        echo(
            &http::header::ACCESS_CONTROL_REQUEST_METHOD,
            http::header::ACCESS_CONTROL_ALLOW_METHODS,
        );
        echo(
            &http::header::ACCESS_CONTROL_REQUEST_HEADERS,
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        );
        response_headers.insert(
            http::header::ACCESS_CONTROL_MAX_AGE,
            http::HeaderValue::from_static("86400"),
        );
        response_headers.insert(
            http::header::VARY,
            http::HeaderValue::from_static(
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
            ),
        );
        Ok(response_with(StatusCode::NO_CONTENT, response_headers))
    }
}

/// A plugin reference resolved for one request.
#[derive(Debug, Clone)]
pub struct PluginEntry {
    /// Plugin GTS id (or custom plugin id).
    pub id: String,
    /// Plugin configuration for this invocation.
    pub config: serde_json::Value,
}

/// Merged plugin list (upstream first, then route), deduplicated.
///
/// `store` resolves bindings written as a bare custom-plugin UUID into the
/// plugin's canonical GTS id and its stored configuration (DESIGN "Resolution
/// Algorithm": a UUID instance is looked up in the plugin store first).
#[must_use]
pub fn plugin_list(
    store: &MemoryStore,
    upstream: &Upstream,
    route: Option<&Route>,
) -> Vec<PluginEntry> {
    let mut out: Vec<PluginEntry> = Vec::new();
    let mut push = |binding: &PluginBinding| {
        if out.iter().any(|entry| entry.id == binding.id()) {
            return;
        }
        let id = binding.id();
        let (id, config) = match gts_helpers::resource_uuid(id) {
            // Bare UUID: a custom plugin, stored under its canonical GTS id.
            Some(uuid) => match find_custom_plugin(store, uuid) {
                Some(plugin) => (plugin.id, plugin.config),
                None => (id.to_owned(), binding.config().clone()),
            },
            None => (id.to_owned(), binding.config().clone()),
        };
        if out.iter().any(|entry| entry.id == id) {
            return;
        }
        out.push(PluginEntry { id, config });
    };
    if let Some(plugins) = &upstream.plugins {
        for binding in &plugins.items {
            push(binding);
        }
    }
    if let Some(route) = route {
        for binding in &route.plugins.items {
            push(binding);
        }
    }
    out
}

fn response_with(status: StatusCode, headers: HeaderMap) -> http::Response<Bytes> {
    let mut builder = http::Response::builder().status(status);
    for (name, value) in &headers {
        builder = builder.header(name.clone(), value.clone());
    }
    builder.body(Bytes::new()).unwrap_or_else(|_| {
        http::Response::builder()
            .status(status)
            .body(Bytes::new())
            .expect("static response")
    })
}

/// `true` when `origin` is permitted by `allowed` (`*` allows everything).
#[must_use]
/// Enforces the upstream's CORS policy on an actual (non-preflight)
/// cross-origin request (ADR-0004): a disallowed origin or method is
/// rejected before the request reaches the upstream.
///
/// # Errors
///
/// [`OagwError::CorsForbidden`] when the origin or the method is not
/// allowed.
pub fn enforce_cors(
    cors: Option<&CorsConfig>,
    request: &HeaderMap,
    method: &http::Method,
) -> OagwResult<()> {
    let Some(cors) = cors.filter(|cors| cors.enabled) else {
        return Ok(());
    };
    let Some(origin) = request
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        // Same-origin (or non-browser) request: CORS does not apply.
        return Ok(());
    };
    if !origin_allowed(&cors.allowed_origins, origin) {
        return Err(OagwError::CorsForbidden(format!(
            "origin '{origin}' is not allowed for this upstream"
        )));
    }
    let method_allowed = cors
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()));
    if !method_allowed {
        return Err(OagwError::CorsForbidden(format!(
            "method '{method}' is not allowed for this upstream"
        )));
    }
    Ok(())
}

/// Errors for a plugin id that no registry can execute.
///
/// A custom plugin is stored (so it *is* registered) but its Starlark body has
/// no execution engine in this build; that is a different failure from an id
/// that resolves to nothing at all, and the two must not share a message.
fn unresolvable_plugin(store: &MemoryStore, id: &str) -> OagwError {
    match gts_helpers::resource_uuid(id).and_then(|uuid| find_custom_plugin(store, uuid)) {
        Some(plugin) if !plugin.source_code.trim().is_empty() => {
            OagwError::PluginRuntimeUnavailable(format!(
                "custom plugin '{id}' has no Starlark engine in this build"
            ))
        }
        Some(_) => OagwError::PluginNotFound(id.to_owned()),
        None => OagwError::PluginNotFound(id.to_owned()),
    }
}

/// Finds a custom plugin row by its UUID instance, in any tenant.
///
/// A bare UUID binding does not name its plugin kind, so the three plugin
/// namespaces are tried in turn.
fn find_custom_plugin(store: &MemoryStore, uuid: uuid::Uuid) -> Option<crate::domain::model::Plugin> {
    let prefixes = [
        crate::domain::gts_helpers::AUTH_PLUGIN_PREFIX,
        crate::domain::gts_helpers::GUARD_PLUGIN_PREFIX,
        crate::domain::gts_helpers::TRANSFORM_PLUGIN_PREFIX,
    ];
    prefixes
        .into_iter()
        .find_map(|prefix| store.find_plugin(&format!("{prefix}{uuid}")).ok().flatten())
}

pub fn origin_allowed(allowed: &[String], origin: &str) -> bool {
    allowed.iter().any(|o| o == "*") || allowed.iter().any(|o| o.eq_ignore_ascii_case(origin))
}

/// Collects a streaming body up to `limit` bytes.
async fn collect(stream: UpstreamBodyStream, limit: usize) -> OagwResult<Bytes> {
    let mut buffer = Vec::new();
    let mut stream = stream;
    while let Some(frame) = stream.next().await {
        let frame = frame.map_err(|err| OagwError::LinkUnavailable(err.to_string()))?;
        let Ok(data) = frame.into_data() else {
            // Trailer frames carry no body bytes.
            continue;
        };
        if buffer.len() + data.len() > limit {
            return Err(OagwError::PayloadTooLarge);
        }
        buffer.extend_from_slice(data.as_ref());
    }
    Ok(Bytes::from(buffer))
}

/// Strips a trailing match marker (`/*` or `/{suffix}`) from a route pattern,
/// leaving the literal prefix to forward upstream.
#[must_use]
pub fn route_base_path(route_path: &str) -> &str {
    for marker in ["/*", "/{suffix}"] {
        if let Some(prefix) = route_path.strip_suffix(marker) {
            return prefix;
        }
    }
    route_path
}

/// `Some(consumed)` when `path` matches the configured route path.
///
/// Both `/prefix/*` and `/prefix/{suffix}` accept a trailing segment; the
/// return value is the number of bytes of `path` covered by the configured
/// prefix, so the caller can compute the suffix.
#[must_use]
pub fn path_matches(route_path: &str, path: &str) -> Option<usize> {    if route_path == path {
        return Some(path.len());
    }
    for marker in ["/*", "/{suffix}"] {
        let Some(prefix) = route_path.strip_suffix(marker) else {
            continue;
        };
        if prefix.is_empty() {
            return Some(0);
        }
        if path == prefix || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')) {
            return Some(prefix.len());
        }
    }
    None
}

/// Joins a route path with a suffix.
#[must_use]
pub fn join_path(base: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return base.to_owned();
    }
    let trimmed = base.trim_end_matches('/');
    let suffix = if suffix.starts_with('/') {
        suffix.to_owned()
    } else {
        format!("/{suffix}")
    };
    format!("{trimmed}{suffix}")
}

/// Keeps only the query parameters the route allows (empty allowlist = all).
#[must_use]
pub fn filter_query(allowlist: &[String], query: &[(String, String)]) -> Vec<(String, String)> {
    if allowlist.is_empty() {
        return query.to_vec();
    }
    query
        .iter()
        .filter(|(name, _)| allowlist.iter().any(|a| a == name))
        .cloned()
        .collect()
}

/// Best-effort client IP from proxy headers.
#[must_use]
pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme, UpstreamServer};

    /// Minimal enabled upstream with one plaintext loopback endpoint.
    fn upstream_for_tests() -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::nil(),
            enabled: true,
            alias: "test.local".to_owned(),
            alias_explicit: false,
            tags: Vec::new(),
            server: UpstreamServer {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: 9199,
                }],
            },
            protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn bare_uuid_bindings_resolve_through_the_plugin_store() {
        let store = std::sync::Arc::new(MemoryStore::new());
        let uuid = uuid::Uuid::new_v4();
        let id = format!("{}{uuid}", crate::domain::gts_helpers::GUARD_PLUGIN_PREFIX);
        store
            .insert_plugin(crate::domain::model::Plugin {
                id: id.clone(),
                tenant_id: uuid::Uuid::nil(),
                name: "custom".to_owned(),
                description: String::new(),
                plugin_type: crate::domain::model::PluginType::Guard,
                config_schema: serde_json::Value::Null,
                config: serde_json::json!({"required_request_headers": "x-correlation-id"}),
                source_code: "def guard(ctx): pass".to_owned(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        let upstream = Upstream {
            plugins: Some(crate::domain::model::PluginListConfig {
                sharing: crate::domain::model::Sharing::Private,
                items: vec![crate::domain::model::PluginBinding::Ref(uuid.to_string())],
            }),
            ..upstream_for_tests()
        };
        let plugins = plugin_list(&store, &upstream, None);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, id, "the canonical GTS id is used");
        assert_eq!(
            plugins[0].config["required_request_headers"],
            "x-correlation-id",
            "the stored plugin configuration is carried"
        );
    }

    #[test]
    fn unknown_bare_uuid_bindings_are_passed_through() {
        let store = std::sync::Arc::new(MemoryStore::new());
        let upstream = Upstream {
            plugins: Some(crate::domain::model::PluginListConfig {
                sharing: crate::domain::model::Sharing::Private,
                items: vec![crate::domain::model::PluginBinding::Ref(
                    uuid::Uuid::new_v4().to_string(),
                )],
            }),
            ..upstream_for_tests()
        };
        let plugins = plugin_list(&store, &upstream, None);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].config, serde_json::Value::Null);
    }

    #[test]
    fn path_matches_exact_and_prefix() {
        assert_eq!(path_matches("/a/b", "/a/b"), Some(4));
        assert_eq!(path_matches("/a/b", "/a/b/c"), None);
        assert_eq!(path_matches("/a/*", "/a/b/c"), Some(2));
        assert_eq!(path_matches("/a/*", "/a"), Some(2));
        assert_eq!(path_matches("/a/{suffix}", "/a/b"), Some(2));
        assert_eq!(path_matches("/a/{suffix}", "/ab"), None);
        assert_eq!(path_matches("/*", "/anything"), Some(0));
    }

    #[test]
    fn route_base_path_strips_match_markers() {
        assert_eq!(route_base_path("/status/*"), "/status");
        assert_eq!(route_base_path("/status/{suffix}"), "/status");
        assert_eq!(route_base_path("/echo"), "/echo");
        assert_eq!(route_base_path("/*"), "");
    }

    #[test]
    fn join_path_appends_suffix() {
        assert_eq!(join_path("/v1", "/users/7"), "/v1/users/7");
        assert_eq!(join_path("/v1", ""), "/v1");
        assert_eq!(join_path("/v1", "x"), "/v1/x");
    }

    #[test]
    fn query_allowlist_filters() {
        let allow = vec!["b".to_owned()];
        let query = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
        ];
        assert_eq!(filter_query(&allow, &query), vec![("b".to_owned(), "2".to_owned())]);
        assert_eq!(filter_query(&[], &query), query);
    }

    #[test]
    fn origin_matching_is_case_insensitive() {
        let allowed = vec!["https://App.Example.com".to_owned()];
        assert!(origin_allowed(&allowed, "https://app.example.com"));
        assert!(!origin_allowed(&allowed, "https://other.example.com"));
        assert!(origin_allowed(&["*".to_owned()], "https://anything.example"));
    }

    fn cors_headers(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::ORIGIN, http::HeaderValue::from_str(origin).unwrap());
        headers
    }

    #[test]
    fn cors_is_not_enforced_without_an_origin() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://good.example".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            ..Default::default()
        };
        // Non-browser request (no `Origin`): CORS does not apply.
        assert!(enforce_cors(Some(&cors), &HeaderMap::new(), &http::Method::GET).is_ok());
    }

    #[test]
    fn cors_is_not_enforced_when_disabled() {
        let cors = CorsConfig {
            enabled: false,
            allowed_origins: vec!["https://good.example".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            ..Default::default()
        };
        assert!(
            enforce_cors(Some(&cors), &cors_headers("https://evil.example"), &http::Method::GET).is_ok()
        );
    }

    #[test]
    fn disallowed_origin_is_rejected_with_403() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://good.example".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            ..Default::default()
        };
        let err = enforce_cors(Some(&cors), &cors_headers("https://evil.example"), &http::Method::GET)
            .unwrap_err();
        assert_eq!(err.status(), http::StatusCode::FORBIDDEN);
        assert_eq!(err.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.cors.forbidden.v1");
    }

    #[test]
    fn disallowed_method_is_rejected_with_403() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            ..Default::default()
        };
        let err = enforce_cors(Some(&cors), &cors_headers("https://good.example"), &http::Method::DELETE)
            .unwrap_err();
        assert_eq!(err.status(), http::StatusCode::FORBIDDEN);
        let ok = enforce_cors(Some(&cors), &cors_headers("https://good.example"), &http::Method::GET);
        assert!(ok.is_ok());
    }
}
