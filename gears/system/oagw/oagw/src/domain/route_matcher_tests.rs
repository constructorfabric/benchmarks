//! Tests of the route matcher
//! (`cpt-cf-oagw-flow-request-proxy-route-matching`,
//! `cpt-cf-oagw-algo-request-proxy-route-select`).

use crate::domain::route_matcher::*;
use crate::domain::dto::{HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Route, RouteMatchType};
use uuid::Uuid;

fn route(path: &str, methods: &[HttpMethod]) -> Route {
    Route {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        upstream_id: Uuid::nil(),
        match_type: RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
    }
}

fn matched(outcome: RouteMatchOutcome) -> MatchedRoute {
    match outcome {
        RouteMatchOutcome::Matched(matched) => matched,
        other => panic!("expected a match, got {other:?}"),
    }
}

#[test]
fn a_prefix_matches_itself_and_whole_following_segments() {
    assert!(path_matches("/v1", "/v1"));
    assert!(path_matches("/v1", "/v1/orders"));
    assert!(!path_matches("/v1", "/v1x"));
    assert!(!path_matches("/v1", "/"));
}

#[test]
fn a_trailing_slash_in_the_route_path_changes_nothing() {
    assert!(path_matches("/v1/", "/v1"));
    assert!(path_matches("/v1/", "/v1/orders"));
    assert_eq!(suffix_of("/v1/", "/v1/orders"), "orders");
}

#[test]
fn the_suffix_is_everything_past_the_matched_prefix() {
    assert_eq!(suffix_of("/v1", "/v1"), "");
    assert_eq!(suffix_of("/v1", "/v1/orders"), "orders");
    assert_eq!(suffix_of("/v1", "/v1/a/b"), "a/b");
}

#[test]
fn a_method_in_the_allowlist_is_admitted() {
    let route = route("/v1", &[HttpMethod::Get, HttpMethod::Post]);
    assert!(method_allows(&route, "GET"));
    assert!(!method_allows(&route, "DELETE"));
    assert_eq!(methods_of(&route), vec!["GET", "POST"]);
}

#[test]
fn a_route_without_an_http_match_block_never_matches() {
    let mut route = route("/v1", &[HttpMethod::Get]);
    route.match_ = MatchConfig { http: None, grpc: None };
    assert!(!is_http_match(&route));
    assert!(!method_allows(&route, "GET"));
    assert!(methods_of(&route).is_empty());
}

#[test]
fn a_disabled_route_never_matches() {
    let mut route = route("/v1", &[HttpMethod::Get]);
    route.enabled = false;
    let outcome = select(
        &[CandidateRoute { route, tenant_distance: 0 }],
        "GET",
        "/v1",
        |_| PathSuffixMode::Append,
    );
    assert!(matches!(outcome, RouteMatchOutcome::NotFound));
}

#[test]
fn a_request_path_with_no_matching_route_is_not_found() {
    let outcome = select(
        &[CandidateRoute { route: route("/v2", &[HttpMethod::Get]), tenant_distance: 0 }],
        "GET",
        "/v1",
        |_| PathSuffixMode::Append,
    );
    assert!(matches!(outcome, RouteMatchOutcome::NotFound));
}

#[test]
fn a_path_matched_but_method_excluded_route_names_its_allowlist() {
    let outcome = select(
        &[CandidateRoute { route: route("/v1", &[HttpMethod::Get, HttpMethod::Put]), tenant_distance: 0 }],
        "DELETE",
        "/v1",
        |_| PathSuffixMode::Append,
    );
    match outcome {
        RouteMatchOutcome::MethodNotAllowed { allowed } => {
            assert_eq!(allowed, vec!["GET", "PUT"]);
        }
        other => panic!("expected method-not-allowed, got {other:?}"),
    }
}

#[test]
fn the_longest_matching_prefix_wins() {
    let outcome = select(
        &[
            CandidateRoute { route: route("/v1", &[HttpMethod::Get]), tenant_distance: 0 },
            CandidateRoute { route: route("/v1/orders", &[HttpMethod::Get]), tenant_distance: 0 },
        ],
        "GET",
        "/v1/orders/42",
        |_| PathSuffixMode::Append,
    );
    assert_eq!(matched(outcome).route.match_.http.expect("http").path, "/v1/orders");
}

