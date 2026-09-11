//! Tests for alias shadowing, route matching and effective-config merge.

use super::{
    EffectiveConfig, ResolvedUpstream, apply_path_suffix, best_route_at_level, effective_config,
    filter_query, match_path, match_route_in_chain, normalize_suffix, outbound_path, strictest,
};
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    AuthConfig, BurstRate, CorsConfig, Endpoint, HttpMatch, MatchConfig, PathSuffixMode,
    PluginBinding, PluginsConfig, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy,
    RateWindow, Route, RouteSpec, ServerConfig, SharingMode, SustainedRate, Upstream, UpstreamSpec,
};
use crate::domain::timeutil;
use serde_json::Map;
use uuid::Uuid;

fn upstream(alias: &str, tenant: Uuid) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        created_at: timeutil::now_rfc3339(),
        updated_at: timeutil::now_rfc3339(),
        spec: UpstreamSpec {
            enabled: true,
            alias: alias.to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: alias.to_owned(),
                    port: 443,
                }],
            },
            protocol: gts::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        },
    }
}

fn route(upstream_id: Uuid, path: &str, methods: &[&str], priority: i32) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        upstream_id,
        created_at: timeutil::now_rfc3339(),
        updated_at: timeutil::now_rfc3339(),
        spec: RouteSpec {
            enabled: true,
            priority,
            tags: Vec::new(),
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        },
    }
}

fn limit(rate: u64, window: RateWindow, sharing: SharingMode) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window },
        burst: None,
        budget: None,
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

fn chain(items: Vec<PluginBinding>, sharing: SharingMode) -> PluginsConfig {
    PluginsConfig { sharing, items }
}

fn binding(plugin_ref: &str) -> PluginBinding {
    PluginBinding {
        plugin_ref: plugin_ref.to_owned(),
        config: Map::new(),
    }
}

// -- suffix / path handling -------------------------------------------------

#[test]
fn suffix_normalization() {
    assert_eq!(normalize_suffix(None), "/");
    assert_eq!(normalize_suffix(Some("")), "/");
    assert_eq!(normalize_suffix(Some("/")), "/");
    assert_eq!(normalize_suffix(Some("v1/models")), "/v1/models");
    assert_eq!(normalize_suffix(Some("/v1/models")), "/v1/models");
}

#[test]
fn path_prefix_matching() {
    assert_eq!(match_path("/v1/chat", "/v1/chat"), Some(""));
    assert_eq!(
        match_path("/v1/chat", "/v1/chat/completions"),
        Some("/completions")
    );
    assert_eq!(match_path("/v1/chat", "/v1/chatty"), None);
    assert_eq!(match_path("/v1/chat", "/v2/chat"), None);
    assert_eq!(
        match_path("/", "/anything/at/all"),
        Some("/anything/at/all")
    );
    assert_eq!(match_path("/v1/chat/", "/v1/chat"), Some(""));
}

#[test]
fn outbound_path_reassembles_the_request() {
    assert_eq!(outbound_path("/v1/chat", ""), "/v1/chat");
    assert_eq!(
        outbound_path("/v1/chat", "/completions"),
        "/v1/chat/completions"
    );
    assert_eq!(outbound_path("/", ""), "/");
    assert_eq!(outbound_path("/", "/v1/models"), "/v1/models");
}

#[test]
fn path_suffix_mode_disabled_rejects_a_suffix() {
    let http = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/models".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Disabled,
    };
    assert_eq!(
        apply_path_suffix(&http, "").expect("exact match allowed"),
        "/v1/models"
    );
    let err = apply_path_suffix(&http, "/extra").expect_err("suffix refused");
    assert_eq!(err.status, 400);
    assert_eq!(err.error_type, gts::ERR_VALIDATION);
}

#[test]
fn path_suffix_mode_append_composes() {
    let http = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert_eq!(
        apply_path_suffix(&http, "/chat/completions").expect("appended"),
        "/v1/chat/completions"
    );
}

// -- route matching --------------------------------------------------------

#[test]
fn longest_path_wins_then_priority() {
    let upstream_id = Uuid::new_v4();
    let routes = vec![
        route(upstream_id, "/v1", &["GET"], 0),
        route(upstream_id, "/v1/chat", &["GET"], 0),
    ];
    let (matched, remaining) =
        best_route_at_level(&routes, "GET", "/v1/chat/completions").expect("matched");
    assert_eq!(matched.http().expect("http").path, "/v1/chat");
    assert_eq!(remaining, "/completions");

    let mut tie = vec![
        route(upstream_id, "/v1", &["GET"], 1),
        route(upstream_id, "/v1", &["GET"], 9),
    ];
    tie[1].spec.priority = 9;
    let (matched, _) = best_route_at_level(&tie, "GET", "/v1/x").expect("matched");
    assert_eq!(matched.spec.priority, 9);
}

#[test]
fn head_is_served_by_a_get_route() {
    let upstream_id = Uuid::new_v4();
    let routes = vec![route(upstream_id, "/v1/models", &["GET"], 0)];
    assert!(
        best_route_at_level(&routes, "HEAD", "/v1/models").is_some(),
        "HEAD is a GET without a body, and the route schema cannot name it"
    );
    let post_only = vec![route(upstream_id, "/v1/models", &["POST"], 0)];
    assert!(best_route_at_level(&post_only, "HEAD", "/v1/models").is_none());
}

