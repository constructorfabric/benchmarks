//! Data-plane resolution service (DESIGN §3.2 "Proxy API", §3.5 proxy flow).
//!
//! The service turns `(tenant, alias, method, path)` into the effective
//! configuration a proxy hop needs: the closest enabled upstream on the tenant
//! chain, the matching route on that chain, and the merged auth/rate-limit/
//! CORS/header/plugin policy. It never performs I/O against the upstream —
//! that is the transport layer's job (`infra::proxy`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::merge::{is_enforced, merge_cors, merge_plugins, merge_rate_limit, merge_tags};
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, HttpMethod, MatchConfig, PluginsConfig,
    Protocol, RateLimitConfig, Route, Upstream,
};
use crate::domain::repo::{RouteRepository, UpstreamRepository};

/// Identity of one rate-limit counter (ADR-0003 `scope`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CounterKey {
    /// `scope: global` — a single gateway-wide counter.
    Global,
    /// `scope: tenant` — one counter per tenant.
    Tenant(String),
    /// `scope: user` — one counter per authenticated subject, scoped to the
    /// tenant so two tenants' subjects never share a counter.
    User { tenant: String, subject: String },
    /// `scope: ip` — one counter per client address.
    Ip(String),
    /// `scope: route` — one counter per route, scoped to the tenant that owns
    /// the route definition.
    Route { tenant: String, route: String },
}

impl CounterKey {
    /// Derives the counter identity from the configured scope.
    #[must_use]
    pub fn for_scope(
        scope: crate::domain::model::RateScope,
        tenant_id: &str,
        subject_id: &str,
        client_ip: &str,
        route_id: &str,
    ) -> Self {
        use crate::domain::model::RateScope;
        match scope {
            RateScope::Global => Self::Global,
            RateScope::Tenant => Self::Tenant(tenant_id.to_owned()),
            RateScope::User => Self::User {
                tenant: tenant_id.to_owned(),
                subject: subject_id.to_owned(),
            },
            RateScope::Ip => Self::Ip(client_ip.to_owned()),
            RateScope::Route => Self::Route {
                tenant: tenant_id.to_owned(),
                route: route_id.to_owned(),
            },
        }
    }
}

/// Result of a rate-limit check, carrying the values the wire layer relays in
/// the `X-RateLimit-*` headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateVerdict {
    /// Configured sustained rate.
    pub limit: u64,
    /// Tokens left after this request.
    pub remaining: u64,
    /// Seconds until the bucket is fully replenished.
    pub reset_seconds: u64,
}

/// Port the data plane uses to charge a request against a counter.
pub trait RateLimitStore: Send + Sync {
    /// Charges `cost` tokens, returning the verdict or a `RateLimitExceeded`.
    ///
    /// # Errors
    ///
    /// Returns `RateLimitExceeded` when the counter is exhausted.
    fn charge(
        &self,
        key: CounterKey,
        config: &RateLimitConfig,
        cost: u64,
    ) -> DomainResult<RateVerdict>;
}

/// Port that resolves a tenant's ancestor chain, closest parent first.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Returns `tenant_id` followed by its ancestors, closest to root.
    ///
    /// # Errors
    ///
    /// Returns an error when the hierarchy cannot be resolved.
    async fn chain(&self, tenant_id: Uuid) -> DomainResult<Vec<Uuid>>;
}

/// Effective configuration of one proxy request.
#[derive(Debug, Clone)]
pub struct ResolvedRequest {
    /// Upstream the request is proxied to.
    pub upstream: Upstream,
    /// Route that matched, when any.
    pub route: Option<Route>,
    /// Merged authentication configuration.
    pub auth: AuthConfig,
    /// Merged plugin chain (upstream bindings then route bindings).
    pub plugins: PluginsConfig,
    /// Merged header transformation.
    pub headers: HeadersConfig,
    /// Merged rate limit, when either level configured one.
    pub rate_limit: Option<RateLimitConfig>,
    /// Merged CORS policy, when either level configured one.
    pub cors: Option<CorsConfig>,
    /// Add-only union of upstream and route tags.
    pub tags: Vec<String>,
    /// Path left over after the route match was cut off.
    pub path_suffix: String,
    /// Query string the matched route admits, kept apart from the path so it
    /// never participates in prefix matching.
    pub query: Option<String>,
    /// Round-robin cursor for pool selection when the request pins no endpoint.
    pool_cursor: Arc<AtomicUsize>,
}

