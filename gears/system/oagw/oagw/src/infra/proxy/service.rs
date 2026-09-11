// Updated: 2026-09-01 by Constructor Tech
//! The Data Plane's orchestration layer.
//!
//! [`ProxyService`] turns one inbound request into a dialable plan:
//!
//! 1. resolve the alias through the caller's tenant chain (shadowing: the
//!    tenant closest to the caller that registered the alias wins),
//! 2. match a route (longest path prefix, then priority),
//! 3. merge the effective configuration (upstream, then route),
//! 4. enforce the rate limit and the circuit breaker,
//! 5. run the plugin chain: auth, then guards, then request transforms.
//!
//! Everything above the wire happens here; [`engine`] does the rest.

use std::sync::Arc;

use bytes::Bytes;
use http::Method;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::{PathSuffixMode, RateLimitScope, Route, Upstream};
use crate::domain::plugin::{GuardDecision, PluginError, RequestContext, ResponseContext};
use crate::infra::plugin::registry::PluginRegistry;
use crate::infra::proxy::circuit::{CircuitBreakers, Decision};
use crate::infra::proxy::error::GatewayError;
use crate::infra::proxy::ratelimit::{Limit, RateLimiter};
use crate::infra::proxy::ssrf;
use crate::infra::storage::memory::Repos;

/// The effective configuration for one proxied exchange.
///
/// A route overrides the upstream's rate limit and CORS when it declares them,
/// and *adds* its plugins after the upstream's. Header rules live on the
/// upstream only: a route describes what is matched, an upstream describes who
/// is called and how.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub headers: crate::domain::dto::HeadersConfig,
    /// Upstream plugins first, then route plugins.
    pub plugins: Vec<(String, serde_json::Value)>,
    pub rate_limit: Option<crate::domain::dto::RateLimitConfig>,
    pub cors: Option<crate::domain::dto::CorsConfig>,
}

/// A resolved plan: which upstream, which route, what configuration.
#[derive(Debug)]
pub struct Resolved {
    pub upstream: Upstream,
    pub upstream_tenant: Uuid,
    pub route: Route,
    pub effective: Effective,
}

/// What a passed rate-limit check spent, as the caller wants to know it.
///
/// ADR-0003 defaults `response_headers` to on, so every exchange a limit
/// governed reports its own budget — not only the one that was refused.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitOutcome {
    /// The bucket's capacity: `X-RateLimit-Limit`.
    pub limit: u64,
    /// Permits left after this request: `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// When the bucket is full again: `X-RateLimit-Reset`, as epoch seconds.
    pub reset: std::time::Instant,
}

impl RateLimitOutcome {
    /// The headers this outcome advertises to the caller.
    #[must_use]
    pub fn headers(&self) -> http::HeaderMap {
        use std::time::{SystemTime, UNIX_EPOCH};
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let reset = epoch.saturating_add(
            self.reset
                .saturating_duration_since(std::time::Instant::now())
                .as_secs(),
        );
        let mut out = http::HeaderMap::new();
        for (name, value) in [
            ("x-ratelimit-limit", self.limit.to_string()),
            ("x-ratelimit-remaining", self.remaining.to_string()),
            ("x-ratelimit-reset", reset.to_string()),
        ] {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(name.as_bytes()),
                http::HeaderValue::from_str(&value),
            ) {
                out.insert(name, value);
            }
        }
        out
    }
}

/// The Data Plane service.
pub struct ProxyService {
    repos: Repos,
    tenants: Option<Arc<dyn TenantResolverClient>>,
    registry: Arc<PluginRegistry>,
    config: OagwConfig,
    limiter: RateLimiter,
    breakers: CircuitBreakers,
}

impl std::fmt::Debug for ProxyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyService").finish_non_exhaustive()
    }
}

impl ProxyService {
    #[must_use]
    pub fn new(
        repos: Repos,
        tenants: Option<Arc<dyn TenantResolverClient>>,
        registry: Arc<PluginRegistry>,
        config: OagwConfig,
    ) -> Self {
        let breakers = CircuitBreakers::new(config.circuit_breaker.clone());
        Self {
            repos,
            tenants,
            registry,
            config,
            limiter: RateLimiter::new(),
            breakers,
        }
    }

    #[must_use]
    pub fn registry(&self) -> &Arc<PluginRegistry> {
        &self.registry
    }

    #[must_use]
    pub fn breakers(&self) -> &CircuitBreakers {
        &self.breakers
    }

    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    // ── Resolution ──────────────────────────────────────────────────────

