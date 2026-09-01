//! Route matching and endpoint-selection tests (ADR-0001 matrix).

use crate::domain::error::DomainError;
use crate::domain::model::{
    Endpoint, EndpointScheme, GrpcMatch, HttpMatch, MatchConfig, PathSuffixMode, Route,
};
use crate::domain::routing::{
    RouteCandidate, apply_path_suffix, grpc_match, match_grpc, match_http_route, select_endpoint,
    validate_query_params,
};

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port,
    }
}

fn route(upstream_id: uuid::Uuid, path: &str, methods: &[&str], priority: u32) -> Route {
    Route {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::new_v4(),
        upstream_id,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                path: path.to_owned(),
                query_allowlist: vec!["api-version".to_owned()],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        priority,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: 0,
    }
}

fn candidates(routes: &[Route]) -> Vec<RouteCandidate<'_>> {
    routes
        .iter()
        .map(|route| RouteCandidate {
            tenant_id: route.tenant_id,
            upstream_id: route.upstream_id,
            route,
        })
        .collect()
}

#[test]
fn head_matches_a_get_route() {
    let route = route(uuid::Uuid::new_v4(), "/v1", &["GET"], 0);
    let list = [route];
    let candidates = candidates(&list);
    let matched = match_http_route(&candidates, "HEAD", "/v1/items").unwrap();
    assert_eq!(matched.priority, 0);
}

#[test]
fn longest_prefix_wins_then_priority() {
    let upstream = uuid::Uuid::new_v4();
    let long = route(upstream, "/v1/chat", &["POST"], 0);
    let short = route(upstream, "/v1", &["POST"], 9);
    let routes = vec![long, short];
    let matched = match_http_route(&candidates(&routes), "POST", "/v1/chat/completions").unwrap();
    assert_eq!(matched.match_config.http.as_ref().unwrap().path, "/v1/chat");

    // Equal prefix, higher priority wins.
    let low = route(upstream, "/v1", &["POST"], 1);
    let high = route(upstream, "/v1", &["POST"], 7);
    let routes = vec![low, high];
    assert_eq!(
        match_http_route(&candidates(&routes), "POST", "/v1/x")
            .unwrap()
            .priority,
        7
    );
}

#[test]
fn method_not_in_allowlist_is_route_not_found() {
    let route = route(uuid::Uuid::new_v4(), "/v1", &["GET"], 0);
    let err =
        match_http_route(&candidates(std::slice::from_ref(&route)), "DELETE", "/v1").unwrap_err();
    assert!(matches!(err, DomainError::RouteNotFound(_)));
}

#[test]
fn options_matches_only_a_route_allowing_options() {
    let get_route = route(uuid::Uuid::new_v4(), "/v1", &["GET"], 0);
    assert!(
        match_http_route(
            &candidates(std::slice::from_ref(&get_route)),
            "OPTIONS",
            "/v1"
        )
        .is_err()
    );
    let options_route = route(uuid::Uuid::new_v4(), "/v1", &["OPTIONS"], 0);
    assert!(
        match_http_route(
            &candidates(std::slice::from_ref(&options_route)),
            "OPTIONS",
            "/v1"
        )
        .is_ok()
    );
}

#[test]
fn disabled_routes_are_skipped() {
    let mut route = route(uuid::Uuid::new_v4(), "/v1", &["GET"], 0);
    route.enabled = false;
    assert!(match_http_route(&candidates(std::slice::from_ref(&route)), "GET", "/v1").is_err());
}

#[test]
fn single_endpoint_never_requires_target_host() {
    let endpoints = vec![endpoint("api.openai.com", 443)];
    let selected = select_endpoint(&endpoints, false, None, 0).unwrap();
    assert_eq!(selected.host, "api.openai.com");
    // Present but validated: an unknown host is still rejected.
    assert!(matches!(
        select_endpoint(&endpoints, false, Some("other.example.com"), 0),
        Err(DomainError::UnknownTargetHost(_))
    ));
    let ok = select_endpoint(&endpoints, false, Some("api.openai.com"), 0).unwrap();
    assert_eq!(ok.host, "api.openai.com");
}

#[test]
fn multi_endpoint_common_suffix_requires_header() {
    let endpoints = vec![
        endpoint("us.vendor.com", 443),
        endpoint("eu.vendor.com", 443),
    ];
    assert!(matches!(
        select_endpoint(&endpoints, true, None, 0),
        Err(DomainError::MissingTargetHost(_))
    ));
    let selected = select_endpoint(&endpoints, true, Some("eu.vendor.com"), 0).unwrap();
    assert_eq!(selected.host, "eu.vendor.com");
}

#[test]
fn multi_endpoint_explicit_alias_round_robins() {
    let endpoints = vec![
        endpoint("a.example.com", 443),
        endpoint("b.example.com", 443),
    ];
    assert_eq!(
        select_endpoint(&endpoints, false, None, 0).unwrap().host,
        "a.example.com"
    );
    assert_eq!(
        select_endpoint(&endpoints, false, None, 1).unwrap().host,
        "b.example.com"
    );
    assert_eq!(
        select_endpoint(&endpoints, false, None, 2).unwrap().host,
        "a.example.com"
    );
    let explicit = select_endpoint(&endpoints, false, Some("b.example.com"), 0).unwrap();
    assert_eq!(explicit.host, "b.example.com");
}

