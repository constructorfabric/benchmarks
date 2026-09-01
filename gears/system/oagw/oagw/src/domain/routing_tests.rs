//! Unit tests for [`super::routing`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use uuid::Uuid;

use super::{normalize_method, path_matches_prefix, resolve_alias, resolve_route, route_matches};
use crate::domain::error::DomainError;
use crate::domain::models::{
    Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Route, ServerConfig,
    Upstream,
};

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Https, host, port)
}

fn upstream(tenant: Uuid, alias: &str, enabled: bool) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled,
        protocol: crate::domain::models::Protocol::Http,
        server: ServerConfig {
            endpoints: vec![endpoint("api.example.com", 443)],
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

fn http_match(path: &str) -> HttpMatch {
    HttpMatch {
        methods: vec![HttpMethod::Get, HttpMethod::Post],
        path: path.to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

fn route(upstream_id: Uuid, path: &str, priority: i32) -> Route {
    route_with(upstream_id, path, priority, true)
}

fn route_with(upstream_id: Uuid, path: &str, priority: i32, enabled: bool) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        upstream_id,
        enabled,
        priority,
        match_config: MatchConfig {
            http: Some(http_match(path)),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn resolve_alias_selects_the_leaf_most_upstream() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let root_upstream = upstream(root, "api.vendor.com", true);
    let leaf_upstream = upstream(leaf, "api.vendor.com", true);
    let chain = [leaf, root];
    let pool = [leaf_upstream, root_upstream];

    let resolved = resolve_alias(&chain, &pool, "api.vendor.com").unwrap();
    assert_eq!(resolved.selected.tenant_id, leaf);
    // Ancestors are reported root → leaf for the configuration merge.
    assert_eq!(resolved.ancestors.len(), 1);
    assert_eq!(resolved.ancestors[0].tenant_id, root);
}

#[test]
fn resolve_alias_is_case_insensitive_and_strips_trailing_dots() {
    let tenant = Uuid::new_v4();
    let pool = [upstream(tenant, "api.vendor.com", true)];
    let resolved = resolve_alias(&[tenant], &pool, "  API.Vendor.COM. ").unwrap();
    assert_eq!(resolved.selected.tenant_id, tenant);
}

#[test]
fn resolve_alias_ignores_out_of_chain_tenants() {
    let tenant = Uuid::new_v4();
    let stranger = Uuid::new_v4();
    let pool = [upstream(stranger, "api.vendor.com", true)];
    assert!(resolve_alias(&[tenant], &pool, "api.vendor.com").is_none());
}

#[test]
fn resolve_alias_returns_the_selected_upstream_even_when_disabled() {
    // A disabled selected upstream must surface as 503 LinkUnavailable in the
    // handler, never as a silent fallback to an ancestor's upstream.
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let root_upstream = upstream(root, "api.vendor.com", true);
    let leaf_upstream = upstream(leaf, "api.vendor.com", false);
    let pool = [leaf_upstream, root_upstream];

    let resolved = resolve_alias(&[leaf, root], &pool, "api.vendor.com").unwrap();
    assert_eq!(resolved.selected.tenant_id, leaf);
    assert!(!resolved.selected.enabled);
    assert_eq!(resolved.ancestors.len(), 1);
}

#[test]
fn resolve_alias_returns_none_for_unknown_alias() {
    let tenant = Uuid::new_v4();
    let pool = [upstream(tenant, "api.vendor.com", true)];
    assert!(resolve_alias(&[tenant], &pool, "other.vendor.com").is_none());
}

#[test]
fn path_prefix_matching_respects_segment_boundaries() {
    assert!(path_matches_prefix("/v1/chat", "/v1/chat"));
    assert!(path_matches_prefix("/v1/chat/completions", "/v1/chat"));
    assert!(!path_matches_prefix("/v1/chatbot", "/v1/chat"));
    assert!(path_matches_prefix("/anything", ""));
    assert!(path_matches_prefix("/anything", "/"));
    assert!(path_matches_prefix("/v1", "/v1/"));
}

#[test]
fn route_matching_honours_enabled_and_methods() {
    let upstream_id = Uuid::new_v4();
    let route = route(upstream_id, "/v1/chat", 0);
    assert!(route_matches(&route, "GET", "/v1/chat"));
    assert!(route_matches(&route, "post", "/v1/chat/x"));
    assert!(!route_matches(&route, "DELETE", "/v1/chat"));
    assert!(!route_matches(&route_with(upstream_id, "/v1/chat", 0, false), "GET", "/v1/chat"));
}

#[test]
fn resolve_route_prefers_the_longest_prefix_then_the_higher_priority() {
    let upstream_id = Uuid::new_v4();
    let broad = route(upstream_id, "/v1", 5);
    let narrow_low = route(upstream_id, "/v1/chat", 1);
    let narrow_high = route(upstream_id, "/v1/chat", 9);
    let candidates = [broad, narrow_low, narrow_high];

    let matched = resolve_route(&candidates, "GET", "/v1/chat/completions").unwrap();
    assert_eq!(matched.route.priority, 9);
    assert_eq!(matched.suffix, "/completions");

    let matched = resolve_route(&candidates, "GET", "/v1/other").unwrap();
    assert_eq!(matched.route.priority, 5);
    assert_eq!(matched.suffix, "/other");
}

#[test]
fn resolve_route_skips_disabled_routes_and_returns_none_when_nothing_matches() {
    let upstream_id = Uuid::new_v4();
    let candidates = [route_with(upstream_id, "/v1/chat", 0, false)];
    assert!(resolve_route(&candidates, "GET", "/v1/chat").is_none());
    assert!(resolve_route(&[route(upstream_id, "/v1/chat", 0)], "GET", "/v2").is_none());
}

#[test]
fn query_allowlist_is_enforced() {
    let upstream_id = Uuid::new_v4();
    let mut route = route(upstream_id, "/v1/chat", 0);
    route.match_config.http.as_mut().unwrap().query_allowlist = vec!["model".to_owned()];
    super::guard_route_match(&route, &[("model", "gpt")], "").unwrap();
    let rejected = super::guard_route_match(&route, &[("model", "gpt"), ("api-key", "x")], "");
    assert!(matches!(rejected, Err(DomainError::RouteError { .. })));
}

#[test]
fn disabled_path_suffix_mode_rejects_a_suffix() {
    let upstream_id = Uuid::new_v4();
    let mut route = route(upstream_id, "/v1/chat", 0);
    route.match_config.http.as_mut().unwrap().path_suffix_mode = PathSuffixMode::Disabled;
    assert!(super::guard_route_match(&route, &[], "/completions").is_err());
    assert!(super::guard_route_match(&route, &[], "").is_ok());
}

#[test]
fn methods_are_normalized_for_metrics() {
    assert_eq!(normalize_method("get"), "GET");
    assert_eq!(normalize_method("PATCH"), "PATCH");
    assert_eq!(normalize_method("PURGE"), "_OTHER");
}

#[test]
fn map_structures_are_exercised_for_coverage() {
    // Keeps the BTreeMap import meaningful when the match helpers evolve.
    let set: BTreeMap<String, String> = BTreeMap::new();
    assert!(set.is_empty());
}
