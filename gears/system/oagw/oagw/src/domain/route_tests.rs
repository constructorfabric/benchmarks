//! Tests of the route aggregate and its match keys.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use uuid::uuid;

use super::*;
use crate::domain::upstream::{AllowedOrigin, CorsConfig, PluginChain, SharingMode};

const TENANT: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000e1");
const UPSTREAM: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000e2");
const ROUTE_ID: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000e3");

fn http_match(path: &str, methods: &[HttpMethod]) -> HttpMatch {
    HttpMatch::new(
        methods.to_vec(),
        path.to_owned(),
        Vec::new(),
        PathSuffixMode::Append,
    )
    .unwrap()
}

fn http_route(path: &str, methods: &[HttpMethod]) -> Route {
    route_with_match(RouteMatch::Http(http_match(path, methods)))
}

fn route_with_match(route_match: RouteMatch) -> Route {
    Route::new(
        ROUTE_ID,
        &RouteSpec {
            tenant_id: TENANT,
            upstream_id: UPSTREAM,
            r#match: route_match,
            plugins: None,
            rate_limit: None,
            cors: None,
            enabled: true,
            tags: Vec::new(),
        },
    )
    .unwrap()
}

#[test]
fn a_route_binds_an_upstream_to_a_match() {
    let route = http_route("/v1/chat", &[HttpMethod::Post]);
    assert_eq!(route.id, ROUTE_ID);
    assert_eq!(route.tenant_id, TENANT);
    assert_eq!(route.upstream_id, UPSTREAM);
    assert_eq!(route.specificity(), "/v1/chat".len());
    assert!(route.r#match.is_http());
    assert!(route.http_match().is_some());
}

#[test]
fn http_matches_require_a_method_and_an_absolute_path() {
    assert!(
        HttpMatch::new(
            Vec::new(),
            "/v1".to_owned(),
            Vec::new(),
            PathSuffixMode::Append
        )
        .is_err()
    );
    assert!(
        HttpMatch::new(
            vec![HttpMethod::Head],
            "/v1".to_owned(),
            Vec::new(),
            PathSuffixMode::Append
        )
        .is_err()
    );
    assert!(
        HttpMatch::new(
            vec![HttpMethod::Get],
            String::new(),
            Vec::new(),
            PathSuffixMode::Append
        )
        .is_err()
    );
    assert!(
        HttpMatch::new(
            vec![HttpMethod::Get],
            "v1".to_owned(),
            Vec::new(),
            PathSuffixMode::Append
        )
        .is_err()
    );
}

#[test]
fn a_prefix_matches_at_a_segment_boundary_only() {
    let route = http_route("/v1", &[HttpMethod::Get]);
    assert!(route.matches("/v1", HttpMethod::Get));
    assert!(route.matches("/v1/users", HttpMethod::Get));
    assert!(!route.matches("/v10", HttpMethod::Get));
    assert!(!route.matches("/v1", HttpMethod::Post));
    assert!(!route.matches("/other", HttpMethod::Get));
}

#[test]
fn path_suffix_mode_append_forwards_the_suffix() {
    let route = http_route("/v1/chat", &[HttpMethod::Post]);
    assert_eq!(
        route.proxy_path("/v1/chat/completions").as_deref(),
        Some("/completions")
    );
    assert_eq!(route.proxy_path("/v1/chat").as_deref(), Some("/"));
    assert_eq!(route.proxy_path("/v1").as_deref(), None);
    let (prefix, suffix) = route
        .http_match()
        .unwrap()
        .split_path("/v1/chat/completions")
        .unwrap();
    assert_eq!(prefix, "/v1/chat");
    assert_eq!(suffix, "/completions");
}

#[test]
fn path_suffix_mode_disabled_never_forwards_a_suffix() {
    let route = route_with_match(RouteMatch::Http(
        HttpMatch::new(
            vec![HttpMethod::Get],
            String::from("/v1/chat"),
            Vec::new(),
            PathSuffixMode::Disabled,
        )
        .unwrap(),
    ));
    assert!(route.matches("/v1/chat", HttpMethod::Get));
    assert!(route.matches("/v1/chat/completions", HttpMethod::Get));
    assert_eq!(
        route.proxy_path("/v1/chat/completions").as_deref(),
        Some("/")
    );
    assert!(!PathSuffixMode::Disabled.accepts_suffix());
    assert!(PathSuffixMode::Append.accepts_suffix());
}

#[test]
fn an_empty_query_allowlist_allows_nothing() {
    let mut route = http_route("/v1/chat", &[HttpMethod::Post]);
    assert!(!route.allows_query_param("model"));
    route.r#match = RouteMatch::Http(
        HttpMatch::new(
            vec![HttpMethod::Post],
            String::from("/v1/chat"),
            vec![String::from("model"), String::from("temperature")],
            PathSuffixMode::Append,
        )
        .unwrap(),
    );
    assert!(route.allows_query_param("model"));
    assert!(!route.allows_query_param("session"));
}

