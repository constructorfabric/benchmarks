//! Data-plane orchestration: alias → route → plugins → rate limit → upstream.
//!
//! The service owns every decision the proxy handler makes before a byte is
//! forwarded, so the handler stays a thin adapter over axum.

use std::sync::Arc;
use std::time::Duration;

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::cors;
use crate::domain::dto::{Cors, PathSuffixMode, Upstream};
use crate::domain::error::DomainError;
use crate::domain::layering::EffectiveConfig;
use crate::domain::matching;
use crate::domain::plugin::{GuardDecision, RequestContext, ResponseContext};
use crate::domain::ratelimit;
use crate::domain::repo::{RateKey, RateLimitStore};
use crate::domain::services::control_plane::ControlPlaneService;
use crate::infra::authz::TenantChain;
use crate::infra::plugin::PluginRegistry;
use crate::infra::proxy::circuit_breaker::CircuitBreakers;
use crate::infra::proxy::connector::{UpstreamConnector, UpstreamExchange, UpstreamRequest};

/// The configured, resolved and plugin-decorated proxy request.
#[derive(Debug)]
pub struct ProxyPlan {
    /// The resolved upstream.
    pub upstream: Upstream,
    /// Owning tenant of the resolved upstream.
    pub owner_tenant_id: uuid::Uuid,
    /// The tenant chain the resolution walked.
    pub chain: TenantChain,
    /// The effective configuration after layering.
    pub config: EffectiveConfig,
    /// The endpoint the request is routed to.
    pub endpoint_host: String,
    /// The endpoint port.
    pub endpoint_port: u16,
    /// Whether the endpoint speaks TLS.
    pub endpoint_tls: bool,
    /// The path forwarded upstream.
    pub forward_path: String,
    /// The query string forwarded upstream, when any.
    pub forward_query: Option<String>,
    /// Per-plugin configuration, `None` for a built-in bound without one.
    pub plugin_configs: std::collections::BTreeMap<String, Option<serde_json::Value>>,
    /// The whole effective configuration, for the response and error halves.
    pub plugin_bindings: std::collections::BTreeMap<String, Option<serde_json::Value>>,
}

/// Everything the proxy needs, wired once at gear start.
pub struct OagwDataPlane {
    control_plane: Arc<ControlPlaneService>,
    connector: UpstreamConnector,
    breakers: Arc<CircuitBreakers>,
    rate_limits: Arc<dyn RateLimitStore>,
    plugins: Arc<PluginRegistry>,
    config: Arc<OagwConfig>,
}