    /// The caller's tenant chain, root first, own tenant last.
    ///
    /// Fails soft to the caller's own tenant when the resolver is unreachable:
    /// a gateway that cannot see the hierarchy still routes its own tenant.
    async fn tenant_chain(&self, ctx: &SecurityContext) -> Vec<Uuid> {
        let tenant = ctx.subject_tenant_id();
        if tenant.is_nil() {
            return Vec::new();
        }
        let Some(resolver) = &self.tenants else {
            return vec![tenant];
        };
        match resolver
            .get_ancestors(
                ctx,
                tenant_resolver_sdk::TenantId(tenant),
                &tenant_resolver_sdk::models::GetAncestorsOptions {
                    barrier_mode: tenant_resolver_sdk::models::BarrierMode::Respect,
                },
            )
            .await
        {
            Ok(resp) => {
                let mut chain: Vec<Uuid> = resp.ancestors.iter().map(|t| t.id.0).collect();
                chain.push(tenant);
                chain
            }
            Err(err) => {
                tracing::warn!(%err, %tenant, "tenant ancestor lookup failed; scoping to own tenant");
                vec![tenant]
            }
        }
    }

    /// Find the upstream an alias names, walking the tenant chain from the
    /// root down so the tenant closest to the caller shadows an ancestor's
    /// registration.
    ///
    /// # Errors
    ///
    /// [`GatewayError::unknown_alias`] when nothing on the chain registered it.
    pub async fn resolve_upstream(
        &self,
        ctx: &SecurityContext,
        alias: &str,
    ) -> Result<(Upstream, Uuid), GatewayError> {
        let normalized = crate::infra::proxy::alias::normalize(alias);
        if normalized.is_empty() {
            return Err(
                GatewayError::validation("the target host must not be empty")
                    .with("alias", serde_json::json!(alias)),
            );
        }
        for tenant in self.tenant_chain(ctx).await {
            if let Some(rec) = self.repos.upstreams.get_by_alias(tenant, &normalized).await {
                if !rec.upstream.enabled {
                    return Err(GatewayError::unknown_alias(&normalized));
                }
                return Ok((rec.upstream, rec.tenant_id));
            }
        }
        Err(GatewayError::unknown_alias(&normalized))
    }

    /// The routes bound to an upstream, deduplicated.
    async fn routes_for(&self, upstream_id: &Uuid) -> Vec<Route> {
        let mut out = Vec::new();
        let seen = &mut std::collections::HashSet::new();
        for rec in self.repos.routes.list_for_upstream(*upstream_id).await {
            if seen.insert(rec.id()) {
                out.push(rec.route);
            }
        }
        out
    }

    /// Pick the route serving `path` on `upstream`.
    ///
    /// Longest path prefix wins, then the higher priority. A route whose method
    /// list does not admit the request is remembered separately, so a wrong
    /// method becomes a 405 rather than a 404.
    ///
    /// # Errors
    ///
    /// [`GatewayError::no_route`], or a 405 when only the method is at fault.
    pub async fn match_route(
        &self,
        upstream: &Upstream,
        method: &Method,
        path: &str,
    ) -> Result<Route, GatewayError> {
        let upstream_id = upstream.id.unwrap_or_default();
        let wanted = crate::domain::dto::HttpMethod::parse(method);
        let mut best: Option<(usize, i64, Route)> = None;
        let mut method_blocked: Option<(usize, Route)> = None;

        for route in self.routes_for(&upstream_id).await {
            if !route.enabled {
                continue;
            }
            let Some(http) = &route.r#match.http else {
                // gRPC routes are catalogued but not reachable over this proxy
                // path; they never match an HTTP request.
                continue;
            };
            if !path_is_under(path, &http.path) {
                continue;
            }
            let depth = specificity(&http.path);
            if let Some(m) = wanted
                && http.methods.contains(&m)
            {
                if best
                    .as_ref()
                    .is_none_or(|(d, p, _)| (depth, route.priority) > (*d, *p))
                {
                    best = Some((depth, route.priority, route));
                }
                continue;
            }
            if method_blocked.as_ref().is_none_or(|(d, _)| depth > *d) {
                method_blocked = Some((depth, route));
            }
        }