#[test]
fn method_must_be_allowed() {
    let upstream_id = Uuid::new_v4();
    let routes = vec![route(upstream_id, "/v1/chat", &["POST"], 0)];
    assert!(best_route_at_level(&routes, "GET", "/v1/chat").is_none());
    assert!(best_route_at_level(&routes, "post", "/v1/chat").is_some());
}

#[test]
fn disabled_routes_are_excluded_from_matching() {
    let upstream_id = Uuid::new_v4();
    let mut routes = vec![route(upstream_id, "/v1/chat", &["GET"], 0)];
    routes[0].spec.enabled = false;
    assert!(best_route_at_level(&routes, "GET", "/v1/chat").is_none());
}

#[test]
fn descendant_routes_beat_ancestor_routes() {
    let descendant_upstream = Uuid::new_v4();
    let ancestor_upstream = Uuid::new_v4();
    // The ancestor has the longer, more specific path — level order still wins.
    let levels = vec![
        vec![route(descendant_upstream, "/v1", &["GET"], 0)],
        vec![route(ancestor_upstream, "/v1/chat", &["GET"], 100)],
    ];
    let (matched, _) = match_route_in_chain(&levels, "GET", "/v1/chat").expect("matched");
    assert_eq!(matched.upstream_id, descendant_upstream);
}

#[test]
fn ancestor_routes_are_inherited_when_the_descendant_has_none() {
    let ancestor_upstream = Uuid::new_v4();
    let levels = vec![
        Vec::new(),
        vec![route(ancestor_upstream, "/v1/chat", &["GET"], 0)],
    ];
    let (matched, _) = match_route_in_chain(&levels, "GET", "/v1/chat").expect("inherited");
    assert_eq!(matched.upstream_id, ancestor_upstream);
}

// -- query allowlist -------------------------------------------------------

#[test]
fn empty_allowlist_allows_no_query_parameters() {
    let http = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/models".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert!(filter_query(&http, &[]).expect("no params").is_empty());
    let err = filter_query(&http, &[("limit".to_owned(), "5".to_owned())])
        .expect_err("nothing is allowed");
    assert_eq!(err.status, 400);
}

#[test]
fn allowlisted_query_parameters_pass() {
    let http = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/models".to_owned(),
        query_allowlist: vec!["limit".to_owned()],
        path_suffix_mode: PathSuffixMode::Append,
    };
    let passed = filter_query(&http, &[("limit".to_owned(), "5".to_owned())]).expect("allowed");
    assert_eq!(passed, vec![("limit".to_owned(), "5".to_owned())]);
    assert!(filter_query(&http, &[("offset".to_owned(), "1".to_owned())]).is_err());
}

// -- shadowing and enabled inheritance -------------------------------------

#[test]
fn closest_tenant_wins_the_alias() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let candidates = vec![
        upstream("api.openai.com", leaf),
        upstream("api.openai.com", root),
    ];
    let resolved = ResolvedUpstream::from_chain(candidates).expect("resolved");
    assert_eq!(resolved.selected.tenant_id, leaf);
    assert_eq!(resolved.ancestors.len(), 1);
    assert!(resolved.effectively_enabled());
}

#[test]
fn an_ancestor_disable_propagates_to_descendants() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.enabled = false;
    let resolved = ResolvedUpstream::from_chain(vec![upstream("api.openai.com", leaf), ancestor])
        .expect("resolved");
    assert!(
        !resolved.effectively_enabled(),
        "a descendant must not be able to re-enable an ancestor-disabled upstream"
    );
}

// -- rate limit merge ------------------------------------------------------

#[test]
fn strictest_takes_the_minimum_of_both_legs() {
    let route_limit = limit(500, RateWindow::Minute, SharingMode::Private);
    let upstream_limit = limit(100, RateWindow::Minute, SharingMode::Private);
    let merged = strictest(Some(route_limit), Some(&upstream_limit)).expect("merged");
    assert_eq!(merged.sustained.rate, 100);

    let mut generous = limit(100, RateWindow::Minute, SharingMode::Private);
    generous.burst = Some(BurstRate { capacity: Some(50) });
    let mut tight = limit(1_000, RateWindow::Minute, SharingMode::Private);
    tight.burst = Some(BurstRate { capacity: Some(5) });
    let merged = strictest(Some(generous), Some(&tight)).expect("merged");
    assert_eq!(merged.capacity(), 5, "burst is min()-ed too");
}

