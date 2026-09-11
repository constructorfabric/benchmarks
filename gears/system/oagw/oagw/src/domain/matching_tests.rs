//! Route matching (T024): method allowlist, longest prefix, query allowlist,
//! suffix modes and the exclusion of disabled routes.

use crate::domain::dto::{GrpcMatch, HttpMatch, HttpMethod, MatchRule, PathSuffixMode, Route};
use crate::domain::matching::{best_route, match_route, query_allowed, suffix_allowed};

fn http_route(id: &str, methods: &[HttpMethod], path: &str) -> Route {
    Route {
        id: Some(id.to_string()),
        match_rule: MatchRule {
            http: Some(HttpMatch {
                methods: methods.iter().map(|m| m.as_str().to_string()).collect(),
                path: path.to_string(),
                ..HttpMatch::default()
            }),
            grpc: None,
        },
        ..Route::default()
    }
}

fn query(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn the_method_allowlist_admits_and_rejects() {
    let routes = [http_route("r1", &[HttpMethod::GET], "/v1/models")];
    assert!(best_route(&routes, "GET", "/v1/models", &[]).is_some());
    assert!(best_route(&routes, "POST", "/v1/models", &[]).is_none());
    assert!(match_route(&routes, "POST", "/v1/models", &[]).is_none());
}

#[test]
fn the_longest_path_prefix_wins() {
    let routes = [
        http_route("short", &[HttpMethod::GET], "/v1"),
        http_route("long", &[HttpMethod::GET], "/v1/models"),
    ];
    let matched = match_route(&routes, "GET", "/v1/models", &[]).unwrap();
    assert_eq!(matched.route_id, "long");
    assert_eq!(matched.route_path, "/v1/models");
}

#[test]
fn a_path_is_matched_by_prefix() {
    let routes = [http_route("r1", &[HttpMethod::GET], "/v1/models")];
    assert!(best_route(&routes, "GET", "/v1/models/gpt-4", &[]).is_some());
    assert!(best_route(&routes, "GET", "/v2/models", &[]).is_none());
}

#[test]
fn an_empty_query_allowlist_rejects_unknown_parameters() {
    let mut route = http_route("r1", &[HttpMethod::GET], "/v1/models");
    if let Some(http) = route.match_rule.http.as_mut() {
        http.query_allowlist = vec!["api-version".to_string()];
    }
    let routes = [route];
    // An allowed parameter passes.
    assert!(best_route(&routes, "GET", "/v1/models", &query(&[("api-version", "1")])).is_some());
    // An unknown parameter does not change which route wins, but the guard
    // rejects it: the caller is told the request is invalid, not unmatched.
    assert!(best_route(&routes, "GET", "/v1/models", &query(&[("debug", "1")])).is_some());
    assert!(!query_allowed(&routes[0], &query(&[("debug", "1")])));
    assert!(query_allowed(&routes[0], &query(&[("api-version", "1")])));
}

#[test]
fn append_mode_forward_paths_carry_the_suffix() {
    let mut route = http_route("r1", &[HttpMethod::GET], "/v1");
    if let Some(http) = route.match_rule.http.as_mut() {
        http.path_suffix_mode = PathSuffixMode::Append;
    }
    let routes = [route];
    let outcome = match_route(&routes, "GET", "/v1/models/gpt-4", &[]).unwrap();
    assert_eq!(
        outcome.forward_path("/models/gpt-4"),
        "/v1/models/gpt-4",
        "the suffix is appended to the route path"
    );
}

#[test]
fn disabled_mode_rejects_a_suffix() {
    let mut route = http_route("r1", &[HttpMethod::GET], "/v1/models");
    if let Some(http) = route.match_rule.http.as_mut() {
        http.path_suffix_mode = PathSuffixMode::Disabled;
    }
    let routes = [route];
    // The suffix mode is not a selection criterion: the route still matches so
    // the caller is told the suffix is illegal (400) rather than unmatched.
    assert!(best_route(&routes, "GET", "/v1/models", &[]).is_some());
    assert!(best_route(&routes, "GET", "/v1/models/gpt-4", &[]).is_some());
    assert!(!suffix_allowed(&routes[0], "/v1/models/gpt-4"));
    assert!(suffix_allowed(&routes[0], "/v1/models"));
}

#[test]
fn a_grpc_route_never_matches_an_http_request() {
    let route = Route {
        id: Some("grpc".to_string()),
        match_rule: MatchRule {
            http: None,
            grpc: Some(GrpcMatch {
                service: "svc.Example".to_string(),
                method: "Get".to_string(),
            }),
        },
        ..Route::default()
    };
    let routes = [route];
    assert!(best_route(&routes, "GET", "/v1/models", &[]).is_none());
}

#[test]
fn ties_are_broken_deterministically_by_route_id() {
    let routes = [
        http_route("b-route", &[HttpMethod::GET], "/v1/models"),
        http_route("a-route", &[HttpMethod::GET], "/v1/models"),
    ];
    let matched = match_route(&routes, "GET", "/v1/models", &[]).unwrap();
    assert_eq!(matched.route_id, "a-route");
}

#[test]
fn only_routes_with_an_exact_single_match_rule_are_candidates() {
    // A route carrying both rules is malformed and never matches.
    let both = Route {
        id: Some("both".to_string()),
        match_rule: MatchRule {
            http: Some(HttpMatch {
                methods: vec!["GET".to_string()],
                path: "/v1/models".to_string(),
                ..HttpMatch::default()
            }),
            grpc: Some(GrpcMatch::default()),
        },
        ..Route::default()
    };
    assert!(!crate::domain::matching::is_http_route(&both));
    let both_routes = [both];
    assert!(best_route(&both_routes, "GET", "/v1/models", &[]).is_none());
}