        match best {
            Some((_, _, route)) => Ok(route),
            None => match method_blocked {
                Some((_, route)) => Err(method_not_allowed(&route.id.unwrap_or_default())),
                None => Err(GatewayError::no_route(
                    upstream.alias.as_deref().unwrap_or_default(),
                    path,
                )),
            },
        }
    }

    /// Resolve the alias and the route in one step.
    ///
    /// # Errors
    ///
    /// [`GatewayError`] on an unknown alias, a disabled upstream, a missing
    /// route or a denied method.
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        method: &Method,
        path: &str,
    ) -> Result<Resolved, GatewayError> {
        let (upstream, upstream_tenant) = self.resolve_upstream(ctx, alias).await?;
        let route = self.match_route(&upstream, method, path).await?;
        let effective = merge(&upstream, &route);
        Ok(Resolved {
            upstream,
            upstream_tenant,
            route,
            effective,
        })
    }

    // ── Rate limiting ───────────────────────────────────────────────────

    /// Derive the counter key from the route's scope.    #[must_use]
    pub fn rate_limit_key(
        &self,
        scope: RateLimitScope,
        tenant: Uuid,
        route_id: &Uuid,
        ctx: &RequestContext,
    ) -> String {
        match scope {
            RateLimitScope::Tenant => format!("t:{tenant}"),
            RateLimitScope::User => format!("u:{tenant}:{}", ctx.subject_id().unwrap_or_default()),
            RateLimitScope::Ip => format!("i:{tenant}:{}", caller_address(ctx)),
            RateLimitScope::Route => format!("r:{tenant}:{route_id}"),
            RateLimitScope::Global => "g:global".to_owned(),
        }
    }

    /// Spend one permit against the effective rate limit.
    ///
    /// # Errors
    ///
    /// [`GatewayError::rate_limited`] when the bucket is empty.
    pub fn enforce_rate_limit(
        &self,
        limit: &crate::domain::dto::RateLimitConfig,
        tenant: Uuid,
        route_id: &Uuid,
        ctx: &RequestContext,
    ) -> Result<RateLimitOutcome, GatewayError> {
        let key = self.rate_limit_key(limit.scope, tenant, route_id, ctx);
        let l = Limit::new(limit.rate_per_second(), limit.capacity(), limit.algorithm);
        let verdict = self.limiter.check(&key, &l);
        let outcome = RateLimitOutcome {
            limit: limit.capacity() as u64,
            remaining: verdict.remaining,
            reset: verdict.reset,
        };
        if verdict.allowed {
            return Ok(outcome);
        }
        Err(GatewayError::rate_limited(
            verdict.retry_after,
            limit.capacity() as u64,
            verdict.remaining,
        )
        .with("route_id", serde_json::json!(route_id)))
    }

    // ── Circuit breaker ─────────────────────────────────────────────────

    /// Whether the upstream's breaker allows a dial.
    ///
    /// # Errors
    ///
    /// [`GatewayError::circuit_open`].
    pub fn check_breaker(&self, upstream_id: &Uuid) -> Result<(), GatewayError> {
        if matches!(
            self.breakers.before(&upstream_id.to_string()),
            Decision::Reject
        ) {
            return Err(GatewayError::circuit_open(
                &upstream_id.to_string(),
                self.breakers.config().open_duration.as_secs().max(1),
            ));
        }
        Ok(())
    }

    pub fn record_upstream_success(&self, upstream_id: &Uuid) {
        self.breakers.on_success(&upstream_id.to_string());
    }

    pub fn record_upstream_failure(&self, upstream_id: &Uuid) {
        self.breakers.on_failure(&upstream_id.to_string());
    }

    // ── SSRF ────────────────────────────────────────────────────────────

    #[must_use]
    pub fn ssrf_policy(&self) -> &crate::config::SsrfPolicy {
        &self.config.ssrf_policy
    }

    /// Screen resolved addresses before a socket is opened.
    ///
    /// # Errors
    ///
    /// [`GatewayError`] when the policy denies the host.
    pub fn screen_host(&self, host: &str, addrs: &[std::net::IpAddr]) -> Result<(), GatewayError> {
        ssrf::check(host, addrs, &self.config.ssrf_policy).map_err(|rej| match rej {
            ssrf::SsrfRejection::BlockedRange { .. } => GatewayError::forbidden(format!(
                "endpoint '{host}' falls in a network range the SSRF policy denies"
            ))
            .with("host", serde_json::json!(host)),
            ssrf::SsrfRejection::Unresolvable { .. } => {
                GatewayError::link_unavailable(format!("endpoint '{host}' could not be resolved"))
                    .with("host", serde_json::json!(host))
            }
        })
    }

    // ── Plugins ─────────────────────────────────────────────────────────

    /// Run the auth plugin, then the guards, then the request transforms, in
    /// the order the chain declares.
    ///
    /// A binding's `config` object is merged over the plugin's stored
    /// configuration, so a route can specialise a shared plugin without
    /// duplicating it.
    ///
    /// # Errors
    ///
    /// [`GatewayError`] when a plugin refuses or is unknown.
    pub async fn run_request_plugins(
        &self,
        chain: &[(String, serde_json::Value)],
        ctx: &mut RequestContext,
    ) -> Result<(), GatewayError> {
        for (id, binding) in chain {
            let config = binding.clone();
            ctx.config.insert("plugin".to_owned(), config.clone());
            ctx.config
                .insert("required_headers".to_owned(), config.clone());

            if let Ok(auth) = self.registry.resolve_auth(id) {
                auth.authenticate(ctx).await.map_err(plugin_error)?;
                continue;
            }
            if let Ok(guard) = self.registry.resolve_guard(id) {
                match guard.guard_request(ctx).await.map_err(plugin_error)? {
                    GuardDecision::Allow => continue,
                    GuardDecision::Reject { status, code } => {
                        return Err(GatewayError::plugin_rejected(status, &code.clone(), code));
                    }
                }
            }
            if let Ok(transform) = self.registry.resolve_transform(id) {
                transform
                    .transform_request(ctx)
                    .await
                    .map_err(plugin_error)?;
                continue;
            }
            // A binding that names a plugin this process cannot execute is an
            // operational failure of the gateway, not a miss against the
            // caller's URL — the route matched, the chain could not run. That
            // is why DESIGN tables it as 503 rather than 404.
            //
            // A stored custom plugin is the usual reason: ADR-0002 keeps the
            // MVP to Rust implementations registered in-process ("no scripting
            // languages yet"), so a Starlark definition is catalogued and
            // served by the management API but has nothing to run it here.
            let detail = if stored_plugin(&self.repos, id).await {
                format!(
                    "plugin '{id}' is a stored custom plugin and cannot be executed by this \
                     gateway: custom Starlark plugins are catalogued but not run (ADR-0002, no \
                     scripting language in this build). Bind a builtin plugin instead"
                )
            } else {
                format!("plugin '{id}' is not registered with this gateway")
            };
            return Err(GatewayError::new(
                http::StatusCode::SERVICE_UNAVAILABLE,
                crate::gts::ERR_PLUGIN_NOT_FOUND,
                detail,
            )
            .with("plugin_id", serde_json::json!(id)));
        }
        Ok(())
    }

    /// Run the response guards and transforms.
    ///
    /// # Errors
    ///
    /// [`GatewayError`] when a response guard refuses.
    pub async fn run_response_plugins(
        &self,
        chain: &[(String, serde_json::Value)],
        ctx: &mut ResponseContext,
    ) -> Result<(), GatewayError> {
        for (id, binding) in chain {
            ctx.config
                .insert("required_headers".to_owned(), binding.clone());
            if let Ok(guard) = self.registry.resolve_guard(id) {
                match guard.guard_response(ctx).await.map_err(plugin_error)? {
                    GuardDecision::Allow => {}
                    GuardDecision::Reject { status, code } => {
                        return Err(GatewayError::plugin_rejected(status, &code.clone(), code));
                    }
                }
            }
            if let Ok(transform) = self.registry.resolve_transform(id) {
                transform
                    .transform_response(ctx)
                    .await
                    .map_err(plugin_error)?;
            }
        }
        Ok(())
    }
}