#[test]
fn the_priority_outranks_a_shorter_prefix_at_equal_length() {
    let mut first = route("/v1", &[HttpMethod::Get]);
    first.priority = 1;
    let mut second = route("/v1", &[HttpMethod::Get]);
    second.priority = 9;
    let outcome = select(
        &[
            CandidateRoute { route: first, tenant_distance: 0 },
            CandidateRoute { route: second, tenant_distance: 0 },
        ],
        "GET",
        "/v1/orders",
        |_| PathSuffixMode::Append,
    );
    assert_eq!(matched(outcome).route.priority, 9);
}

#[test]
fn the_suffix_mode_of_the_selected_route_is_what_the_caller_supplied() {
    let outcome = select(
        &[CandidateRoute { route: route("/v1", &[HttpMethod::Get]), tenant_distance: 0 }],
        "GET",
        "/v1/orders",
        |_| PathSuffixMode::Disabled,
    );
    assert!(matches!(outcome, RouteMatchOutcome::NotFound));
}

#[test]
fn a_suffix_mode_of_append_matches_the_suffixed_request() {
    let outcome = select(
        &[CandidateRoute { route: route("/v1", &[HttpMethod::Get]), tenant_distance: 0 }],
        "GET",
        "/v1/orders",
        |_| PathSuffixMode::Append,
    );
    assert_eq!(matched(outcome).path_suffix, "orders");
}

#[test]
fn a_tenant_distance_is_carried_not_ranked() {
    let outcome = select(
        &[CandidateRoute { route: route("/v1", &[HttpMethod::Get]), tenant_distance: 3 }],
        "GET",
        "/v1",
        |_| PathSuffixMode::Append,
    );
    assert_eq!(matched(outcome).tenant_distance, 3);
}

#[test]
fn a_dot_segment_is_rejected() {
    assert!(reject_dot_segments("/v1/orders").is_ok());
    let error = reject_dot_segments("/v1/../orders").expect_err("a dot segment is rejected");
    assert!(format!("{error}").contains('.'));
    assert!(reject_dot_segments("/v1/./orders").is_err());
}

#[test]
fn a_suffix_that_escapes_the_prefix_is_rejected() {
    assert!(reject_suffix_escape("/v1", "/v1/orders").is_ok());
    assert!(reject_suffix_escape("/v1", "/v2/orders").is_err());
    assert!(reject_suffix_escape("/v1", "/v1//orders").is_err());
    assert!(reject_suffix_escape("/v1", "/v1/../v2").is_err());
}

#[test]
fn a_query_outside_the_allowlist_is_rejected() {
    let mut route = route("/v1", &[HttpMethod::Get]);
    route.match_.http.as_mut().expect("http").query_allowlist = vec!["limit".to_owned()];
    assert!(query_is_allowed(&route, Some("limit=1")).is_ok());
    assert!(query_is_allowed(&route, Some("offset=1")).is_err());
    assert!(query_is_allowed(&route, Some("limit=1&offset=2")).is_err());
    assert!(query_is_allowed(&route, None).is_ok());
}

#[test]
fn an_empty_allowlist_rejects_every_query_parameter() {
    let route = route("/v1", &[HttpMethod::Get]);
    assert!(query_is_allowed(&route, Some("limit=1")).is_err());
}

#[test]
fn query_pairs_keep_the_written_order_and_empty_values() {
    assert_eq!(query_pairs("a=1&b=2"), vec![("a".to_owned(), "1".to_owned()), ("b".to_owned(), "2".to_owned())]);
    assert_eq!(query_pairs("a=&b"), vec![("a".to_owned(), String::new()), ("b".to_owned(), String::new())]);
    assert!(query_pairs("").is_empty());
}

#[test]
fn the_selected_route_keeps_its_identifier_for_the_total_order() {
    let mut route = route("/v1", &[HttpMethod::Get]);
    route.id = Uuid::nil();
    let outcome = select(
        &[CandidateRoute { route, tenant_distance: 0 }],
        "GET",
        "/v1",
        |_| PathSuffixMode::Append,
    );
    assert_eq!(matched(outcome).matched_path, "/v1");
}
