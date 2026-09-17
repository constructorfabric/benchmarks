//! Tests for alias resolution, route matching and configuration layering.
use uuid::Uuid;

use super::{
    ResolvedUpstream, effective_config, exact_match, layer_cors, layer_rate_limit, match_route,
    matched_upstream_path, prefix_match, query_allowed, resolve_alias, route_not_found,
    upstream_path, upstream_unavailable,
};
use crate::domain::model::{
    BurstConfig, CorsConfig, Endpoint, EndpointScheme, HttpMatch, PathSuffixMode, PluginBinding,
    PluginsConfig, Protocol, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    Route, RouteMatcher, ServerConfig, SharingMode, SustainedRate, Upstream,
};
use crate::infra::plugin::{AuthBinding, PluginBinding as EngineBinding};

fn endpoint(host: &str) -> Endpoint {
    Endpoint::new(EndpointScheme::Https, host, None).expect("endpoint")
}

fn upstream(tenant: Uuid, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled: true,
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![endpoint("api.example.com")],
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn route(upstream_id: Uuid, path: &str, methods: &[&str]) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        upstream_id,
        name: None,
        tags: vec![],
        matcher: RouteMatcher::Http(HttpMatch {
            methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            path: path.to_owned(),
            query_allowlist: vec![],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        priority: 0,
        enabled: true,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn rate(rate: u32, window: RateWindow) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window },
        burst: None,
        scope: RateScope::Global,
        strategy: RateStrategy::Reject,
        cost: 1,
    }
}

fn cors(origins: &[&str], methods: &[&str], enabled: bool) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
        expose_headers: vec![],
        allow_credentials: false,
    }
}

#[test]
fn the_closest_tenant_owning_the_alias_wins() {
    let tenant = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let own = upstream(tenant, "payments");
    let inherited = upstream(parent, "payments");
    // The chain is ordered caller-first, so the caller's own upstream is the
    // closest match.
    let chain = [own, inherited];
    let ResolvedUpstream { upstream, depth } = resolve_alias("payments", &chain).expect("resolved");
    assert_eq!(depth, 0, "the caller's own upstream shadows the ancestor's");
    assert_eq!(upstream.tenant_id, tenant);
    assert_eq!(upstream.alias, "payments");
}

#[test]
fn a_disabled_upstream_is_resolved_and_refused_rather_than_skipped() {
    let tenant = Uuid::new_v4();
    let mut disabled = upstream(tenant, "payments");
    disabled.enabled = false;
    let fallback = upstream(tenant, "payments");
    // The closest match wins even when it is switched off, so the caller sees a
    // precise 503 instead of another tenant's configuration.
    let resolved = resolve_alias("payments", &[disabled, fallback]).expect("resolved");
    assert_eq!(resolved.depth, 0);
    assert!(!resolved.upstream.enabled);
}

#[test]
fn an_unknown_alias_resolves_to_nothing() {
    let tenant = Uuid::new_v4();
    let chain = [upstream(tenant, "payments")];
    assert!(resolve_alias("absent", &chain).is_none());
}

#[test]
fn prefix_matching_scores_by_matched_length() {
    assert_eq!(prefix_match("/v1/*", "/v1/chat/completions"), Some(3));
    assert_eq!(prefix_match("/v1/*", "/v1"), Some(3));
    assert_eq!(prefix_match("/v1/*", "/v2"), None);
    assert_eq!(prefix_match("/v1/*", "/v1x"), None);
    // The wildcard is optional spelling: `path_suffix_mode: append` already
    // grants the suffix, so a bare pattern is a prefix pattern too.
    assert_eq!(prefix_match("/v1", "/v1"), Some(3));
    assert_eq!(prefix_match("/v1", "/v1/chat"), Some(3));
    assert_eq!(prefix_match("/v1", "/v1x"), None);
    assert_eq!(prefix_match("/", "/index.html"), Some(0));
    assert_eq!(prefix_match("/*", "/anything"), Some(0));
}

