//! Hierarchical merge semantics (DESIGN.md §3.2 "Hierarchical Configuration").
//!
//! OAGW layers configuration across a tenant hierarchy and across the
//! upstream/route split. The merge table is:
//!
//! | Field | Merge strategy |
//! |---|---|
//! | Auth | Override if `inherit`; forced if `enforce` |
//! | Rate limits | `min(ancestor, descendant)` — stricter always wins |
//! | Plugins | Concatenate: `ancestor.plugins + descendant.plugins` |
//! | CORS | Union origins if `inherit`; forced if `enforce` |
//!
//! Tags have no sharing mode and always use add-only union semantics. Headers
//! have no sharing mode either and are therefore never inherited.
//!
//! Ancestor `private` values are not visible to descendants, so they never
//! contribute to the merged result. The module is pure: it never touches the
//! store, so the caller resolves the ancestor chain first (see
//! [`OagwStore::resolve_alias`](crate::domain::store::OagwStore::resolve_alias))
//! and folds it root -> descendant with [`merge_chain`].

use crate::domain::model::{
    AuthConfig, CorsConfig, HttpMethod, PluginRef, PluginsConfig, RateLimitConfig, Route,
    RouteConfig, SharingMode, Upstream, UpstreamConfig,
};

/// Merges an ancestor upstream into a descendant one.
///
/// The result keeps the descendant's identity (`id`, `tenant_id`) and its
/// endpoint pool, and merges the policy fields per the table above. Pass the
/// *effective* ancestor value: [`merge_chain`] does that for a whole tenant
/// chain.
#[must_use]
pub fn merge_upstream(ancestor: &Upstream, descendant: &Upstream) -> Upstream {
    Upstream::new(
        descendant.id,
        descendant.tenant_id,
        merge_upstream_config(ancestor, descendant),
    )
}

/// Merges the configuration part of two upstreams.
#[must_use]
pub fn merge_upstream_config(ancestor: &Upstream, descendant: &Upstream) -> UpstreamConfig {
    let ancestor_config = &ancestor.config;
    let descendant_config = &descendant.config;

    UpstreamConfig {
        alias: descendant_config.alias.clone(),
        // A disabled ancestor disables the upstream for every descendant.
        enabled: ancestor_config.enabled && descendant_config.enabled,
        // Tags have no sharing mode: add-only union, descendants cannot remove.
        tags: merge_tags(&ancestor_config.tags, &descendant_config.tags),
        // Endpoints are not hierarchical: the descendant owns the pool it
        // declared (that is what shadowing is for).
        server: descendant_config.server.clone(),
        protocol: descendant_config.protocol,
        auth: merge_auth(
            ancestor_config.auth.as_ref(),
            descendant_config.auth.as_ref(),
        ),
        // Headers have no sharing mode, so nothing is inherited.
        headers: descendant_config.headers.clone(),
        plugins: merge_plugins(&ancestor_config.plugins, &descendant_config.plugins),
        rate_limit: merge_rate_limit(
            ancestor_config.rate_limit.as_ref(),
            descendant_config.rate_limit.as_ref(),
        ),
        cors: merge_cors(
            ancestor_config.cors.as_ref(),
            descendant_config.cors.as_ref(),
        ),
    }
}

/// Merges an ancestor route into a descendant one.
///
/// `upstream_id` comes from the descendant (an ancestor's upstream is not
/// addressable by a descendant route) and the descendant keeps its own match
/// rules; only the policy fields are hierarchical.
#[must_use]
pub fn merge_route(ancestor: &Route, descendant: &Route) -> Route {
    Route::new(
        descendant.id,
        descendant.tenant_id,
        merge_route_config(ancestor, descendant),
    )
}