#[test]
fn the_most_specific_route_wins() {
    let broad = http_route("/v1", &[HttpMethod::Get]);
    let specific = http_route("/v1/chat/completions", &[HttpMethod::Post]);
    let routes = vec![broad, specific.clone()];
    let best = best_route_match(&routes, "/v1/chat/completions", HttpMethod::Post).unwrap();
    assert_eq!(best.id, specific.id);
    // No match at all.
    assert!(best_route_match(&routes, "/v9", HttpMethod::Post).is_none());
    // First registered route wins a tie.
    let other = http_route("/v1/chat/completions", &[HttpMethod::Get]);
    let ties = vec![other, specific.clone()];
    let best = best_route_match(&ties, "/v1/chat/completions", HttpMethod::Get).unwrap();
    assert_eq!(best.id, specific.id);
}

#[test]
fn grpc_routes_never_match_http_requests() {
    let route = route_with_match(RouteMatch::Grpc(
        GrpcMatch::new(String::from("vendor.api.ChatService"), String::from("Send")).unwrap(),
    ));
    assert!(!route.matches("/v1/chat", HttpMethod::Post));
    assert!(route.http_match().is_none());
    assert!(!route.allows_query_param("model"));
    assert_eq!(route.proxy_path("/v1/chat"), None);
    assert!(GrpcMatch::new(String::new(), String::from("Send")).is_err());
    assert!(GrpcMatch::new(String::from("vendor.api.ChatService"), String::new()).is_err());
}

#[test]
fn route_tags_and_cors_are_validated() {
    let mut route_spec = RouteSpec {
        tenant_id: TENANT,
        upstream_id: UPSTREAM,
        r#match: RouteMatch::Http(http_match("/v1", &[HttpMethod::Get])),
        plugins: Some(PluginChain::default()),
        rate_limit: None,
        cors: Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec![AllowedOrigin::Any],
            allowed_methods: vec![HttpMethod::Get],
            expose_headers: Vec::new(),
            allow_credentials: true,
        }),
        enabled: true,
        tags: Vec::new(),
    };
    assert!(Route::new(ROUTE_ID, &route_spec).is_err());
    route_spec.cors = None;
    route_spec.tags = vec![String::from("Invalid Tag")];
    assert!(Route::new(ROUTE_ID, &route_spec).is_err());
    route_spec.tags = vec![String::from("valid")];
    assert!(Route::new(ROUTE_ID, &route_spec).is_ok());
}

#[test]
fn path_suffix_mode_and_methods_parse() {
    assert_eq!(
        PathSuffixMode::parse("append").unwrap(),
        PathSuffixMode::Append
    );
    assert_eq!(
        PathSuffixMode::parse("disabled").unwrap(),
        PathSuffixMode::Disabled
    );
    assert!(PathSuffixMode::parse("strip").is_err());
    assert_eq!(PathSuffixMode::default(), PathSuffixMode::Append);
}

#[test]
fn the_route_gts_type_is_valid() {
    assert_eq!(ROUTE_GTS_TYPE, "gts.cf.core.oagw.route.v1~");
    assert!(ROUTE_GTS_TYPE.starts_with("gts.cf.core.oagw.route.v1~"));
}