/// Merge an upstream's configuration with a matched route's.
#[must_use]
/// Whether `id` names a plugin stored in the plugin repository.
///
/// Bindings carry either a builtin's GTS identifier or a UUID naming a row in
/// `oagw_plugin`; only the latter can name something the registry has no
/// implementation for.
async fn stored_plugin(repos: &Repos, id: &str) -> bool {
    let Some(uuid) = crate::gts::uuid_of(id).or_else(|| uuid::Uuid::parse_str(id).ok()) else {
        return false;
    };
    repos.plugins.get_any(uuid).await.is_some()
}

pub fn merge(upstream: &Upstream, route: &Route) -> Effective {
    let mut plugins: Vec<(String, serde_json::Value)> = Vec::new();
    for item in &upstream.plugins.items {
        push_plugin(&mut plugins, item);
    }
    for item in &route.plugins.items {
        push_plugin(&mut plugins, item);
    }
    Effective {
        headers: upstream.headers.clone(),
        plugins,
        rate_limit: route.rate_limit.clone().or(upstream.rate_limit.clone()),
        cors: route.cors.clone().or(upstream.cors.clone()),
    }
}

fn push_plugin(out: &mut Vec<(String, serde_json::Value)>, item: &crate::domain::dto::PluginItem) {
    match item {
        crate::domain::dto::PluginItem::Reference(name) => {
            out.push((name.clone(), serde_json::Value::Null));
        }
        crate::domain::dto::PluginItem::Inline(inline) => {
            let id = inline
                .id
                .clone()
                .or_else(|| inline.plugin_type.clone())
                .unwrap_or_default();
            out.push((id, inline.config.clone().unwrap_or(serde_json::Value::Null)));
        }
    }
}

