//! Alias and route resolution for the proxy data plane (R3 steps 1-3, R4, R5).
//!
//! Resolution is the control-plane half of a proxied request: the caller's
//! tenant chain is walked from the closest tenant to the root, the first
//! upstream registered under the request alias wins, its configuration is
//! folded with its ancestors' (DESIGN.md §3.2 "Shadowing Behavior") and the
//! routes declared for it are matched against the request.
//!
//! Every failure this module returns is a gateway problem: an unknown alias and
//! an unmatched route are the 404 `cf.oagw.route.not_found.v1` problem of R5, a
//! disabled upstream is the 503 `cf.oagw.link.unavailable.v1` problem of the
//! DESIGN.md §3.3 error table.

use uuid::Uuid;

use crate::domain::merge::{effective_rate_limit, merge_cors, plugin_chain};
use crate::domain::model::{CorsConfig, PluginRef, RateLimitConfig, Route, Upstream};
use crate::domain::store::ConfigService;
use crate::error::{GatewayError, GatewayErrorKind};
use crate::proxy::matcher::{self, RouteMatch};

/// The tenant chain of a request: the caller's tenant first, then its
/// ancestors up to the root.
///
/// The tenant hierarchy itself is owned by the platform's tenant resolver, so
/// OAGW consumes it as an ordered list. Until a hierarchy source is wired into
/// the gear, the chain is the caller's own tenant — the closest match — which
/// keeps single-tenant deployments correct without pretending to know about
/// ancestors that the gear cannot see.
#[must_use]
pub fn tenant_chain(caller_tenant: Uuid) -> Vec<Uuid> {
    vec![caller_tenant]
}

/// Normalizes the `{alias}` of a proxy URL to its stored form: ASCII lowercase
/// with the trailing dot stripped (DESIGN.md §3.1 "Alias Normalization").
#[must_use]
pub fn normalize_alias(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// An alias resolved to an upstream, with the configuration the request is
/// served under.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The effective upstream: the closest match with its ancestors' policy
    /// folded in.
    pub upstream: Upstream,
    /// The effective CORS configuration of the upstream and of the matched
    /// route (ADR 0004 "Hierarchical configuration").
    pub cors: CorsConfig,
    /// The effective rate limit: the stricter of the upstream's and the
    /// route's.
    pub rate_limit: Option<RateLimitConfig>,
}

/// Resolves `{alias}` to the upstream a request is served by (R3 step 1, R4).
///
/// The tenant chain is walked from the closest tenant to the root and the
/// closest upstream registered under `alias` is selected; its configuration is
/// folded with the configurations its ancestors declare for the same alias.
///
/// # Errors
///
/// Returns 404 `cf.oagw.route.not_found.v1` when no tenant of the chain owns
/// `alias`, and 503 `cf.oagw.link.unavailable.v1` when the resolved upstream is
/// disabled.
pub fn resolve_upstream(
    config: &ConfigService,
    chain: &[Uuid],
    alias: &str,
) -> Result<ResolvedUpstream, GatewayError> {
    let alias = normalize_alias(alias);

    if config.store().resolve_alias(chain, &alias).is_none() {
        return Err(unknown_alias(&alias));
    }

    let upstream = config
        .store()
        .effective_upstream(chain, &alias)
        .ok_or_else(|| unknown_alias(&alias))?;

    if !upstream.is_enabled() {
        return Err(disabled_upstream(&upstream));
    }

    let cors = upstream.config.cors.clone().unwrap_or_default();
    let rate_limit = upstream.config.rate_limit;

    Ok(ResolvedUpstream {
        upstream,
        cors,
        rate_limit,
    })
}

/// The routes declared for `upstream` across the tenant chain, root first.
///
/// The caller's own routes come last, so a declaration of the same
/// `(path, priority)` folds ancestor-first through [`merge_route_chain`].
#[must_use]
pub fn routes_of(config: &ConfigService, chain: &[Uuid], upstream_id: Uuid) -> Vec<Route> {
    let mut routes = Vec::new();

    for tenant_id in chain.iter().rev() {
        for route in config.store().list_routes(*tenant_id) {
            if route.upstream_id() == upstream_id {
                routes.push(route);
            }
        }
    }

    routes
}

/// Resolves the route a request is served by (R3 step 2, R4).
///
/// # Errors
///
/// Returns 404 `cf.oagw.route.not_found.v1` when no route of the resolved
/// upstream matches, and the guard errors of [`matcher::find_route`] when the
/// request carries a path suffix or a query parameter the route does not
/// accept.
pub fn resolve_route(
    config: &ConfigService,
    chain: &[Uuid],
    upstream: &Upstream,
    method: &axum::http::Method,
    path: &str,
    query: &str,
) -> Result<RouteMatch, GatewayError> {
    let routes = routes_of(config, chain, upstream.id);

    matcher::find_route(&routes, method, path, query)
}