impl ResolvedRequest {
    /// Picks the endpoint the request should be dialled.
    ///
    /// `X-OAGW-Target-Host` pins a specific endpoint on upstreams whose
    /// endpoints share the same host suffix (DESIGN §3.2 routing).
    ///
    /// # Errors
    ///
    /// Returns `MissingTargetHost`, `InvalidTargetHost` or `UnknownTargetHost`.
    pub fn target_endpoint(&self, target_host: Option<&str>) -> DomainResult<&Endpoint> {
        let endpoints = &self.upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(DomainError::ProtocolError(format!(
                "upstream {:?} has no endpoints configured",
                self.upstream.alias
            )));
        }
        let Some(target_host) = target_host else {
            // A common-suffix alias names a set of interchangeable hosts
            // (`vendor.com` → `us.`/`eu.vendor.com`), which only the caller can
            // disambiguate (ADR-0001 "header required").
            if is_common_suffix_alias(&self.upstream.alias, endpoints) {
                return Err(DomainError::MissingTargetHost {
                    alias: self.upstream.alias.clone(),
                });
            }
            // Otherwise the pool is load-shared: one member per request in turn
            // (ADR-0001 round-robin) instead of always answering from
            // `endpoints[0]`.
            let index = self.pool_cursor.fetch_add(1, Ordering::Relaxed) % endpoints.len();
            return Ok(&endpoints[index]);
        };
        let host = crate::infra::ssrf::canonical_host(target_host);
        if host.is_empty()
            || host
                .chars()
                .any(|character| !(character.is_ascii_alphanumeric() || ".-:".contains(character)))
            || host.contains('/')
        {
            return Err(DomainError::InvalidTargetHost {
                value: target_host.to_owned(),
            });
        }
        endpoints
            .iter()
            .find(|endpoint| {
                crate::infra::ssrf::canonical_host(&endpoint.host) == host
                    || endpoint.host.eq_ignore_ascii_case(target_host)
            })
            .ok_or(DomainError::UnknownTargetHost {
                value: target_host.to_owned(),
            })
    }

    /// `true` when the matched upstream speaks a protocol this build proxies.
    #[must_use]
    pub const fn is_http(&self) -> bool {
        matches!(self.upstream.protocol, Protocol::Http)
    }
}

/// `true` when `alias` is a common suffix of every endpoint host.
///
/// ADR-0001 distinguishes an explicit alias (round-robin over the pool) from a
/// common-suffix alias, whose members are only addressable through an explicit
/// `X-OAGW-Target-Host`.
#[must_use]
pub fn is_common_suffix_alias(alias: &str, endpoints: &[Endpoint]) -> bool {
    if endpoints.len() < 2 {
        return false;
    }
    let alias = alias.trim().to_ascii_lowercase();
    if alias.is_empty() {
        return false;
    }
    let suffixed = |host: &str| {
        let host = host.trim().to_ascii_lowercase();
        host == alias
            || host
                .strip_suffix(&alias)
                .is_some_and(|rest| rest.ends_with('.'))
    };
    endpoints.iter().all(|endpoint| suffixed(&endpoint.host))
}

/// Data-plane facade over the repositories and the hierarchy port.
pub struct ProxyService<U, R, H, L> {
    upstreams: Arc<U>,
    routes: Arc<R>,
    hierarchy: Arc<H>,
    limiter: Arc<L>,
    config: Arc<OagwConfig>,
    /// Shared round-robin cursor for endpoint pools (ADR-0001 selection).
    pool_cursor: Arc<AtomicUsize>,
}