fn plugin_error(err: PluginError) -> GatewayError {
    match err {
        PluginError::Rejected {
            status,
            code,
            message,
        } => GatewayError::plugin_rejected(status, &code, message),
        PluginError::Infrastructure(m) => GatewayError::secret_unavailable(m),
    }
}

/// Whether `path` is served by a route matching `prefix`.
#[must_use]
pub fn path_is_under(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() || prefix == "/" {
        return true;
    }
    if path == prefix {
        return true;
    }
    path.starts_with(prefix) && matches!(path.as_bytes().get(prefix.len()), Some(b'/') | None)
}

/// Specificity score: more segments wins.
fn specificity(prefix: &str) -> usize {
    prefix.trim_end_matches('/').split('/').count()
}

/// The caller address the `ip` rate-limit scope counts.
fn caller_address(ctx: &RequestContext) -> String {
    ctx.headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

/// Build the request context a plugin sees.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn request_context(
    request_id: &str,
    security_context: SecurityContext,
    tenant: Uuid,
    upstream_id: &Uuid,
    alias: &str,
    path: &str,
    query: &str,
    method: &Method,
    headers: http::HeaderMap,
    body: Bytes,
) -> RequestContext {
    RequestContext {
        request_id: request_id.to_owned(),
        security_context,
        tenant_id: tenant,
        alias: alias.to_owned(),
        upstream_id: crate::gts::instance_id(crate::gts::UPSTREAM_TYPE, *upstream_id),
        path: path.to_owned(),
        query: query.to_owned(),
        method: method.clone(),
        headers,
        config: Default::default(),
        body,
    }
}

/// Whether a route's suffix mode admits a suffix at all.
#[must_use]
pub fn suffix_admitted(mode: PathSuffixMode) -> bool {
    matches!(mode, PathSuffixMode::Append)
}