impl std::fmt::Debug for OagwDataPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwDataPlane")
            .field("connector", &self.connector)
            .field("breakers", &self.breakers)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OagwDataPlane {
    /// Wires the data plane over a control plane.
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        rate_limits: Arc<dyn RateLimitStore>,
        plugins: Arc<PluginRegistry>,
        config: Arc<OagwConfig>,
    ) -> Self {
        let breakers = CircuitBreakers::new(5, Duration::from_secs(30));
        Self {
            connector: UpstreamConnector::new(&config),
            breakers: Arc::new(breakers),
            rate_limits,
            plugins,
            config,
            control_plane,
        }
    }

    /// The plugin registry.
    pub fn plugins(&self) -> &Arc<PluginRegistry> {
        &self.plugins
    }

    /// The upstream connector, for transports the HTTP exchange does not cover.
    pub fn connector(&self) -> &UpstreamConnector {
        &self.connector
    }
    /// The gear configuration.
    pub fn config(&self) -> &Arc<OagwConfig> {
        &self.config
    }

    /// The circuit breakers.
    pub fn breakers(&self) -> &Arc<CircuitBreakers> {
        &self.breakers
    }

    /// Resolves an alias down the caller's tenant chain.
    pub async fn resolve(
        &self,
        chain: &TenantChain,
        requested_alias: &str,
    ) -> Result<Option<crate::domain::services::data_plane::ResolvedUpstream>, DomainError> {
        self.control_plane.resolve_alias(chain, requested_alias).await
    }

    /// Builds the plan for a request that has already been matched.
    pub async fn plan(
        &self,
        chain: &TenantChain,
        resolved: &crate::domain::services::data_plane::ResolvedUpstream,
        route: Option<&crate::domain::dto::Route>,
        _method: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<ProxyPlan, DomainError> {
        if !resolved.upstream.enabled {
            return Err(DomainError::LinkUnavailable {
                detail: format!("upstream `{}` is disabled", resolved.upstream.alias_str()),
                upstream_id: Some(ControlPlaneService::upstream_gts_id(&resolved.upstream)),
                alias: Some(resolved.upstream.alias_str().to_string()),
            });
        }
        let config = self
            .control_plane
            .effective_config(chain, resolved, route)
            .await?;
        if !config.enabled {
            return Err(DomainError::LinkUnavailable {
                detail: format!("upstream `{}` is disabled by an ancestor", resolved.upstream.alias_str()),
                upstream_id: Some(ControlPlaneService::upstream_gts_id(&resolved.upstream)),
                alias: Some(resolved.upstream.alias_str().to_string()),
            });
        }
        let (endpoint_host, endpoint_port, endpoint_tls) = select_endpoint(&resolved.upstream)?;
        // Guard rules: the route was selected by method and path, so a query
        // parameter or path suffix it does not admit is a validation error.
        if let Some(route) = route {
            if !matching::query_allowed(route, query) {
                return Err(DomainError::ValidationError {
                    detail: "a query parameter is not in the route's allowlist".to_string(),
                });
            }
        }
        let forward_path = route
            .and_then(|r| r.match_rule.http.as_ref())
            .map(|http| {
                let suffix = matching::path_suffix(&http.path, path);
                // A route that forbids a suffix still matched the request (the
                // mode is not a selection criterion), so the rejection is a
                // validation error, not a route miss.
                if http.path_suffix_mode == PathSuffixMode::Disabled && !suffix.is_empty() {
                    return Err(DomainError::ValidationError {
                        detail: format!(
                            "route `{}` does not accept a path suffix, but `/{} ` was supplied",
                            http.path, suffix
                        ),
                    });
                }
                Ok(matching::MatchOutcome {
                    route_id: String::new(),
                    route_path: http.path.clone(),
                    methods: Vec::new(),
                    query_allowlist: Vec::new(),
                    suffix_mode: http.path_suffix_mode,
                }
                .forward_path(&suffix))
            })
            .transpose()?
            .unwrap_or_else(|| path.to_string());
        let forward_query = if let Some(http) = route.and_then(|r| r.match_rule.http.as_ref()) {
            if http.query_allowlist.is_empty() {
                None
            } else {
                let allowed: Vec<String> = query
                    .iter()
                    .filter(|(k, _)| {
                        http.query_allowlist
                            .iter()
                            .any(|a| a.eq_ignore_ascii_case(k))
                    })
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                if allowed.is_empty() {
                    None
                } else {
                    Some(allowed.join("&"))
                }
            }
        } else {
            Some(
                query
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("&"),
            )
            .filter(|s| !s.is_empty())
        };
        let plugin_configs = self
            .plugin_configs(resolved.owner_tenant_id, &config)
            .await;
        Ok(ProxyPlan {
            upstream: resolved.upstream.clone(),
            owner_tenant_id: resolved.owner_tenant_id,
            chain: chain.clone(),
            config,
            endpoint_host,
            endpoint_port,
            endpoint_tls,
            forward_path,
            forward_query,
            plugin_configs: plugin_configs.clone(),
            plugin_bindings: plugin_configs,
        })
    }

    /// The effective CORS policy of a plan.
    pub fn cors_of<'a>(&self, plan: &'a ProxyPlan) -> Option<&'a Cors> {
        plan.config.cors.as_ref()
    }

    /// Whether an actual request may be sent cross-origin.
    ///
    /// A request with no `Origin` header is same-origin by definition; an
    /// upstream with no CORS policy admits every origin.
    pub fn origin_allowed(&self, plan: &ProxyPlan, origin: Option<&str>) -> bool {
        match origin {
            None => true,
            Some(origin) => match self.cors_of(plan) {
                Some(policy) => cors::origin_allowed(policy, origin),
                None => false,
            },
        }
    }

    /// Runs the auth, guard and transform chain over a request.
    pub async fn run_request_plugins(
        &self,
        plan: &ProxyPlan,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        for plugin_id in plan.config.plugins.clone() {
            ctx.plugin_config = plan.plugin_configs.get(&plugin_id).cloned().flatten();
            if let Some(auth) = self.plugins.auth(&plugin_id) {
                auth.authenticate(ctx).await.map_err(|err| match err {
                    // A credential injection that did not happen is an
                    // authentication failure: the 401 row the error contract
                    // documents.
                    crate::domain::plugin::PluginError::Failure { plugin_id, message } => {
                        DomainError::AuthenticationFailed {
                            detail: message,
                            plugin_id: Some(plugin_id),
                        }
                    }
                    crate::domain::plugin::PluginError::Reject(error) => error,
                })?;
                continue;
            }
            if let Some(guard) = self.plugins.guard(&plugin_id) {
                match guard.guard_request(ctx).await {
                    Ok(GuardDecision::Next) => {}
                    Ok(GuardDecision::Reject(err)) => return Err(err),
                    Err(err) => return Err(err.into()),
                }
                continue;
            }
            if let Some(transform) = self.plugins.transform(&plugin_id) {
                transform.transform_request(ctx).await.map_err(DomainError::from)?;
                continue;
            }
            // DESIGN §"Resolution Algorithm": a UUID instance addresses a
            // persisted, tenant-owned plugin, which is resolved through the
            // plugin store; anything else resolves through the registry. A
            // persisted plugin this gear cannot execute is still a loud
            // `PluginNotFound`, never a silent skip.
            if crate::domain::gts_helpers::plugin_uuid(&plugin_id).is_some() {
                let resolved = self
                    .resolve_persisted(&plan.owner_tenant_id, &plugin_id)
                    .await;
                if let Some(record) = resolved {
                    tracing::warn!(
                        plugin = %record.name,
                        kind = ?record.kind,
                        "custom plugins are catalogued but not executable in this gear"
                    );
                }
            }
            return Err(DomainError::PluginNotFound { plugin_id });
        }
        Ok(())
    }

    /// Runs the response half of the transform and guard chain.
    ///
    /// Guards run here too: an upstream response that omits a required header
    /// is refused (502) rather than handed to the caller. A guard that rejects
    /// raises its own error, so the caller renders the problem instead of the
    /// upstream's response.
    pub async fn run_response_plugins(
        &self,
        plan: &ProxyPlan,
        ctx: &mut ResponseContext,
        request_attributes: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DomainError> {
        ctx.attributes = request_attributes.clone();
        for plugin_id in plan.config.plugins.iter().rev() {
            ctx.plugin_config = plan.plugin_bindings.get(plugin_id).cloned().flatten();
            if let Some(guard) = self.plugins.guard(plugin_id) {
                match guard.guard_response(ctx).await {
                    Ok(GuardDecision::Next) => {}
                    Ok(GuardDecision::Reject(err)) => return Err(err),
                    Err(err) => return Err(err.into()),
                }
            }
            if let Some(transform) = self.plugins.transform(plugin_id) {
                transform.transform_response(ctx).await.map_err(DomainError::from)?;
            }
        }
        Ok(())
    }

    /// Runs the error half of the transform chain, in reverse, over a gateway
    /// failure. A transform that itself fails is logged and skipped: the
    /// original error must still be rendered.
    pub async fn run_error_plugins(
        &self,
        plan: &ProxyPlan,
        ctx: &mut crate::domain::plugin::ErrorContext,
        request_attributes: &std::collections::BTreeMap<String, String>,
    ) {
        ctx.attributes = request_attributes.clone();
        for plugin_id in plan.config.plugins.iter().rev() {
            let Some(transform) = self.plugins.transform(plugin_id) else {
                continue;
            };
            if let Err(err) = transform.transform_error(ctx).await {
                tracing::warn!(plugin = %plugin_id, error = %err, "error transform failed");
            }
        }
    }

    /// Resolves a UUID-backed plugin reference through the plugin store.
    async fn resolve_persisted(
        &self,
        tenant_id: &uuid::Uuid,
        plugin_id: &str,
    ) -> Option<crate::domain::repo::PluginRecord> {
        let uuid = crate::domain::gts_helpers::plugin_uuid(plugin_id)?;
        self.control_plane
            .plugins()
            .get(*tenant_id, &uuid.to_string())
            .await
            .ok()
            .flatten()
    }

    /// Enforces the effective rate limit, returning the snapshot to report.
    pub async fn enforce_rate_limit(
        &self,
        plan: &ProxyPlan,
        ctx: &RequestContext,
    ) -> Result<crate::domain::error::RateLimitSnapshot, DomainError> {
        let Some(limit) = plan.config.rate_limit.as_ref() else {
            return Ok(crate::domain::error::RateLimitSnapshot {
                limit: 0,
                remaining: 0,
                reset: 0,
                retry_after: 0,
            });
        };
        let scope_value = ratelimit::scope_value(
            limit.scope,
            &ctx.tenant_id.clone().unwrap_or_default(),
            &ctx.principal_id.clone().unwrap_or_default(),
            &ctx.client_ip.clone().unwrap_or_default(),
            &ctx.path.clone().unwrap_or_default(),
        );
        let key = RateKey {
            bucket: plan.config.upstream_id.clone(),
            scope: format!("{:?}:{}", limit.scope, scope_value),
        };
        let outcome = self
            .rate_limits
            .try_take(key, limit.capacity, limit.refill_rate, limit.cost)
            .await?;
        match outcome {
            crate::domain::repo::RateLimitOutcome::Acquired(snapshot) => {
                crate::infra::metrics::record_rate_limit_usage(
                    &plan.endpoint_host,
                    &ctx.path.clone().unwrap_or_default(),
                    limit.capacity,
                    snapshot.remaining,
                );
                Ok(snapshot)
            }
            crate::domain::repo::RateLimitOutcome::Exceeded(snapshot) => {
                crate::infra::metrics::record_rate_limit_exceeded(
                    &plan.endpoint_host,
                    &ctx.path.clone().unwrap_or_default(),
                );
                Err(DomainError::RateLimitExceeded {
                    snapshot,
                    host: Some(plan.endpoint_host.clone()),
                    path: ctx.path.clone(),
                    upstream_id: Some(plan.config.upstream_id.clone()),
                })
            }
        }
    }

    /// Whether the circuit breaker permits a dial and, when not, for how long.
    pub fn breaker_allows(&self, host: &str) -> Option<Duration> {
        self.breakers.allows(host)
    }

    /// Records a successful exchange against the breaker.
    pub fn record_success(&self, host: &str) {
        self.breakers.record_success(host);
    }

    /// Records a failed exchange against the breaker.
    pub fn record_failure(&self, host: &str) {
        self.breakers.record_failure(host);
    }
    /// Sends the request upstream.
    pub async fn send(
        &self,
        plan: &ProxyPlan,
        request: UpstreamRequest,
    ) -> Result<UpstreamExchange, DomainError> {
        if self.breaker_allows(&plan.endpoint_host).is_some() {
            return Err(DomainError::CircuitBreakerOpen {
                host: Some(plan.endpoint_host.clone()),
            });
        }
        let peer = self
            .connector
            .peer(&plan.endpoint_host, plan.endpoint_port, plan.endpoint_tls)?;
        match self.connector.exchange(&peer, request).await {
            Ok(exchange) => {
                self.record_success(&plan.endpoint_host);
                Ok(exchange)
            }
            Err(err) => {
                self.record_failure(&plan.endpoint_host);
                Err(err)
            }
        }
    }

    /// The alias-normalisation helper, exposed for the handlers.
    pub fn normalise_alias(&self, value: &str) -> String {
        alias::normalise_alias(value)
    }

    /// Resolves the configuration of every bound plugin.
    ///
    /// A built-in identifier has no stored configuration; a custom plugin's
    /// configuration is what the tenant recorded when it created the plugin.
    async fn plugin_configs(
        &self,
        owner_tenant_id: uuid::Uuid,
        config: &crate::domain::layering::EffectiveConfig,
    ) -> std::collections::BTreeMap<String, Option<serde_json::Value>> {
        let mut resolved = std::collections::BTreeMap::new();
        for binding in &config.plugins {
            if resolved.contains_key(binding) {
                continue;
            }
            // The configuration the binding itself carried (ADR 0009) wins
            // over the one the plugin record holds.
            let from_binding = config.plugin_configs.get(binding).cloned();
            let from_record = match crate::domain::dto::parse_plugin_ref(binding) {
                Some(crate::domain::dto::PluginRef::Custom(uuid)) => {
                    let record = self
                        .control_plane
                        .plugins()
                        .get(owner_tenant_id, &uuid.to_string())
                        .await
                        .ok()
                        .flatten();
                    record.map(|r| r.config)
                }
                Some(crate::domain::dto::PluginRef::Builtin(_)) => None,
                None => None,
            };
            resolved.insert(
                binding.clone(),
                from_binding.or(from_record).filter(|value| !value.is_null()),
            );
        }
        resolved
    }
}