#[test]
fn exact_matching_is_literal() {
    assert!(exact_match("/v1/charges", "/v1/charges"));
    assert!(!exact_match("/v1/charges", "/v1/charges/extra"));
}

#[test]
fn the_upstream_path_depends_on_the_suffix_mode() {
    let mut matcher = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/*".to_owned(),
        query_allowlist: vec![],
        path_suffix_mode: PathSuffixMode::Disabled,
    };
    assert_eq!(upstream_path(&matcher, "/v1/chat"), "/v1/*");
    matcher.path_suffix_mode = PathSuffixMode::Append;
    assert_eq!(upstream_path(&matcher, "/v1/chat"), "/v1/chat");
    assert_eq!(upstream_path(&matcher, "/v1"), "/v1");
}

#[test]
fn the_matched_path_follows_the_route_matcher() {
    let upstream_id = Uuid::new_v4();
    let mut matched = route(upstream_id, "/v1/*", &["GET"]);
    matched.matcher = RouteMatcher::Http(HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/*".to_owned(),
        query_allowlist: vec![],
        path_suffix_mode: PathSuffixMode::Append,
    });
    assert_eq!(
        matched_upstream_path(&matched, "/v1/charges"),
        "/v1/charges"
    );
}

#[test]
fn an_empty_query_allowlist_admits_any_query() {
    let matcher = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/*".to_owned(),
        query_allowlist: vec![],
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert!(query_allowed(&matcher, None));
    assert!(query_allowed(&matcher, Some("anything=goes")));
}

#[test]
fn a_non_empty_query_allowlist_rejects_unknown_parameters() {
    let matcher = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/*".to_owned(),
        query_allowlist: vec!["limit".to_owned()],
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert!(query_allowed(&matcher, None));
    assert!(query_allowed(&matcher, Some("limit=10")));
    assert!(query_allowed(&matcher, Some("LIMIT=10")));
    assert!(!query_allowed(&matcher, Some("limit=10&debug=1")));
}

#[test]
fn the_longest_path_prefix_wins_over_priority() {
    let upstream_id = Uuid::new_v4();
    let short = route(upstream_id, "/v1/*", &["GET"]);
    let long = route(upstream_id, "/v1/charges/*", &["GET"]);
    let routes = [
        Route {
            priority: 10,
            ..short
        },
        Route {
            priority: 0,
            ..long
        },
    ];
    let matched = match_route(
        &chain_upstream(upstream_id),
        &routes,
        "GET",
        "/v1/charges/extra",
        None,
    )
    .expect("matched");
    assert_eq!(matched.priority, 0);
}

#[test]
fn a_non_matching_method_falls_through() {
    let upstream_id = Uuid::new_v4();
    let only_get = route(upstream_id, "/v1/*", &["GET"]);
    let routes = vec![only_get];
    let upstream = chain_upstream(upstream_id);
    assert!(match_route(&upstream, &routes, "DELETE", "/v1/charges", None).is_none());
}

#[test]
fn a_disabled_route_never_matches() {
    let upstream_id = Uuid::new_v4();
    let mut disabled = route(upstream_id, "/v1/*", &["GET"]);
    disabled.enabled = false;
    let routes = vec![disabled];
    let upstream = chain_upstream(upstream_id);
    assert!(match_route(&upstream, &routes, "GET", "/v1/charges", None).is_none());
}

#[test]
fn routes_of_another_upstream_never_match() {
    let upstream_id = Uuid::new_v4();
    let routes = vec![route(Uuid::new_v4(), "/v1/*", &["GET"])];
    let upstream = chain_upstream(upstream_id);
    assert!(match_route(&upstream, &routes, "GET", "/v1/charges", None).is_none());
}

#[test]
fn a_grpc_upstream_has_no_reachable_http_path() {
    let upstream_id = Uuid::new_v4();
    let mut upstream = chain_upstream(upstream_id);
    upstream.protocol = Protocol::Grpc;
    let routes = vec![route(upstream_id, "/v1/*", &["GET"])];
    assert!(match_route(&upstream, &routes, "GET", "/v1/charges", None).is_none());
}

#[test]
fn the_plugin_chain_concatenates_upstream_first_then_route() {
    let mut owner = upstream(Uuid::new_v4(), "payments");
    owner.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![binding(
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
        )],
    });
    let route_binding = PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![binding(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        )],
    };
    let upstream_id = owner.id;
    let mut matched = route(upstream_id, "/v1/*", &["GET"]);
    matched.plugins = Some(route_binding);

    let effective = effective_config(&owner, Some(&matched));
    assert_eq!(effective.plugins.len(), 2);
    assert_eq!(
        effective.plugins[0].plugin_ref,
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );
    assert_eq!(
        effective.plugins[1].plugin_ref,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
    assert_eq!(effective.guards.len(), 1);
    assert_eq!(effective.transforms.len(), 1);
}

#[test]
fn an_upstream_without_a_route_still_yields_an_effective_config() {
    let owner = upstream(Uuid::new_v4(), "payments");
    let effective = effective_config(&owner, None);
    assert!(effective.plugins.is_empty());
    assert!(effective.guards.is_empty());
    assert!(effective.transforms.is_empty());
    assert!(effective.auth.is_none());
    assert!(effective.rate_limit.is_none());
    assert!(effective.cors.is_none());
}

#[test]
fn the_auth_binding_comes_from_the_upstream_only() {
    let mut owner = upstream(Uuid::new_v4(), "payments");
    owner.auth = Some(crate::domain::model::AuthConfig {
        plugin_type: "cf.oagw.plugin.auth.v1~cf.oagw.oauth2_client_cred.v1".to_owned(),
        plugin_uuid: None,
        sharing: SharingMode::Private,
        config: Some(serde_json::json!({"token_endpoint": "https://idp.example/token"})),
    });
    let effective = effective_config(&owner, None);
    let AuthBinding {
        plugin_type,
        config,
    } = effective.auth.expect("auth binding");
    assert_eq!(
        plugin_type,
        "cf.oagw.plugin.auth.v1~cf.oagw.oauth2_client_cred.v1"
    );
    assert_eq!(config["token_endpoint"], "https://idp.example/token");
}

#[test]
fn a_route_without_a_limit_inherits_the_upstream_limit() {
    let parent = rate(100, RateWindow::Minute);
    let layered = layer_rate_limit(Some(&parent), None).expect("layered");
    assert_eq!(layered.sustained.rate, 100);
}

#[test]
fn a_route_limit_is_used_when_the_upstream_has_none() {
    let child = rate(10, RateWindow::Minute);
    let layered = layer_rate_limit(None, Some(&child)).expect("layered");
    assert_eq!(layered.sustained.rate, 10);
}

#[test]
fn the_slower_of_two_limits_wins() {
    let parent = rate(100, RateWindow::Minute);
    let child = rate(1, RateWindow::Second);
    let layered = layer_rate_limit(Some(&parent), Some(&child)).expect("layered");
    assert_eq!(layered.sustained.rate, 1);
    assert_eq!(layered.sustained.window, RateWindow::Second);

    let other = layer_rate_limit(Some(&child), Some(&parent)).expect("layered");
    assert_eq!(other.sustained.rate, 1);
    assert_eq!(other.sustained.window, RateWindow::Second);
}

#[test]
fn burst_capacities_take_the_minimum() {
    let mut parent = rate(100, RateWindow::Minute);
    parent.burst = Some(BurstConfig { capacity: 50 });
    let mut child = rate(10, RateWindow::Minute);
    child.burst = Some(BurstConfig { capacity: 20 });
    let layered = layer_rate_limit(Some(&parent), Some(&child)).expect("layered");
    assert_eq!(layered.burst.map(|burst| burst.capacity), Some(20));

    child.burst = None;
    let inherited = layer_rate_limit(Some(&parent), Some(&child)).expect("layered");
    assert_eq!(inherited.burst.map(|burst| burst.capacity), Some(50));
}

#[test]
fn a_route_without_a_limit_and_an_upstream_without_a_limit_has_none() {
    assert!(layer_rate_limit(None, None).is_none());
}

#[test]
fn the_route_limit_keeps_its_own_scope() {
    let mut parent = rate(100, RateWindow::Minute);
    parent.scope = RateScope::Global;
    let mut child = rate(10, RateWindow::Minute);
    child.scope = RateScope::User;
    let layered = layer_rate_limit(Some(&parent), Some(&child)).expect("layered");
    assert_eq!(layered.scope, RateScope::User);
}

#[test]
fn cors_origins_and_methods_union() {
    let parent = cors(&["https://a.example"], &["GET"], true);
    let child = cors(&["https://b.example"], &["POST"], true);
    let layered = layer_cors(Some(&parent), Some(&child)).expect("layered");
    assert!(
        layered
            .allowed_origins
            .contains(&"https://a.example".to_owned())
    );
    assert!(
        layered
            .allowed_origins
            .contains(&"https://b.example".to_owned())
    );
    assert!(layered.allowed_methods.contains(&"GET".to_owned()));
    assert!(layered.allowed_methods.contains(&"POST".to_owned()));
    assert!(layered.enabled);
}

#[test]
fn cors_credentials_narrow_and_enabled_widens() {
    let mut parent = cors(&["https://a.example"], &["GET"], false);
    parent.allow_credentials = true;
    let child = cors(&["https://a.example"], &["GET"], true);
    let layered = layer_cors(Some(&parent), Some(&child)).expect("layered");
    assert!(!layered.allow_credentials);
    assert!(layered.enabled);

    let mut strict = cors(&["https://a.example"], &["GET"], false);
    strict.allow_credentials = true;
    let mut permissive = cors(&["https://a.example"], &["GET"], true);
    permissive.allow_credentials = false;
    let both = layer_cors(Some(&strict), Some(&permissive)).expect("layered");
    assert!(!both.allow_credentials);
}

#[test]
fn cors_layers_are_independent() {
    assert!(layer_cors(None, None).is_none());
    let parent = cors(&["https://a.example"], &["GET"], true);
    assert!(layer_cors(Some(&parent), None).is_some());
}

#[test]
fn the_route_not_found_failure_is_a_404() {
    let failure = route_not_found("payments", "GET", "/v1/missing");
    assert_eq!(failure.status, 404);
    assert_eq!(
        failure.type_uri,
        crate::domain::plugin::problem_type(crate::domain::plugin::ROUTE_NOT_FOUND)
    );
    assert!(failure.detail.contains("payments"));
}

#[test]
fn the_unavailable_upstream_failure_is_a_503() {
    let failure = upstream_unavailable("payments");
    assert_eq!(failure.status, 503);
    assert_eq!(
        failure.type_uri,
        crate::domain::plugin::problem_type(crate::domain::plugin::LINK_UNAVAILABLE)
    );
}

#[test]
fn an_engine_binding_carries_a_null_config_when_none_is_declared() {
    let owner = upstream(Uuid::new_v4(), "payments");
    let effective = effective_config(&owner, None);
    assert!(effective.plugins.is_empty());

    let mut with_plugins = upstream(Uuid::new_v4(), "payments");
    with_plugins.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: "cf.oagw.plugin.auth.v1~cf.oagw.noop.v1".to_owned(),
            plugin_uuid: None,
            config: None,
        }],
    });
    let effective = effective_config(&with_plugins, None);
    assert_eq!(effective.plugins.len(), 1);
    assert_eq!(effective.plugins[0].config, serde_json::Value::Null);
    let _ = std::mem::size_of::<EngineBinding>();
}

fn chain_upstream(id: Uuid) -> Upstream {
    let mut owner = upstream(Uuid::new_v4(), "payments");
    owner.id = id;
    owner
}

fn binding(plugin_ref: &str) -> PluginBinding {
    PluginBinding {
        plugin_ref: plugin_ref.to_owned(),
        plugin_uuid: None,
        config: None,
    }
}