/// Merges the configuration part of two routes.
#[must_use]
pub fn merge_route_config(ancestor: &Route, descendant: &Route) -> RouteConfig {
    let ancestor_config = &ancestor.config;
    let descendant_config = &descendant.config;

    RouteConfig {
        upstream_id: descendant_config.upstream_id,
        match_rule: descendant_config.match_rule.clone(),
        enabled: ancestor_config.enabled && descendant_config.enabled,
        priority: descendant_config.priority,
        tags: merge_tags(&ancestor_config.tags, &descendant_config.tags),
        plugins: merge_plugins(&ancestor_config.plugins, &descendant_config.plugins),
        rate_limit: merge_rate_limit(
            ancestor_config.rate_limit.as_ref(),
            descendant_config.rate_limit.as_ref(),
        ),
        cors: merge_cors(
            ancestor_config.cors.as_ref(),
            descendant_config.cors.as_ref(),
        ),
    }
}

/// Folds an ancestor chain (root first, descendant last) into one effective
/// upstream.
#[must_use]
pub fn merge_chain(chain: &[Upstream]) -> Option<Upstream> {
    let (root, descendants) = chain.split_first()?;

    Some(
        descendants
            .iter()
            .fold((*root).clone(), |effective, descendant| {
                merge_upstream(&effective, descendant)
            }),
    )
}

/// Folds an ancestor chain of routes into one effective route.
#[must_use]
pub fn merge_route_chain(chain: &[Route]) -> Option<Route> {
    let (root, descendants) = chain.split_first()?;

    Some(
        descendants
            .iter()
            .fold((*root).clone(), |effective, descendant| {
                merge_route(&effective, descendant)
            }),
    )
}

/// Merges the auth binding of an ancestor into a descendant's (DESIGN.md:
/// "Override if `inherit`; forced if `enforce`").
///
/// An ancestor's `private` auth is not visible to descendants, so they keep
/// their own (possibly none).
#[must_use]
pub fn merge_auth(
    ancestor: Option<&AuthConfig>,
    descendant: Option<&AuthConfig>,
) -> Option<AuthConfig> {
    let Some(ancestor_auth) = ancestor else {
        return descendant.cloned();
    };

    match ancestor_auth.sharing {
        SharingMode::Private => descendant.cloned(),
        SharingMode::Inherit => descendant.cloned().or_else(|| Some(ancestor_auth.clone())),
        // `enforce`: descendants cannot override.
        SharingMode::Enforce => Some(ancestor_auth.clone()),
    }
}

/// Merges the rate limits of an ancestor into a descendant's (ADR 0003
/// "Inheritance rules"): `private` yields the descendant's own limit,
/// `inherit` with no descendant limit yields the ancestor's, and `inherit`
/// with a descendant limit or `enforce` yields `min(ancestor, descendant)`.
#[must_use]
pub fn merge_rate_limit(
    ancestor: Option<&RateLimitConfig>,
    descendant: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let Some(ancestor_limit) = ancestor else {
        return descendant.copied();
    };

    if ancestor_limit.sharing == SharingMode::Private {
        return descendant.copied();
    }

    match descendant {
        None => Some(*ancestor_limit),
        Some(descendant_limit) => Some(stricter(ancestor_limit, descendant_limit)),
    }
}

/// The stricter of two rate limits: the lower sustained rate per second wins;
/// on a tie the descendant's (more local) configuration wins.
fn stricter(ancestor: &RateLimitConfig, descendant: &RateLimitConfig) -> RateLimitConfig {
    if ancestor
        .per_second()
        .is_stricter_than(descendant.per_second())
    {
        *ancestor
    } else {
        *descendant
    }
}

/// Merges the plugin chains of an ancestor and a descendant: the ancestor's
/// plugins run first, the descendant's are appended, and enforced plugins
/// cannot be removed.
#[must_use]
pub fn merge_plugins(ancestor: &PluginsConfig, descendant: &PluginsConfig) -> PluginsConfig {
    PluginsConfig {
        sharing: effective_sharing(ancestor.sharing, descendant.sharing),
        items: concatenate(&ancestor.items, &descendant.items),
    }
}

/// The effective plugin chain of an upstream and one of its routes: upstream
/// plugins first, route plugins second (DESIGN.md §3.2: `[U1, U2] + [R1, R2]`
/// => `[U1, U2, R1, R2]`).
#[must_use]
pub fn plugin_chain(upstream: &PluginsConfig, route: &PluginsConfig) -> Vec<PluginRef> {
    concatenate(&upstream.items, &route.items)
}