/// Folds the CORS configuration a request is served under (R3 step 4): the
/// upstream's CORS configuration is the ancestor's, the matched route's is the
/// descendant's (ADR 0004 "Hierarchical configuration").
///
/// A route that declares no CORS at all leaves the upstream's configuration in
/// force — a route is not an implicit CORS opt-out.
#[must_use]
pub fn effective_cors(upstream: &CorsConfig, route: Option<&CorsConfig>) -> CorsConfig {
    match route {
        Some(route) => merge_cors(Some(upstream), Some(route)).unwrap_or_default(),
        None => upstream.clone(),
    }
}

/// Folds the rate limit a request is served under: `min(upstream, route)`
/// (DESIGN.md §3.1 "Shadowing Behavior").
#[must_use]
pub fn effective_limit(
    upstream: Option<RateLimitConfig>,
    route: Option<RateLimitConfig>,
) -> Option<RateLimitConfig> {
    effective_rate_limit(upstream.as_ref(), route.as_ref())
}

/// The plugin chain a request runs, upstream first then route (R3 step 4, ADR
/// 0002 "Plugin Sharing").
///
/// The chain is resolved here so the data plane has one place that knows which
/// plugins apply; executing them is the plugin registry's job and is not part
/// of this gear's data plane yet (R2).
#[must_use]
pub fn effective_plugins(upstream: &Upstream, route: &Route) -> Vec<PluginRef> {
    plugin_chain(&upstream.config.plugins, &route.config.plugins)
}

/// 404 for an alias no tenant of the chain registers.
fn unknown_alias(alias: &str) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::RouteNotFound,
        format!("no upstream is registered under the alias `{alias}`"),
    )
    .with_extension("alias", alias.to_owned())
}