#[test]
fn enforced_ancestor_limit_survives_shadowing() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.rate_limit = Some(limit(500, RateWindow::Minute, SharingMode::Private));
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.rate_limit = Some(limit(10_000, RateWindow::Minute, SharingMode::Enforce));

    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let effective = effective_config(&resolved, None);
    let merged = effective.rate_limit.expect("limit present");
    assert_eq!(merged.sustained.rate, 500, "min(10000, 500)");

    // The other direction: a descendant cannot loosen the enforced ceiling.
    let leaf2 = Uuid::new_v4();
    let mut greedy = upstream("api.openai.com", leaf2);
    greedy.spec.rate_limit = Some(limit(50_000, RateWindow::Minute, SharingMode::Private));
    let mut ancestor2 = upstream("api.openai.com", root);
    ancestor2.spec.rate_limit = Some(limit(10_000, RateWindow::Minute, SharingMode::Enforce));
    let resolved = ResolvedUpstream::from_chain(vec![greedy, ancestor2]).expect("resolved");
    let merged = effective_config(&resolved, None)
        .rate_limit
        .expect("limit present");
    assert_eq!(merged.sustained.rate, 10_000);
}

#[test]
fn a_private_ancestor_limit_does_not_clamp() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.rate_limit = Some(limit(500, RateWindow::Minute, SharingMode::Private));
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.rate_limit = Some(limit(10, RateWindow::Minute, SharingMode::Private));
    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let merged = effective_config(&resolved, None)
        .rate_limit
        .expect("limit present");
    assert_eq!(merged.sustained.rate, 500);
}

// -- auth / plugins / cors / tags merge -------------------------------------

#[test]
fn enforced_ancestor_auth_cannot_be_overridden() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.auth = Some(AuthConfig {
        plugin_type: Some(gts::NOOP_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Private,
        config: Map::new(),
    });
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.auth = Some(AuthConfig {
        plugin_type: Some(gts::APIKEY_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Enforce,
        config: Map::new(),
    });
    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let effective = effective_config(&resolved, None);
    assert_eq!(
        effective.auth.and_then(|a| a.plugin_type).as_deref(),
        Some(gts::APIKEY_AUTH_PLUGIN_ID)
    );
}

#[test]
fn inheritable_ancestor_auth_is_overridable() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.auth = Some(AuthConfig {
        plugin_type: Some(gts::NOOP_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Private,
        config: Map::new(),
    });
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.auth = Some(AuthConfig {
        plugin_type: Some(gts::APIKEY_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Inherit,
        config: Map::new(),
    });
    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    assert_eq!(
        effective_config(&resolved, None)
            .auth
            .and_then(|a| a.plugin_type)
            .as_deref(),
        Some(gts::NOOP_AUTH_PLUGIN_ID)
    );
}

#[test]
fn plugin_chains_concatenate_ancestor_then_upstream_then_route() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.plugins = Some(chain(vec![binding("u1")], SharingMode::Private));
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.plugins = Some(chain(vec![binding("a1")], SharingMode::Inherit));
    let mut matched = route(descendant.id, "/v1", &["GET"], 0);
    matched.spec.plugins = Some(chain(vec![binding("r1")], SharingMode::Private));

    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let effective = effective_config(&resolved, Some(&matched));
    let refs: Vec<&str> = effective
        .plugins
        .iter()
        .map(|b| b.plugin_ref.as_str())
        .collect();
    assert_eq!(refs, vec!["a1", "u1", "r1"]);
}

#[test]
fn a_private_ancestor_chain_is_invisible() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.plugins = Some(chain(vec![binding("u1")], SharingMode::Private));
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.plugins = Some(chain(vec![binding("a1")], SharingMode::Private));
    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let refs: Vec<String> = effective_config(&resolved, None)
        .plugins
        .into_iter()
        .map(|b| b.plugin_ref)
        .collect();
    assert_eq!(refs, vec!["u1".to_owned()]);
}

#[test]
fn inherited_cors_origins_are_unioned() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.cors = Some(CorsConfig {
        enabled: true,
        allowed_origins: vec!["https://admin.example.com".to_owned()],
        ..CorsConfig::default()
    });
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.cors = Some(CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        ..CorsConfig::default()
    });
    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let cors = effective_config(&resolved, None).cors.expect("cors");
    assert!(cors.origin_allowed("https://app.example.com"));
    assert!(cors.origin_allowed("https://admin.example.com"));
}

#[test]
fn enforced_cors_replaces_the_descendant_policy() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.cors = Some(CorsConfig {
        enabled: true,
        allowed_origins: vec!["https://evil.example.com".to_owned()],
        ..CorsConfig::default()
    });
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.cors = Some(CorsConfig {
        sharing: SharingMode::Enforce,
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        ..CorsConfig::default()
    });
    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let cors = effective_config(&resolved, None).cors.expect("cors");
    assert!(!cors.origin_allowed("https://evil.example.com"));
    assert!(cors.origin_allowed("https://app.example.com"));
}

#[test]
fn tags_are_add_only_across_the_hierarchy() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let mut descendant = upstream("api.openai.com", leaf);
    descendant.spec.tags = vec!["leaf".to_owned()];
    let mut ancestor = upstream("api.openai.com", root);
    ancestor.spec.tags = vec!["openai".to_owned(), "llm".to_owned()];
    let resolved = ResolvedUpstream::from_chain(vec![descendant, ancestor]).expect("resolved");
    let effective: EffectiveConfig = effective_config(&resolved, None);
    assert_eq!(effective.tags, vec!["leaf", "llm", "openai"]);
}