/// Composes the sharing mode of an inherited field: `enforce` dominates
/// `inherit`, which dominates `private`.
#[must_use]
pub const fn effective_sharing(ancestor: SharingMode, descendant: SharingMode) -> SharingMode {
    match (ancestor, descendant) {
        (SharingMode::Enforce, _) | (_, SharingMode::Enforce) => SharingMode::Enforce,
        (SharingMode::Inherit, _) | (_, SharingMode::Inherit) => SharingMode::Inherit,
        (SharingMode::Private, SharingMode::Private) => SharingMode::Private,
    }
}

/// Merges two plugin item lists: ancestor first, descendant appended, without
/// duplicates.
fn concatenate(ancestor: &[PluginRef], descendant: &[PluginRef]) -> Vec<PluginRef> {
    let mut items = Vec::with_capacity(ancestor.len() + descendant.len());
    items.extend_from_slice(ancestor);
    for item in descendant {
        if !items.contains(item) {
            items.push(item.clone());
        }
    }

    items
}

/// Merges CORS configurations (ADR 0004 "Hierarchical configuration").
///
/// An ancestor `private` CORS configuration is not visible to descendants. An
/// ancestor `inherit` configuration unions its origins, methods and exposed
/// headers with the descendant's. An ancestor `enforce` configuration wins
/// outright: the descendant cannot add origins.
#[must_use]
pub fn merge_cors(
    ancestor: Option<&CorsConfig>,
    descendant: Option<&CorsConfig>,
) -> Option<CorsConfig> {
    let Some(ancestor_cors) = ancestor else {
        return descendant.cloned();
    };

    match ancestor_cors.sharing {
        SharingMode::Private => descendant.cloned(),
        SharingMode::Enforce => Some(ancestor_cors.clone()),
        SharingMode::Inherit => {
            let Some(descendant_cors) = descendant else {
                return Some(ancestor_cors.clone());
            };

            Some(union_cors(ancestor_cors, descendant_cors))
        }
    }
}

/// Unions two CORS configurations, keeping the union free of the
/// credentials-plus-wildcard combination ADR 0004 forbids.
fn union_cors(ancestor: &CorsConfig, descendant: &CorsConfig) -> CorsConfig {
    let mut origins = union_strings(&ancestor.allowed_origins, &descendant.allowed_origins);
    let allow_credentials = ancestor.allow_credentials || descendant.allow_credentials;

    // A wildcard origin may never be combined with credentials (ADR 0004).
    if allow_credentials && origins.iter().any(|origin| origin == "*") {
        origins.retain(|origin| origin != "*");
    }

    CorsConfig {
        sharing: effective_sharing(ancestor.sharing, descendant.sharing),
        enabled: ancestor.enabled || descendant.enabled,
        allowed_origins: origins,
        allowed_methods: union_methods(&ancestor.allowed_methods, &descendant.allowed_methods),
        expose_headers: union_strings(&ancestor.expose_headers, &descendant.expose_headers),
        allow_credentials,
    }
}

/// Add-only union of two string lists, preserving the ancestor's order.
fn union_strings(ancestor: &[String], descendant: &[String]) -> Vec<String> {
    let mut union = Vec::with_capacity(ancestor.len() + descendant.len());
    for value in ancestor.iter().chain(descendant.iter()) {
        if !union.contains(value) {
            union.push(value.clone());
        }
    }

    union
}

/// Add-only union of two method lists, preserving the ancestor's order.
fn union_methods(ancestor: &[HttpMethod], descendant: &[HttpMethod]) -> Vec<HttpMethod> {
    let mut union = Vec::with_capacity(ancestor.len() + descendant.len());
    for method in ancestor.iter().chain(descendant.iter()) {
        if !union.contains(method) {
            union.push(*method);
        }
    }

    union
}