impl<U, R, H, L> ProxyService<U, R, H, L>
where
    U: UpstreamRepository,
    R: RouteRepository,
    H: TenantHierarchy,
    L: RateLimitStore,
{
    /// Creates a proxy service.
    #[must_use]
    pub fn new(
        upstreams: Arc<U>,
        routes: Arc<R>,
        hierarchy: Arc<H>,
        limiter: Arc<L>,
        config: Arc<OagwConfig>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            hierarchy,
            limiter,
            config,
            pool_cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The gear configuration.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The rate-limit store, so the wire layer can report counters.
    #[must_use]
    pub fn limiter(&self) -> &L {
        &self.limiter
    }

    /// Resolves the closest enabled upstream for `alias` on the tenant chain.
    ///
    /// # Errors
    ///
    /// Returns `RouteNotFound` when no level of the chain owns an enabled
    /// upstream with that alias.
    pub async fn resolve_upstream(&self, tenant_id: Uuid, alias: &str) -> DomainResult<Upstream> {
        let chain = self.hierarchy.chain(tenant_id).await?;
        for level in chain {
            if let Some(upstream) = self.upstreams.find_by_alias(level, alias).await? {
                if upstream.enabled {
                    return Ok(upstream);
                }
                // Shadowing: a closer level wins even when disabled, so the
                // walk stops rather than falling through to an ancestor.
                return Err(DomainError::RouteNotFound(alias.to_owned()));
            }
        }
        Err(DomainError::RouteNotFound(alias.to_owned()))
    }

    /// Resolves the full request configuration.
    ///
    /// # Errors
    ///
    /// Returns `RouteNotFound` for an unknown alias or a request no route
    /// covers, and the routing errors of [`ResolvedRequest::target_endpoint`]
    /// when the alias is ambiguous.
    pub async fn resolve(
        &self,
        tenant_id: Uuid,
        alias: &str,
        method: HttpMethod,
        path: &str,
        query: Option<&str>,
        target_host: Option<&str>,
    ) -> DomainResult<ResolvedRequest> {
        if !crate::infra::proxy::is_canonical_path(path) {
            return Err(DomainError::RouteRejected(format!(
                "request path {path:?} leaves the proxy root"
            )));
        }
        let upstream = self.resolve_upstream(tenant_id, alias).await?;
        if !matches!(upstream.protocol, Protocol::Http) {
            return Err(DomainError::ProtocolError(
                "gRPC proxying is catalogued but not reachable in this phase".to_owned(),
            ));
        }
        let chain = self.hierarchy.chain(tenant_id).await?;
        let mut routes = Vec::new();
        for level in chain {
            for route in self.routes.list_by_tenant(level, Some(upstream.id)).await? {
                if route.enabled {
                    routes.push(route);
                }
            }
        }
        let matched = select_route(&routes, &method, path);
        let mut resolved = match matched {
            Some((route, suffix)) => {
                // DESIGN §4.4 guard rules, evaluated against the matched route.
                enforce_match_rules(&route, &suffix, query)?;
                effective(&upstream, Some(&route), suffix)
            }
            // A request with no matching route still honours the upstream's own
            // policy so headers, auth and rate limits apply; the upstream path
            // is then the request path itself.
            None => effective(&upstream, None, path.to_owned()),
        };
        // Only the parameters the matched route admits travel upstream.
        resolved.query = allowed_query(resolved.route.as_ref(), query);
        resolved.pool_cursor = Arc::clone(&self.pool_cursor);
        // Fail fast on an ambiguous alias before any plugin runs.
        resolved.target_endpoint(target_host)?;
        Ok(resolved)
    }

    /// Charges the request against its configured counter.
    ///
    /// # Errors
    ///
    /// Returns `RateLimitExceeded` with the wire header values.
    pub fn charge_rate(
        &self,
        resolved: &ResolvedRequest,
        tenant_id: Uuid,
        subject_id: &str,
        client_ip: &str,
    ) -> DomainResult<Option<RateVerdict>> {
        let Some(config) = resolved.rate_limit.as_ref() else {
            return Ok(None);
        };
        let key = CounterKey::for_scope(
            config.scope,
            &tenant_id.to_string(),
            subject_id,
            client_ip,
            &resolved
                .route
                .as_ref()
                .map(|route| route.id.to_string())
                .unwrap_or_default(),
        );
        let verdict = self.limiter.charge(key, config, config.cost)?;
        Ok(Some(verdict))
    }
}

/// Picks the route that covers `(method, path)`: the highest priority wins,
/// ties broken by the longest path prefix (DESIGN §3.6 match determinism).
///
/// Returns the route and the request-path remainder after the match key.
#[must_use]
pub fn select_route(routes: &[Route], method: &HttpMethod, path: &str) -> Option<(Route, String)> {
    let mut candidates: Vec<(&Route, String)> = routes
        .iter()
        .filter(|route| match &route.r#match {
            // gRPC matches never apply to HTTP requests (Phase 3).
            MatchConfig::Http(http) => {
                http.methods.iter().any(|allowed| allowed == method)
                    && split_suffix(&http.path, path).is_some()
            }
            MatchConfig::Grpc(_) => false,
        })
        .filter_map(|route| {
            let MatchConfig::Http(http) = &route.r#match else {
                return None;
            };
            split_suffix(&http.path, path).map(|suffix| (route, suffix))
        })
        .collect();
    candidates.sort_by(|(left, _), (right, _)| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| match_key(right).len().cmp(&match_key(left).len()))
            .then_with(|| left.id.as_bytes().cmp(right.id.as_bytes()))
    });
    candidates
        .into_iter()
        .next()
        .map(|(route, suffix)| (route.clone(), suffix))
}