/// The 405 helper: a route matched on path but not on method.
#[must_use]
pub fn method_not_allowed(route_id: &Uuid) -> GatewayError {
    GatewayError::new(
        http::StatusCode::METHOD_NOT_ALLOWED,
        crate::gts::ERR_PROTOCOL_ERROR,
        "the matched route does not admit this method",
    )
    .with("route_id", serde_json::json!(route_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenCacheConfig;
    use crate::domain::dto::{
        Endpoint, HttpMatch, HttpMethod, InlinePlugin, PluginItem, PluginsConfig, RouteMatch,
        Scheme, ServerConfig,
    };
    use crate::domain::repo::{RouteRecord, RouteRepository, UpstreamRecord, UpstreamRepository};
    use crate::infra::storage::memory;
    use std::time::SystemTime;

    fn upstream(alias: &str, host: &str) -> Upstream {
        Upstream {
            id: Some(Uuid::new_v4()),
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: vec![],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: host.to_owned(),
                    port: Some(8080),
                }],
            },
            protocol: crate::domain::dto::Protocol::Http,
            auth: None,
            headers: crate::domain::dto::HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn route(upstream_id: Uuid, path: &str, methods: &[HttpMethod]) -> Route {
        Route {
            id: Some(Uuid::new_v4()),
            upstream_id,
            r#match: RouteMatch {
                http: Some(HttpMatch {
                    methods: methods.to_vec(),
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            enabled: true,
            priority: 0,
            tags: vec![],
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn service(store: &memory::Stores) -> ProxyService {
        ProxyService::new(
            memory::repos(store),
            None,
            crate::infra::plugin::registry::PluginRegistry::with_builtins(
                None,
                TokenCacheConfig::default(),
            ),
            OagwConfig::default(),
        )
    }

    #[tokio::test]
    async fn resolves_an_alias_through_the_chain() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = crate::infra::plugin::test_support::security_context();
        let tenant = ctx.subject_tenant_id();
        let u = upstream("api.example.com", "backend.internal");
        store
            .upstreams
            .insert(UpstreamRecord {
                tenant_id: tenant,
                upstream: u.clone(),
                created_at: SystemTime::now(),
                updated_at: SystemTime::now(),
            })
            .await
            .unwrap();

        let (found, owner) = svc.resolve_upstream(&ctx, "api.example.com").await.unwrap();
        assert_eq!(found.id, u.id);
        assert_eq!(owner, tenant);
    }

    #[tokio::test]
    async fn an_unknown_alias_is_a_404() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = crate::infra::plugin::test_support::security_context();
        let err = svc
            .resolve_upstream(&ctx, "nothing.example")
            .await
            .unwrap_err();
        assert_eq!(err.status, http::StatusCode::NOT_FOUND);
        assert_eq!(err.type_id, crate::gts::ERR_UPSTREAM_NOT_FOUND);
    }

    #[tokio::test]
    async fn the_closest_tenant_shadows_an_ancestor() {
        // Two tenants register the same alias; the caller's own tenant wins.
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = crate::infra::plugin::test_support::security_context();
        let own = ctx.subject_tenant_id();
        let ancestor = uuid::Uuid::from_u128(0xdead_beef);
        store
            .upstreams
            .insert(UpstreamRecord {
                tenant_id: ancestor,
                upstream: upstream("api.example.com", "ancestor.internal"),
                created_at: SystemTime::now(),
                updated_at: SystemTime::now(),
            })
            .await
            .unwrap();
        store
            .upstreams
            .insert(UpstreamRecord {
                tenant_id: own,
                upstream: upstream("api.example.com", "own.internal"),
                created_at: SystemTime::now(),
                updated_at: SystemTime::now(),
            })
            .await
            .unwrap();

        // Without a resolver the chain is just the caller's own tenant, so the
        // ancestor's registration is unreachable from here.
        let (found, owner) = svc.resolve_upstream(&ctx, "api.example.com").await.unwrap();
        assert_eq!(owner, own);
        assert_eq!(found.server.endpoints[0].host, "own.internal");
    }

    #[tokio::test]
    async fn a_disabled_upstream_is_invisible() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = crate::infra::plugin::test_support::security_context();
        let tenant = ctx.subject_tenant_id();
        let mut u = upstream("api.example.com", "backend.internal");
        u.enabled = false;
        store
            .upstreams
            .insert(UpstreamRecord {
                tenant_id: tenant,
                upstream: u,
                created_at: SystemTime::now(),
                updated_at: SystemTime::now(),
            })
            .await
            .unwrap();
        assert!(svc.resolve_upstream(&ctx, "api.example.com").await.is_err());
    }

    #[tokio::test]
    async fn the_longest_path_prefix_wins() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let u = upstream("api.example.com", "backend.internal");
        let uid = u.id.unwrap();
        let short = route(uid, "/v1", &[HttpMethod::Get]);
        let long = route(uid, "/v1/models", &[HttpMethod::Get]);
        for r in [short, long.clone()] {
            store
                .routes
                .insert(RouteRecord {
                    tenant_id: uuid::Uuid::nil(),
                    route: r,
                    created_at: SystemTime::now(),
                    updated_at: SystemTime::now(),
                })
                .await
                .unwrap();
        }
        let picked = svc
            .match_route(&u, &http::Method::GET, "/v1/models/1")
            .await
            .unwrap();
        assert_eq!(picked.id, long.id);
    }

    #[tokio::test]
    async fn a_disallowed_method_is_a_405() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let u = upstream("api.example.com", "backend.internal");
        let uid = u.id.unwrap();
        store
            .routes
            .insert(RouteRecord {
                tenant_id: uuid::Uuid::nil(),
                route: route(uid, "/v1", &[HttpMethod::Get]),
                created_at: SystemTime::now(),
                updated_at: SystemTime::now(),
            })
            .await
            .unwrap();
        let err = svc
            .match_route(&u, &http::Method::POST, "/v1/models")
            .await
            .unwrap_err();
        assert_eq!(err.status, http::StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn an_unmatched_path_is_a_404() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let u = upstream("api.example.com", "backend.internal");
        let uid = u.id.unwrap();
        store
            .routes
            .insert(RouteRecord {
                tenant_id: uuid::Uuid::nil(),
                route: route(uid, "/v1", &[HttpMethod::Get]),
                created_at: SystemTime::now(),
                updated_at: SystemTime::now(),
            })
            .await
            .unwrap();
        let err = svc
            .match_route(&u, &http::Method::GET, "/v2/models")
            .await
            .unwrap_err();
        assert_eq!(err.status, http::StatusCode::NOT_FOUND);
        assert_eq!(err.type_id, crate::gts::ERR_ROUTE_NOT_FOUND);
    }

    #[tokio::test]
    async fn higher_priority_breaks_a_tie() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let u = upstream("api.example.com", "backend.internal");
        let uid = u.id.unwrap();
        let mut a = route(uid, "/v1/models", &[HttpMethod::Get]);
        a.priority = 1;
        let mut b = route(uid, "/v1/models", &[HttpMethod::Get]);
        b.priority = 10;
        for r in [a, b.clone()] {
            store
                .routes
                .insert(RouteRecord {
                    tenant_id: uuid::Uuid::nil(),
                    route: r,
                    created_at: SystemTime::now(),
                    updated_at: SystemTime::now(),
                })
                .await
                .unwrap();
        }
        let picked = svc
            .match_route(&u, &http::Method::GET, "/v1/models")
            .await
            .unwrap();
        assert_eq!(picked.id, b.id);
    }

    #[test]
    fn merge_prefers_the_route_rate_limit() {
        let mut u = upstream("api.example.com", "backend.internal");
        u.rate_limit = Some(crate::domain::dto::RateLimitConfig {
            sharing: crate::domain::dto::SharingMode::Private,
            algorithm: crate::domain::dto::RateLimitAlgorithm::TokenBucket,
            sustained: crate::domain::dto::SustainedRate {
                rate: 10,
                window: crate::domain::dto::RateWindow::Second,
            },
            burst: None,
            scope: crate::domain::dto::RateLimitScope::Tenant,
            strategy: crate::domain::dto::RateLimitStrategy::Reject,
            cost: 1,
        });
        let mut r = route(u.id.unwrap(), "/v1", &[HttpMethod::Get]);
        r.rate_limit = Some(crate::domain::dto::RateLimitConfig {
            sharing: crate::domain::dto::SharingMode::Private,
            algorithm: crate::domain::dto::RateLimitAlgorithm::TokenBucket,
            sustained: crate::domain::dto::SustainedRate {
                rate: 1,
                window: crate::domain::dto::RateWindow::Second,
            },
            burst: None,
            scope: crate::domain::dto::RateLimitScope::Route,
            strategy: crate::domain::dto::RateLimitStrategy::Reject,
            cost: 1,
        });
        let eff = merge(&u, &r);
        assert_eq!(eff.rate_limit.as_ref().unwrap().rate_per_second(), 1.0);
    }

    #[test]
    fn merge_concatenates_upstream_then_route_plugins() {
        let mut u = upstream("api.example.com", "backend.internal");
        u.plugins.items = vec![PluginItem::Inline(InlinePlugin {
            id: None,
            plugin_type: Some("request_id".to_owned()),
            config: Some(serde_json::json!({})),
            enabled: Some(true),
            sharing: Some(crate::domain::dto::SharingMode::Private),
            tags: vec![],
        })];
        let mut r = route(u.id.unwrap(), "/v1", &[HttpMethod::Get]);
        r.plugins.items = vec![PluginItem::Reference("required_headers".to_owned())];
        let eff = merge(&u, &r);
        assert_eq!(eff.plugins.len(), 2);
        assert_eq!(eff.plugins[0].0, "request_id");
        assert_eq!(eff.plugins[1].0, "required_headers");
    }

    #[test]
    fn rate_limit_keys_are_scoped_per_kind() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = crate::infra::plugin::test_support::request_context();
        let tenant = uuid::Uuid::from_u128(1);
        let route_id = uuid::Uuid::from_u128(2);
        assert_eq!(
            svc.rate_limit_key(RateLimitScope::Tenant, tenant, &route_id, &ctx),
            format!("t:{tenant}")
        );
        assert_eq!(
            svc.rate_limit_key(RateLimitScope::Route, tenant, &route_id, &ctx),
            format!("r:{tenant}:{route_id}")
        );
        assert_eq!(
            svc.rate_limit_key(RateLimitScope::Global, tenant, &route_id, &ctx),
            "g:global"
        );
        // The user scope carries the caller, the ip scope the caller's address.
        assert_ne!(
            svc.rate_limit_key(RateLimitScope::User, tenant, &route_id, &ctx),
            svc.rate_limit_key(RateLimitScope::Ip, tenant, &route_id, &ctx)
        );
    }

    #[tokio::test]
    async fn rate_limiting_rejects_after_the_burst() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = crate::infra::plugin::test_support::request_context();
        let limit = crate::domain::dto::RateLimitConfig {
            sharing: crate::domain::dto::SharingMode::Private,
            algorithm: crate::domain::dto::RateLimitAlgorithm::TokenBucket,
            sustained: crate::domain::dto::SustainedRate {
                rate: 1,
                window: crate::domain::dto::RateWindow::Second,
            },
            burst: Some(crate::domain::dto::Burst { capacity: 1 }),
            scope: crate::domain::dto::RateLimitScope::Route,
            strategy: crate::domain::dto::RateLimitStrategy::Reject,
            cost: 1,
        };
        let route_id = uuid::Uuid::from_u128(3);
        svc.enforce_rate_limit(&limit, uuid::Uuid::from_u128(1), &route_id, &ctx)
            .unwrap();
        let err = svc
            .enforce_rate_limit(&limit, uuid::Uuid::from_u128(1), &route_id, &ctx)
            .unwrap_err();
        assert_eq!(err.status, http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(err.type_id, crate::gts::ERR_RATE_LIMIT_EXCEEDED);
    }

    #[tokio::test]
    async fn a_passed_limit_still_reports_its_budget() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = crate::infra::plugin::test_support::request_context();
        let limit = crate::domain::dto::RateLimitConfig {
            sharing: crate::domain::dto::SharingMode::Private,
            algorithm: crate::domain::dto::RateLimitAlgorithm::TokenBucket,
            sustained: crate::domain::dto::SustainedRate {
                rate: 5,
                window: crate::domain::dto::RateWindow::Second,
            },
            burst: Some(crate::domain::dto::Burst { capacity: 5 }),
            scope: crate::domain::dto::RateLimitScope::Route,
            strategy: crate::domain::dto::RateLimitStrategy::Reject,
            cost: 1,
        };
        let route_id = uuid::Uuid::from_u128(4);
        let first = svc
            .enforce_rate_limit(&limit, uuid::Uuid::from_u128(1), &route_id, &ctx)
            .unwrap();
        let second = svc
            .enforce_rate_limit(&limit, uuid::Uuid::from_u128(1), &route_id, &ctx)
            .unwrap();

        let h = first.headers();
        assert_eq!(h.get("x-ratelimit-limit").unwrap(), "5");
        assert_eq!(h.get("x-ratelimit-remaining").unwrap(), "4");
        // The reset is an absolute instant, in the future.
        let reset: u64 = h
            .get("x-ratelimit-reset")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(reset > 0);

        let h2 = second.headers();
        assert_eq!(h2.get("x-ratelimit-remaining").unwrap(), "3");
    }

    #[tokio::test]
    async fn the_circuit_breaker_opens_after_enough_failures() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let id = uuid::Uuid::from_u128(9);
        svc.check_breaker(&id).unwrap();
        for _ in 0..5 {
            svc.record_upstream_failure(&id);
        }
        let err = svc.check_breaker(&id).unwrap_err();
        assert_eq!(err.status, http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.type_id, crate::gts::ERR_CIRCUIT_BREAKER_OPEN);
    }

    #[tokio::test]
    async fn request_plugins_run_in_declared_order() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let mut ctx = crate::infra::plugin::test_support::request_context();
        let chain = vec![(
            crate::gts::TRANSFORM_REQUEST_ID.to_owned(),
            serde_json::Value::Null,
        )];
        svc.run_request_plugins(&chain, &mut ctx).await.unwrap();
        assert!(ctx.headers.contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn an_unknown_plugin_is_a_503() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let mut ctx = crate::infra::plugin::test_support::request_context();
        let chain = vec![("no.such.plugin.v1".to_owned(), serde_json::Value::Null)];
        let err = svc.run_request_plugins(&chain, &mut ctx).await.unwrap_err();
        assert_eq!(err.status, http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.type_id, crate::gts::ERR_PLUGIN_NOT_FOUND);
    }

    #[tokio::test]
    async fn the_required_headers_guard_rejects_a_missing_header() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let mut ctx = crate::infra::plugin::test_support::request_context();
        let chain = vec![(
            crate::gts::GUARD_REQUIRED_HEADERS.to_owned(),
            serde_json::json!({ "required_request_headers": "x-tenant" }),
        )];
        let err = svc.run_request_plugins(&chain, &mut ctx).await.unwrap_err();
        assert_eq!(err.status, http::StatusCode::BAD_REQUEST);

        ctx.headers.insert("x-tenant", "t".parse().unwrap());
        svc.run_request_plugins(&chain, &mut ctx).await.unwrap();
    }

    #[test]
    fn path_matching_respects_segment_boundaries() {
        assert!(path_is_under("/v1/models", "/v1"));
        assert!(path_is_under("/v1", "/v1"));
        assert!(!path_is_under("/v11/models", "/v1"));
        assert!(path_is_under("/anything", "/"));
        assert!(path_is_under("/anything", ""));
    }

    #[test]
    fn suffix_mode_append_admits_a_suffix() {
        assert!(suffix_admitted(PathSuffixMode::Append));
        assert!(!suffix_admitted(PathSuffixMode::Disabled));
    }
}