#[test]
fn target_host_rejects_non_host_values() {
    let endpoints = vec![
        endpoint("a.example.com", 443),
        endpoint("b.example.com", 443),
    ];
    assert!(matches!(
        select_endpoint(&endpoints, false, Some("not a host"), 0),
        Err(DomainError::InvalidTargetHost(_))
    ));
    assert!(matches!(
        select_endpoint(&endpoints, false, Some("a.example.com/v1"), 0),
        Err(DomainError::InvalidTargetHost(_))
    ));
    assert!(matches!(
        select_endpoint(&endpoints, false, Some("missing.example.com"), 0),
        Err(DomainError::UnknownTargetHost(_))
    ));
}

#[test]
fn a_single_label_hostname_is_a_valid_target_host() {
    // A single-label hostname passes `validate_hostname`, so it is not
    // rejected by `invalid_target_host` (ADR-0007): it either selects the
    // endpoint or reports `unknown_target_host`.
    let endpoints = vec![endpoint("intranet", 443)];
    let selected = select_endpoint(&endpoints, false, Some("INTRANET."), 0).unwrap();
    assert_eq!(selected.host, "intranet");

    let unknown = vec![endpoint("a.example.com", 443)];
    assert!(matches!(
        select_endpoint(&unknown, false, Some("intranet"), 0),
        Err(DomainError::UnknownTargetHost(_))
    ));
}

#[test]
fn a_port_bearing_target_host_is_invalid_not_unknown() {
    let endpoints = vec![
        endpoint("us.vendor.com", 443),
        endpoint("eu.vendor.com", 443),
    ];
    assert!(matches!(
        select_endpoint(&endpoints, true, Some("us.vendor.com:8443"), 0),
        Err(DomainError::InvalidTargetHost(_))
    ));
    assert!(matches!(
        select_endpoint(&endpoints, true, Some("us.vendor.com/v1"), 0),
        Err(DomainError::InvalidTargetHost(_))
    ));
}

#[test]
fn an_ipv6_literal_is_not_rejected_as_invalid() {
    let endpoints = vec![endpoint("2001:db8::1", 443)];
    let selected = select_endpoint(&endpoints, false, Some("2001:db8::1"), 0).unwrap();
    assert_eq!(selected.host, "2001:db8::1");
}

#[test]
fn query_allowlist_enforced() {
    let mut route = route(uuid::Uuid::new_v4(), "/v1", &["GET"], 0);
    let http = route.match_config.http.as_mut().unwrap();
    http.query_allowlist = vec!["api-version".to_owned()];
    assert!(validate_query_params(http, "api-version=1&api-version=2").is_ok());
    assert!(validate_query_params(http, "").is_ok());
    assert!(validate_query_params(http, "api-version=1").is_ok());
    assert!(validate_query_params(http, "other=1").is_err());
}

#[test]
fn path_suffix_modes() {
    let mut http = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert_eq!(
        apply_path_suffix(&http, "chat/completions").unwrap(),
        "/v1/chat/completions"
    );
    assert_eq!(apply_path_suffix(&http, "").unwrap(), "/v1");
    http.path_suffix_mode = PathSuffixMode::Disabled;
    assert!(apply_path_suffix(&http, "extra").is_err());
    assert_eq!(apply_path_suffix(&http, "").unwrap(), "/v1");
    http.path = "/".to_owned();
    http.path_suffix_mode = PathSuffixMode::Append;
    assert_eq!(apply_path_suffix(&http, "a/b").unwrap(), "/a/b");
}

#[test]
fn grpc_matching_is_catalogued_only() {
    let mut route = route(uuid::Uuid::new_v4(), "/", &["POST"], 0);
    route.match_config = MatchConfig {
        http: None,
        grpc: Some(GrpcMatch {
            service: "foo.v1.UserService".to_owned(),
            method: "GetUser".to_owned(),
        }),
    };
    assert!(grpc_match(&route).is_some());
    assert!(
        match_grpc(
            &candidates(std::slice::from_ref(&route)),
            "foo.v1.UserService",
            "GetUser"
        )
        .is_some()
    );
    assert!(
        match_grpc(
            &candidates(std::slice::from_ref(&route)),
            "foo.v1.UserService",
            "Other"
        )
        .is_none()
    );
}

#[test]
fn grpc_match_returns_the_route_that_actually_matched() {
    let first = route(uuid::Uuid::new_v4(), "/first", &["POST"], 0);
    let mut second = route(uuid::Uuid::new_v4(), "/second", &["POST"], 0);
    second.match_config = MatchConfig {
        http: None,
        grpc: Some(GrpcMatch {
            service: "foo.v1.UserService".to_owned(),
            method: "GetUser".to_owned(),
        }),
    };
    let second_id = second.id;
    let list = vec![first, second];
    let matched = match_grpc(&candidates(&list), "foo.v1.UserService", "GetUser").unwrap();
    assert_eq!(
        matched.id, second_id,
        "the matched candidate's route is returned, not the first candidate"
    );
}