fn match_key(route: &Route) -> &str {
    match &route.r#match {
        MatchConfig::Http(http) => http.path.as_str(),
        MatchConfig::Grpc(_) => "",
    }
}

/// Cuts the request path into the match key and the remainder, honouring the
/// route's suffix mode.
fn split_suffix(match_path: &str, request_path: &str) -> Option<String> {
    let match_path = if match_path.is_empty() {
        "/"
    } else {
        match_path
    };
    if request_path == match_path {
        return Some(String::new());
    }
    let remainder = if match_path.ends_with('/') {
        request_path.strip_prefix(match_path)?
    } else {
        // A match key covers whole path segments only: `/api/v1/admin` must
        // never swallow `/api/v1/administrators`.
        let remainder = request_path.strip_prefix(match_path)?;
        remainder.strip_prefix('/')?
    };
    Some(remainder.to_owned())
}

/// Applies the guard rules DESIGN §4.4 attaches to a matched route.
///
/// # Errors
///
/// [`DomainError::RouteRejected`] when the route's suffix mode refuses the
/// request's suffix, or when the request carries a query parameter the route
/// does not admit.
pub fn enforce_match_rules(route: &Route, suffix: &str, query: Option<&str>) -> DomainResult<()> {
    let crate::domain::model::MatchConfig::Http(http) = &route.r#match else {
        return Ok(());
    };
    if http.path_suffix_mode == crate::domain::model::PathSuffixMode::Disabled && !suffix.is_empty()
    {
        return Err(DomainError::RouteRejected(format!(
            "route {} does not accept a path suffix",
            http.path
        )));
    }
    if let Some(query) = query {
        for name in query_param_names(query) {
            if !http
                .query_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&name))
            {
                return Err(DomainError::RouteRejected(format!(
                    "query parameter {name:?} is not allowed by route {}",
                    http.path
                )));
            }
        }
    }
    Ok(())
}

/// Splits a query string into its parameter names.
#[must_use]
pub fn query_param_names(query: &str) -> Vec<String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            percent_decode(pair.split('=').next().unwrap_or_default())
                .unwrap_or_else(|| pair.split('=').next().unwrap_or_default().to_owned())
        })
        .collect()
}