/// Add-only tag union: `union(ancestor_tags, descendant_tags)` (PRD.md §5.5).
/// Descendants can add tags but cannot remove inherited ones.
#[must_use]
pub fn merge_tags(ancestor: &[String], descendant: &[String]) -> Vec<String> {
    union_strings(ancestor, descendant)
}

/// The effective rate limit of a resolved upstream and route:
/// `min(selected_rate, route_rate)` (DESIGN.md §3.1 "Shadowing Behavior").
/// Ancestor enforced limits are already folded in by [`merge_rate_limit`].
#[must_use]
pub fn effective_rate_limit(
    upstream: Option<&RateLimitConfig>,
    route: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (upstream, route) {
        (Some(upstream_limit), Some(route_limit)) => Some(stricter(upstream_limit, route_limit)),
        (Some(upstream_limit), None) => Some(*upstream_limit),
        (None, Some(route_limit)) => Some(*route_limit),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{
        BurstCapacity, Endpoint, Host, HttpMatch, MatchRule, PathSuffixMode, Protocol,
        RateLimitAlgorithm, RateScope, RateStrategy, RateWindow, Scheme, ServerConfig,
        SustainedRate, UpstreamSpec,
    };
    use crate::domain::validation::{validate_route, validate_upstream};
    use serde_json::json;
    use uuid::Uuid;

    const ROOT_TENANT: Uuid = Uuid::from_u128(1);
    const LEAF_TENANT: Uuid = Uuid::from_u128(2);

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint::new(Scheme::Https, Host::parse(host).unwrap(), port)
    }

    fn rate(sharing: SharingMode, rate: u32, window: RateWindow) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: BurstCapacity::default(),
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
        }
    }

    fn cors(sharing: SharingMode, origins: &[&str], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing,
            enabled: true,
            allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            allowed_methods: vec![HttpMethod::Get, HttpMethod::Post],
            expose_headers: Vec::new(),
            allow_credentials: credentials,
        }
    }

    fn upstream(tenant_id: Uuid, alias: &str, config: UpstreamConfig) -> Upstream {
        Upstream::new(
            Uuid::new_v4(),
            tenant_id,
            UpstreamConfig {
                alias: alias.to_owned(),
                ..config
            },
        )
    }

    fn base_upstream_config(alias: &str) -> UpstreamConfig {
        UpstreamConfig {
            alias: alias.to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: ServerConfig::new(vec![endpoint("api.openai.com", 443)]),
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn upstream_from_json(tenant_id: Uuid, alias: &str, body: serde_json::Value) -> Upstream {
        let mut spec: UpstreamSpec = serde_json::from_value(body).unwrap();
        spec.alias = None;
        let mut config = validate_upstream(&spec).unwrap();
        config.alias = alias.to_owned();
        Upstream::new(Uuid::new_v4(), tenant_id, config)
    }

    fn route(tenant_id: Uuid, upstream_id: Uuid, path: &str) -> Route {
        let spec: crate::domain::model::RouteSpec = serde_json::from_value(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": path } }
        }))
        .unwrap();

        Route::new(Uuid::new_v4(), tenant_id, validate_route(&spec).unwrap())
    }

    fn http_match(path: &str) -> MatchRule {
        MatchRule::Http(HttpMatch {
            methods: vec![HttpMethod::Get],
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        })
    }

    // -- auth -------------------------------------------------------------

    #[test]
    fn test_private_ancestor_auth_is_not_visible() {
        let ancestor = AuthConfig {
            plugin_type: Some(
                PluginRef::parse("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1").unwrap(),
            ),
            sharing: SharingMode::Private,
            config: None,
        };

        assert_eq!(merge_auth(Some(&ancestor), None), None);
        assert_eq!(
            merge_auth(Some(&ancestor), Some(&AuthConfig::default())),
            Some(AuthConfig::default()),
        );
    }

    #[test]
    fn test_inherit_ancestor_auth_is_overridable() {
        let ancestor = AuthConfig {
            plugin_type: Some(
                PluginRef::parse("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1").unwrap(),
            ),
            sharing: SharingMode::Inherit,
            config: None,
        };

        // No descendant auth -> the ancestor's value applies.
        assert_eq!(merge_auth(Some(&ancestor), None), Some(ancestor.clone()));

        // A descendant with its own auth overrides it.
        let own = AuthConfig {
            plugin_type: Some(
                PluginRef::parse("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1").unwrap(),
            ),
            sharing: SharingMode::Private,
            config: None,
        };
        assert_eq!(merge_auth(Some(&ancestor), Some(&own)), Some(own));
    }

    #[test]
    fn test_enforce_ancestor_auth_always_applies() {
        let ancestor = AuthConfig {
            plugin_type: Some(
                PluginRef::parse("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1").unwrap(),
            ),
            sharing: SharingMode::Enforce,
            config: None,
        };
        let own = AuthConfig::default();

        assert_eq!(merge_auth(Some(&ancestor), None), Some(ancestor.clone()));
        assert_eq!(merge_auth(Some(&ancestor), Some(&own)), Some(ancestor));
    }

    #[test]
    fn test_descendant_without_ancestor_auth_keeps_its_own() {
        let own = AuthConfig::default();

        assert_eq!(merge_auth(None, Some(&own)), Some(own));
        assert_eq!(merge_auth(None, None), None);
    }

    // -- rate limits ------------------------------------------------------

    #[test]
    fn test_rate_limit_min_with_enforce() {
        // PRD.md example: root 10000/min enforced, leaf 100/min -> 100/min.
        let ancestor = rate(SharingMode::Enforce, 10_000, RateWindow::Minute);
        let descendant = rate(SharingMode::Private, 100, RateWindow::Minute);

        assert_eq!(
            merge_rate_limit(Some(&ancestor), Some(&descendant)),
            Some(descendant)
        );
        assert_eq!(merge_rate_limit(Some(&ancestor), None), Some(ancestor));
    }

    #[test]
    fn test_rate_limit_stricter_ancestor_wins_across_windows() {
        // 100/minute is stricter than 1000/second.
        let ancestor = rate(SharingMode::Enforce, 100, RateWindow::Minute);
        let descendant = rate(SharingMode::Inherit, 1000, RateWindow::Second);

        assert_eq!(
            merge_rate_limit(Some(&ancestor), Some(&descendant)),
            Some(ancestor)
        );
    }

    #[test]
    fn test_rate_limit_inherit_uses_the_descendant_when_present() {
        let ancestor = rate(SharingMode::Inherit, 500, RateWindow::Second);
        let descendant = rate(SharingMode::Private, 100, RateWindow::Second);

        assert_eq!(
            merge_rate_limit(Some(&ancestor), Some(&descendant)),
            Some(descendant)
        );
        assert_eq!(merge_rate_limit(Some(&ancestor), None), Some(ancestor));
    }

    #[test]
    fn test_rate_limit_private_ancestor_is_invisible() {
        let ancestor = rate(SharingMode::Private, 500, RateWindow::Second);
        let descendant = rate(SharingMode::Private, 100, RateWindow::Second);

        assert_eq!(merge_rate_limit(Some(&ancestor), None), None);
        assert_eq!(
            merge_rate_limit(Some(&ancestor), Some(&descendant)),
            Some(descendant)
        );
    }

    #[test]
    fn test_effective_rate_limit_takes_the_route_into_account() {
        let upstream_limit = rate(SharingMode::Enforce, 10_000, RateWindow::Minute);
        let route_limit = rate(SharingMode::Private, 100, RateWindow::Minute);

        assert_eq!(
            effective_rate_limit(Some(&upstream_limit), Some(&route_limit)),
            Some(route_limit)
        );
        assert_eq!(effective_rate_limit(None, None), None);
        assert_eq!(
            effective_rate_limit(None, Some(&route_limit)),
            Some(route_limit)
        );
    }

    // -- plugins ----------------------------------------------------------

    #[test]
    fn test_plugins_concatenate_ancestor_then_descendant() {
        let guard = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
        let request_id = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

        let ancestor = PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![PluginRef::parse(guard).unwrap()],
        };
        let descendant = PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::parse(request_id).unwrap()],
        };

        let merged = merge_plugins(&ancestor, &descendant);

        assert_eq!(merged.sharing, SharingMode::Inherit);
        assert_eq!(merged.items.len(), 2);
        assert_eq!(merged.items[0].as_str(), guard);
        assert_eq!(merged.items[1].as_str(), request_id);
    }

    #[test]
    fn test_plugins_enforced_cannot_be_removed_but_can_be_appended() {
        let guard = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

        let ancestor = PluginsConfig {
            sharing: SharingMode::Enforce,
            items: vec![PluginRef::parse(guard).unwrap()],
        };
        let descendant = PluginsConfig::default();

        let merged = merge_plugins(&ancestor, &descendant);

        assert_eq!(merged.sharing, SharingMode::Enforce);
        assert_eq!(merged.items.len(), 1);
        assert_eq!(merged.items[0].as_str(), guard);
    }

    #[test]
    fn test_plugin_chain_is_upstream_then_route() {
        let guard = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
        let request_id = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

        let upstream_plugins = PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::parse(guard).unwrap()],
        };
        let route_plugins = PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::parse(request_id).unwrap()],
        };

        let chain = plugin_chain(&upstream_plugins, &route_plugins);

        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].as_str(), guard);
        assert_eq!(chain[1].as_str(), request_id);
    }

    #[test]
    fn test_plugins_do_not_duplicate_on_merge() {
        let guard = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

        let ancestor = PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![PluginRef::parse(guard).unwrap()],
        };
        let descendant = PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::parse(guard).unwrap()],
        };

        assert_eq!(merge_plugins(&ancestor, &descendant).items.len(), 1);
    }

    // -- cors -------------------------------------------------------------

    #[test]
    fn test_cors_union_when_inherited() {
        let ancestor = cors(SharingMode::Inherit, &["https://app.example.com"], false);
        let descendant = cors(SharingMode::Private, &["https://app.other.com"], false);

        let merged = merge_cors(Some(&ancestor), Some(&descendant)).unwrap();

        assert_eq!(merged.allowed_origins.len(), 2);
        assert!(
            merged
                .allowed_origins
                .contains(&"https://app.example.com".to_owned())
        );
        assert!(
            merged
                .allowed_origins
                .contains(&"https://app.other.com".to_owned())
        );
    }

    #[test]
    fn test_cors_enforced_cannot_add_origins() {
        let ancestor = cors(SharingMode::Enforce, &["https://app.example.com"], false);
        let descendant = cors(SharingMode::Private, &["https://app.other.com"], false);

        let merged = merge_cors(Some(&ancestor), Some(&descendant)).unwrap();

        assert_eq!(
            merged.allowed_origins,
            vec!["https://app.example.com".to_owned()]
        );
    }

    #[test]
    fn test_cors_private_ancestor_is_invisible() {
        let ancestor = cors(SharingMode::Private, &["https://app.example.com"], false);
        let descendant = cors(SharingMode::Private, &["https://app.other.com"], false);

        assert_eq!(merge_cors(Some(&ancestor), None), None);
        assert_eq!(
            merge_cors(Some(&ancestor), Some(&descendant)),
            Some(descendant)
        );
    }

    #[test]
    fn test_cors_union_never_combines_credentials_with_a_wildcard() {
        let ancestor = cors(SharingMode::Inherit, &["https://app.example.com"], true);
        let descendant = cors(SharingMode::Inherit, &["*"], false);

        let merged = merge_cors(Some(&ancestor), Some(&descendant)).unwrap();

        assert!(merged.allow_credentials);
        assert!(!merged.allowed_origins.contains(&"*".to_owned()));
    }

    // -- tags, enabled ----------------------------------------------------

    #[test]
    fn test_tags_are_add_only_union() {
        let merged = merge_tags(
            &["openai".to_owned(), "llm".to_owned()],
            &["llm".to_owned(), "preview".to_owned()],
        );

        assert_eq!(
            merged,
            vec!["openai".to_owned(), "llm".to_owned(), "preview".to_owned()]
        );
    }

    #[test]
    fn test_disabled_ancestor_disables_the_upstream() {
        let mut ancestor_config = base_upstream_config("api.openai.com");
        ancestor_config.enabled = false;
        let ancestor = upstream(ROOT_TENANT, "api.openai.com", ancestor_config);
        let descendant = upstream(
            LEAF_TENANT,
            "api.openai.com",
            base_upstream_config("api.openai.com"),
        );

        let merged = merge_upstream(&ancestor, &descendant);

        assert!(!merged.is_enabled());
        assert_eq!(merged.tenant_id, LEAF_TENANT);
        assert_eq!(merged.alias(), "api.openai.com");
    }

    // -- whole-document merges --------------------------------------------

    #[test]
    fn test_merge_upstream_keeps_the_descendant_identity_and_pool() {
        let ancestor = upstream(
            ROOT_TENANT,
            "api.openai.com",
            base_upstream_config("api.openai.com"),
        );
        let mut descendant_config = base_upstream_config("api.openai.com");
        descendant_config.server = ServerConfig::new(vec![endpoint("eu.api.openai.com", 443)]);
        let descendant = upstream(LEAF_TENANT, "api.openai.com", descendant_config);

        let merged = merge_upstream(&ancestor, &descendant);

        assert_eq!(merged.id, descendant.id);
        assert_eq!(merged.tenant_id, LEAF_TENANT);
        assert_eq!(merged.endpoints(), descendant.endpoints());
    }

    #[test]
    fn test_merge_chain_folds_root_to_descendant() {
        let root = upstream_from_json(
            ROOT_TENANT,
            "api.openai.com",
            json!({
                "server": { "endpoints": [{ "host": "api.openai.com" }] },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "rate_limit": { "sharing": "enforce", "sustained": { "rate": 10_000, "window": "minute" } },
                "tags": ["llm"],
            }),
        );
        let leaf = upstream_from_json(
            LEAF_TENANT,
            "api.openai.com",
            json!({
                "server": { "endpoints": [{ "host": "api.openai.com" }] },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "rate_limit": { "sustained": { "rate": 100, "window": "minute" } },
                "tags": ["preview"],
            }),
        );

        let merged = merge_chain(&[root.clone(), leaf.clone()]).unwrap();

        assert_eq!(merged.config.rate_limit, leaf.config.rate_limit);
        assert_eq!(merged.config.tags.len(), 2);
        assert_eq!(merged.id, leaf.id);
        assert_eq!(merge_chain(std::slice::from_ref(&root)), Some(root));
        assert_eq!(merge_chain(&[]), None);
    }

    #[test]
    fn test_merge_route_chain_folds_root_to_descendant() {
        let upstream_id = Uuid::new_v4();
        let root = route(ROOT_TENANT, upstream_id, "/v1");
        let leaf = route(LEAF_TENANT, upstream_id, "/v1/chat");

        let merged = merge_route_chain(&[root.clone(), leaf.clone()]).unwrap();

        assert_eq!(merged.id, leaf.id);
        assert_eq!(merged.upstream_id(), upstream_id);
        assert_eq!(merged.config.match_rule, http_match("/v1/chat"));
        assert_eq!(merge_route_chain(std::slice::from_ref(&root)), Some(root));
        assert_eq!(merge_route_chain(&[]), None);
    }

    #[test]
    fn test_merge_route_concatenates_plugins() {
        let guard = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
        let request_id = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

        let upstream_id = Uuid::new_v4();
        let mut root = route(ROOT_TENANT, upstream_id, "/v1");
        root.config.plugins.items = vec![PluginRef::parse(guard).unwrap()];
        let mut leaf = route(LEAF_TENANT, upstream_id, "/v1");
        leaf.config.plugins.items = vec![PluginRef::parse(request_id).unwrap()];

        let merged = merge_route(&root, &leaf);

        assert_eq!(merged.config.plugins.items.len(), 2);
        assert_eq!(merged.config.plugins.items[0].as_str(), guard);
        assert_eq!(merged.config.plugins.items[1].as_str(), request_id);
    }
}