/// 503 for an upstream that exists but does not accept traffic.
fn disabled_upstream(upstream: &Upstream) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::LinkUnavailable,
        format!(
            "upstream `{}` is disabled and does not accept traffic",
            upstream.alias()
        ),
    )
    .with_upstream_id(upstream.id.to_string())
    .with_extension("alias", upstream.alias().to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use uuid::Uuid;

    use super::*;

    const ROOT: &str = "00000000-0000-0000-0000-000000000001";
    const LEAF: &str = "00000000-0000-0000-0000-000000000003";

    fn service() -> ConfigService {
        ConfigService::new(crate::config::OagwConfig::default())
    }

    fn chain(tenants: &[&str]) -> Vec<Uuid> {
        tenants
            .iter()
            .map(|tenant| Uuid::parse_str(tenant).unwrap())
            .collect()
    }

    /// An upstream spec for an IP-literal pool: such a pool is not derivable,
    /// so the tests can name the alias freely.
    fn upstream(alias: &str, host: &str) -> crate::domain::model::UpstreamSpec {
        serde_json::from_value(serde_json::json!({
            "alias": alias,
            "protocol": crate::domain::model::PROTOCOL_HTTP,
            "server": { "endpoints": [{ "host": host, "port": 443 }] }
        }))
        .unwrap()
    }

    #[test]
    fn test_the_chain_is_the_caller_tenant() {
        let chain = tenant_chain(Uuid::nil());

        assert_eq!(chain, vec![Uuid::nil()]);
    }

    #[test]
    fn test_the_alias_is_normalized_before_lookup() {
        assert_eq!(normalize_alias(" API.OpenAI.com. "), "api.openai.com");
    }

    #[test]
    fn test_the_closest_tenant_wins_the_alias() {
        let config = service();
        let root = Uuid::parse_str(ROOT).unwrap();
        let leaf = Uuid::parse_str(LEAF).unwrap();

        config
            .create_upstream(root, &upstream("api.openai.com", "10.0.0.1"))
            .unwrap();
        config
            .create_upstream(leaf, &upstream("api.openai.com", "10.0.0.2"))
            .unwrap();

        let resolved =
            resolve_upstream(&config, &chain(&[LEAF, ROOT]), "api.openai.com").expect("resolved");

        assert_eq!(resolved.upstream.endpoints()[0].host.as_str(), "10.0.0.2");
    }

    #[test]
    fn test_an_ancestor_upstream_is_visible_to_a_descendant() {
        let config = service();
        let root = Uuid::parse_str(ROOT).unwrap();

        config
            .create_upstream(root, &upstream("vendor.com", "10.0.1.1"))
            .unwrap();

        let resolved =
            resolve_upstream(&config, &chain(&[LEAF, ROOT]), "vendor.com").expect("resolved");

        assert_eq!(resolved.upstream.tenant_id, Uuid::parse_str(ROOT).unwrap());
        assert_eq!(resolved.upstream.alias(), "vendor.com");
    }

    #[test]
    fn test_an_unknown_alias_is_a_404_route_problem() {
        let config = service();

        let error = resolve_upstream(&config, &chain(&[LEAF]), "no-such-alias").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::RouteNotFound);
        assert_eq!(error.status(), 404);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        );
    }

    #[test]
    fn test_a_disabled_upstream_is_a_503_link_problem() {
        let config = service();
        let leaf = Uuid::parse_str(LEAF).unwrap();

        let created = config
            .create_upstream(leaf, &upstream("vendor.com", "10.0.1.1"))
            .unwrap();
        config
            .replace_upstream(
                leaf,
                created.id,
                &serde_json::from_value(serde_json::json!({
                    "alias": "vendor.com",
                    "protocol": crate::domain::model::PROTOCOL_HTTP,
                    "enabled": false,
                    "server": { "endpoints": [{ "host": "10.0.1.1", "port": 443 }] }
                }))
                .unwrap(),
            )
            .unwrap();

        let error = resolve_upstream(&config, &chain(&[LEAF]), "vendor.com").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::LinkUnavailable);
        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        );
    }

    #[test]
    fn test_routes_are_collected_from_every_tenant_of_the_chain() {
        let config = service();
        let leaf = Uuid::parse_str(LEAF).unwrap();

        let upstream = config
            .create_upstream(leaf, &upstream("vendor.com", "10.0.1.1"))
            .unwrap();

        config
            .create_route(
                leaf,
                &serde_json::from_value(serde_json::json!({
                    "upstream_id": upstream.id,
                    "match": { "http": { "methods": ["GET"], "path": "/v1" } }
                }))
                .unwrap(),
            )
            .unwrap();

        let routes = routes_of(&config, &chain(&[LEAF, ROOT]), upstream.id);

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].upstream_id(), upstream.id);
    }

    #[test]
    fn test_a_route_of_the_resolved_upstream_is_matched() {
        let config = service();
        let leaf = Uuid::parse_str(LEAF).unwrap();

        let upstream = config
            .create_upstream(leaf, &upstream("vendor.com", "10.0.1.1"))
            .unwrap();
        config
            .create_route(
                leaf,
                &serde_json::from_value(serde_json::json!({
                    "upstream_id": upstream.id,
                    "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } }
                }))
                .unwrap(),
            )
            .unwrap();

        let resolved = resolve_upstream(&config, &chain(&[LEAF]), "vendor.com").expect("resolved");
        let matched = resolve_route(
            &config,
            &chain(&[LEAF]),
            &resolved.upstream,
            &axum::http::Method::GET,
            "/v1/chat/completions",
            "",
        )
        .expect("a route matches");

        assert_eq!(matched.upstream_path, "/v1/chat/completions");
    }

    #[test]
    fn test_effective_cors_unions_an_inherit_upstream_with_a_route() {
        let upstream: CorsConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "sharing": "inherit",
            "allowed_origins": ["https://console.example.com"],
            "allowed_methods": ["GET", "POST"]
        }))
        .unwrap();
        let route: CorsConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "sharing": "inherit",
            "allowed_origins": ["https://portal.example.com"]
        }))
        .unwrap();

        let effective = effective_cors(&upstream, Some(&route));

        assert!(
            effective
                .allowed_origins
                .contains(&"https://console.example.com".to_owned())
        );
        assert!(
            effective
                .allowed_origins
                .contains(&"https://portal.example.com".to_owned())
        );
    }

    #[test]
    fn test_the_route_cors_wins_when_the_upstream_keeps_its_private() {
        let upstream: CorsConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "sharing": "private",
            "allowed_origins": ["https://console.example.com"]
        }))
        .unwrap();
        let route: CorsConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "allowed_origins": ["https://portal.example.com"]
        }))
        .unwrap();

        let effective = effective_cors(&upstream, Some(&route));

        assert_eq!(
            effective.allowed_origins,
            vec!["https://portal.example.com"]
        );
    }

    #[test]
    fn test_without_cors_configuration_cors_stays_disabled() {
        let upstream = CorsConfig::default();

        let effective = effective_cors(&upstream, None);

        assert!(!effective.enabled);
    }

    #[test]
    fn test_the_effective_rate_limit_is_the_stricter_of_both() {
        let upstream: RateLimitConfig = serde_json::from_value(serde_json::json!({
            "sustained": { "rate": 10, "window": "second" }
        }))
        .unwrap();
        let route: RateLimitConfig = serde_json::from_value(serde_json::json!({
            "sustained": { "rate": 4, "window": "second" }
        }))
        .unwrap();

        let effective = effective_limit(Some(upstream), Some(route)).expect("a limit applies");

        assert_eq!(effective.sustained.rate, 4);
    }
}