/// Rebuilds the query string from the parameters a route admits.
///
/// A route with no allowlist admits none, per `route.v1.schema.json` ("If
/// empty, allow none"); a request with no matched route keeps its query.
#[must_use]
pub fn allowed_query(route: Option<&Route>, query: Option<&str>) -> Option<String> {
    let query = query?;
    let Some(route) = route else {
        return Some(query.to_owned());
    };
    let crate::domain::model::MatchConfig::Http(http) = &route.r#match else {
        return Some(query.to_owned());
    };
    let kept: Vec<String> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let raw = pair.split('=').next().unwrap_or_default();
            let name = percent_decode(raw).unwrap_or_else(|| raw.to_owned());
            http.query_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&name))
        })
        .map(ToOwned::to_owned)
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

/// Percent-decodes a query token; `None` when the escape is malformed.
#[must_use]
pub fn percent_decode(token: &str) -> Option<String> {
    let bytes = token.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes.get(index + 1..index + 3)?;
                let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                out.push(value);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Builds the effective configuration for one hop.
#[must_use]
pub fn effective(
    upstream: &Upstream,
    route: Option<&Route>,
    path_suffix: String,
) -> ResolvedRequest {
    let route_cors = route
        .and_then(|route| route.cors.as_ref())
        .map(CorsConfig::from);
    let Some(route) = route else {
        return ResolvedRequest {
            upstream: upstream.clone(),
            route: None,
            auth: upstream.auth.clone(),
            plugins: upstream.plugins.clone(),
            headers: upstream.headers.clone(),
            rate_limit: upstream.rate_limit.clone(),
            cors: upstream.cors.clone(),
            tags: upstream.tags.clone(),
            path_suffix,
            query: None,
            pool_cursor: Arc::new(AtomicUsize::new(0)),
        };
    };
    ResolvedRequest {
        upstream: upstream.clone(),
        route: Some(route.clone()),
        // Auth and header transformation are upstream-level concerns: a route
        // cannot re-authenticate an upstream it belongs to.
        auth: upstream.auth.clone(),
        plugins: merge_plugins(&upstream.plugins, &route.plugins),
        headers: upstream.headers.clone(),
        rate_limit: merge_rate_limit(upstream.rate_limit.as_ref(), route.rate_limit.as_ref()),
        cors: merge_cors(upstream.cors.as_ref(), route_cors.as_ref()),
        tags: merge_tags(&upstream.tags, &route.tags),
        path_suffix,
        query: None,
        pool_cursor: Arc::new(AtomicUsize::new(0)),
    }
}

/// `true` when the merged policy pins a value the request cannot bypass.
#[must_use]
pub fn pinned(config: &crate::domain::model::Sharing) -> bool {
    is_enforced(*config)
}

/// Splits a proxy request path into the alias and the remainder.
///
/// `/proxy/api.openai.com:8080/v1/chat` → `("api.openai.com:8080", "v1/chat")`.
#[must_use]
pub fn split_proxy_path(tail: &str) -> Option<(String, String)> {
    let tail = tail.trim_start_matches('/');
    let (alias, rest) = match tail.split_once('/') {
        Some((alias, rest)) => (alias, rest),
        None => (tail, ""),
    };
    if alias.is_empty() {
        return None;
    }
    Some((alias.to_owned(), rest.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, HttpMatch, PathSuffixMode, Scheme, ServerConfig};

    struct StaticHierarchy(Vec<Uuid>);

    #[async_trait]
    impl TenantHierarchy for StaticHierarchy {
        async fn chain(&self, _tenant_id: Uuid) -> DomainResult<Vec<Uuid>> {
            Ok(self.0.clone())
        }
    }

    struct RecordingLimiter;

    impl RateLimitStore for RecordingLimiter {
        fn charge(
            &self,
            _key: CounterKey,
            _config: &RateLimitConfig,
            _cost: u64,
        ) -> DomainResult<RateVerdict> {
            Ok(RateVerdict {
                limit: 1,
                remaining: 1,
                reset_seconds: 1,
            })
        }
    }

    fn upstream(alias: &str, host: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            protocol: Protocol::Http,
            enabled: true,
            server: ServerConfig {
                endpoints: vec![
                    Endpoint {
                        scheme: Scheme::Http,
                        host: host.to_owned(),
                        port: 8080,
                    },
                    Endpoint {
                        scheme: Scheme::Http,
                        host: "backup.example".to_owned(),
                        port: 8080,
                    },
                ],
            },
            auth: AuthConfig::default(),
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn route(upstream_id: Uuid, method: HttpMethod, path: &str, priority: i64) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Some(upstream_id),
            r#match: MatchConfig::Http(HttpMatch {
                methods: vec![method],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            priority,
            enabled: true,
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn proxy_paths_split_into_alias_and_suffix() {
        let (alias, rest) = split_proxy_path("api.openai.com:8080/v1/chat").expect("split");
        assert_eq!(alias, "api.openai.com:8080");
        assert_eq!(rest, "v1/chat");
        assert!(split_proxy_path("").is_none());
    }

    #[test]
    fn longest_prefix_wins_and_cuts_the_suffix() {
        let id = Uuid::new_v4();
        let short = route(id, HttpMethod::Get, "/v1", 0);
        let long = route(id, HttpMethod::Get, "/v1/chat", 0);
        let routes = vec![short.clone(), long.clone()];
        let (selected, suffix) =
            select_route(&routes, &HttpMethod::Get, "/v1/chat/completions").expect("matched");
        assert_eq!(selected.id, long.id);
        assert_eq!(suffix, "completions");
        let (_, suffix) = select_route(&routes, &HttpMethod::Get, "/v1/models").expect("matched");
        assert_eq!(suffix, "models");
    }

    #[test]
    fn higher_priority_beats_a_longer_prefix() {
        let id = Uuid::new_v4();
        let short = route(id, HttpMethod::Get, "/v1", 10);
        let long = route(id, HttpMethod::Get, "/v1/chat", 0);
        let (selected, _) =
            select_route(&[long, short], &HttpMethod::Get, "/v1/chat/x").expect("matched");
        assert_eq!(selected.priority, 10);
    }

    #[test]
    fn other_methods_do_not_match() {
        let id = Uuid::new_v4();
        let only_get = route(id, HttpMethod::Get, "/v1", 0);
        assert!(select_route(&[only_get], &HttpMethod::Post, "/v1/x").is_none());
    }

    #[test]
    fn grpc_routes_never_match_http_requests() {
        let mut grpc = route(Uuid::new_v4(), HttpMethod::Get, "/v1", 0);
        grpc.r#match = MatchConfig::Grpc(crate::domain::model::GrpcMatch {
            service: "pkg.Service".to_owned(),
            method: "Call".to_owned(),
        });
        assert!(select_route(&[grpc], &HttpMethod::Get, "/v1/x").is_none());
    }

    #[test]
    fn a_single_endpoint_needs_no_target_host_hint() {
        let mut single = upstream("api.example:8080", "api.example");
        single.server.endpoints.truncate(1);
        let resolved = effective(&single, None, String::new());
        let endpoint = resolved.target_endpoint(None).expect("endpoint");
        assert_eq!(endpoint.host, "api.example");
    }

    #[test]
    fn a_common_suffix_alias_still_requires_the_target_host() {
        // ADR-0001: `vendor.com` over `us.`/`eu.vendor.com` cannot be resolved
        // without the caller naming the endpoint.
        let mut multi = upstream("vendor.com", "vendor.com");
        multi.server.endpoints = vec![
            crate::domain::model::Endpoint {
                scheme: crate::domain::model::Scheme::Http,
                host: "us.vendor.com".to_owned(),
                port: 443,
            },
            crate::domain::model::Endpoint {
                scheme: crate::domain::model::Scheme::Http,
                host: "eu.vendor.com".to_owned(),
                port: 443,
            },
        ];
        let resolved = effective(&multi, None, String::new());
        assert!(matches!(
            resolved.target_endpoint(None),
            Err(DomainError::MissingTargetHost { .. })
        ));
        assert!(resolved.target_endpoint(Some("eu.vendor.com")).is_ok());
    }

    #[test]
    fn an_unpinned_request_rotates_over_the_endpoint_pool() {
        let multi = upstream("api.example:8080", "api.example");
        let mut resolved = effective(&multi, None, String::new());
        let cursor = Arc::new(AtomicUsize::new(0));
        resolved.pool_cursor = Arc::clone(&cursor);
        let hosts: Vec<String> = (0..3)
            .map(|_| {
                resolved
                    .target_endpoint(None)
                    .expect("endpoint")
                    .host
                    .clone()
            })
            .collect();
        // Two members, three picks: the pool cycles instead of always answering
        // from the first endpoint (ADR-0001 endpoint selection).
        assert_eq!(hosts, ["api.example", "backup.example", "api.example"]);
        // An explicit hint still pins the member and overrides the rotation.
        let pinned = resolved
            .target_endpoint(Some("backup.example"))
            .expect("endpoint");
        assert_eq!(pinned.host, "backup.example");
        assert!(matches!(
            resolved.target_endpoint(Some("nowhere.example")),
            Err(DomainError::UnknownTargetHost { .. })
        ));
        assert!(matches!(
            resolved.target_endpoint(Some("bad host")),
            Err(DomainError::InvalidTargetHost { .. })
        ));
    }

    #[test]
    fn a_multi_port_single_host_pool_is_unambiguous() {
        let mut multi = upstream("api.example:8080", "api.example");
        multi.server.endpoints.push(crate::domain::model::Endpoint {
            scheme: crate::domain::model::Scheme::Http,
            host: "api.example".to_owned(),
            port: 8081,
        });
        let resolved = effective(&multi, None, String::new());
        // Both members share a host, so a pinned host is accepted either way.
        assert!(resolved.target_endpoint(Some("api.example")).is_ok());
    }

    #[tokio::test]
    async fn resolves_the_closest_enabled_upstream() {
        let ancestor = Uuid::new_v4();
        let descendant = Uuid::new_v4();
        let store = std::sync::Arc::new(crate::infra::storage::InMemoryStore::new());
        let mut inherited = upstream("api.example:8080", "api.example");
        inherited.tenant_id = ancestor;
        crate::domain::repo::UpstreamRepository::insert(&*store, inherited.clone())
            .await
            .expect("inserted");
        let mut own = upstream("api.example:8080", "own.example");
        own.tenant_id = descendant;
        crate::domain::repo::UpstreamRepository::insert(&*store, own.clone())
            .await
            .expect("inserted");
        let service = ProxyService::new(
            store.clone(),
            store.clone(),
            Arc::new(StaticHierarchy(vec![descendant, ancestor])),
            Arc::new(RecordingLimiter),
            Arc::new(OagwConfig::default()),
        );
        // The descendant's own upstream shadows the ancestor's.
        let resolved = service
            .resolve(
                descendant,
                "api.example:8080",
                HttpMethod::Get,
                "/v1",
                None,
                Some("own.example"),
            )
            .await
            .expect("resolved");
        assert_eq!(resolved.upstream.id, own.id);
        // A tenant without its own upstream inherits the ancestor's.
        let inherited_service = ProxyService::new(
            store.clone(),
            store.clone(),
            Arc::new(StaticHierarchy(vec![ancestor])),
            Arc::new(RecordingLimiter),
            Arc::new(OagwConfig::default()),
        );
        let resolved = inherited_service
            .resolve(
                descendant,
                "api.example:8080",
                HttpMethod::Get,
                "/v1",
                None,
                Some("api.example"),
            )
            .await
            .expect("resolved");
        assert_eq!(resolved.upstream.id, inherited.id);
        assert!(matches!(
            service
                .resolve(descendant, "absent:1", HttpMethod::Get, "/v1", None, None)
                .await,
            Err(DomainError::RouteNotFound(_))
        ));
    }
}