/// Picks the endpoint a request is routed to: the first of the pool.
///
/// Pooling is a listing concern — the gear dials the first healthy endpoint and
/// the breaker rejects the host when it fails.
pub fn select_endpoint(upstream: &Upstream) -> Result<(String, u16, bool), DomainError> {
    let endpoint = upstream
        .server
        .endpoints
        .first()
        .ok_or_else(|| DomainError::MissingTargetHost {
            alias: upstream.alias_str().to_string(),
            valid_hosts: Vec::new(),
            upstream_id: upstream.id.clone().unwrap_or_default(),
        })?;
    Ok((
        alias::normalise_host(&endpoint.host),
        endpoint.effective_port(),
        endpoint.scheme.is_tls(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Endpoint, EndpointScheme, Server};

    fn upstream(host: &str, scheme: EndpointScheme) -> Upstream {
        Upstream {
            id: Some("u1".to_string()),
            enabled: true,
            alias: Some("vendor.com".to_string()),
            tags: Vec::new(),
            server: Server {
                endpoints: vec![Endpoint {
                    scheme,
                    host: host.to_string(),
                    port: 0,
                }],
            },
            protocol: crate::domain::dto::Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn the_first_endpoint_is_selected() {
        let (host, port, tls) = select_endpoint(&upstream("API.Vendor.com", EndpointScheme::Https))
            .unwrap();
        assert_eq!(host, "api.vendor.com");
        assert_eq!(port, 443);
        assert!(tls);
    }

    #[test]
    fn an_http_endpoint_is_not_tls() {
        let (_, port, tls) = select_endpoint(&upstream("api.vendor.com", EndpointScheme::Http))
            .unwrap();
        assert_eq!(port, 80);
        assert!(!tls);
    }

    #[test]
    fn an_upstream_without_endpoints_cannot_be_routed() {
        let mut u = upstream("api.vendor.com", EndpointScheme::Https);
        u.server.endpoints.clear();
        assert!(select_endpoint(&u).is_err());
    }

    #[test]
    fn an_unknown_plugin_binding_is_reported() {
        let registry = Arc::new(PluginRegistry::builtin());
        assert!(registry.auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nope.v1").is_none());
        assert!(registry.guard("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nope.v1").is_none());
        assert_eq!(
            crate::domain::gts_helpers::plugin_short_name(
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            ),
            "apikey"
        );
    }
}
