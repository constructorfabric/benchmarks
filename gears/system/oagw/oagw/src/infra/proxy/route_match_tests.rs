#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::route_match::select_route;
use crate::domain::error::DomainError;
use crate::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Protocol, Route,
    ServerConfig, Upstream,
};

const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0xC001);
const UPSTREAM_ID: uuid::Uuid = uuid::Uuid::from_u128(0x11);

fn upstream() -> Upstream {
    Upstream {
        id: UPSTREAM_ID,
        tenant_id: TENANT,
        alias: "api.vendor.com".to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "api.vendor.com".to_owned(),
                port: 80,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

fn http_match(path: &str, methods: &[HttpMethod], suffix: PathSuffixMode) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: methods.to_vec(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: suffix,
        }),
        grpc: None,
    }
}

fn route(id: u128, path: &str, priority: u32, methods: &[HttpMethod]) -> Route {
    Route {
        id: uuid::Uuid::from_u128(id),
        tenant_id: TENANT,
        upstream_id: UPSTREAM_ID,
        r#match: http_match(path, methods, PathSuffixMode::Append),
        priority,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

/// A longer prefix shadows a shorter one regardless of list order.
#[test]
fn the_longest_prefix_wins() {
    let specific = route(0x01, "/v1/chat", 10, &[HttpMethod::Post]);
    let generic = route(0x02, "/v1", 20, &[HttpMethod::Post]);
    // Listed shortest-first to prove the selector is not order dependent.
    let selected = select_route(
        &upstream(),
        &[generic, specific],
        &http::Method::POST,
        "/v1/chat/completions",
        &[],
    )
    .unwrap();
    assert_eq!(selected.route.id, uuid::Uuid::from_u128(0x01));
}

/// The dispatch contract: prefix ties are broken by the lower `priority`
/// value.
#[test]
fn a_prefix_tie_is_broken_by_the_lowest_priority() {
    let low = route(0x01, "/v1", 5, &[HttpMethod::Get]);
    let high = route(0x02, "/v1", 50, &[HttpMethod::Get]);
    let selected = select_route(
        &upstream(),
        &[high, low],
        &http::Method::GET,
        "/v1/models",
        &[],
    )
    .unwrap();
    assert_eq!(selected.route.id, uuid::Uuid::from_u128(0x01));
}

/// An exact match is not treated as a prefix of a longer unrelated path.
#[test]
fn a_prefix_must_end_on_a_path_boundary() {
    let selected = select_route(
        &upstream(),
        &[route(0x01, "/v1", 1, &[HttpMethod::Get])],
        &http::Method::GET,
        "/v11/models",
        &[],
    )
    .unwrap_err();
    assert!(matches!(selected, DomainError::RouteNotFound { .. }));
}

/// The method must be in the route's allowlist; a method-matching route beats
/// a longer prefix that excludes the method.
#[test]
fn the_method_is_part_of_the_match() {
    let routes = [
        route(0x01, "/v1/chat", 1, &[HttpMethod::Post]),
        route(0x02, "/v1", 2, &[HttpMethod::Get]),
    ];
    let get = select_route(&upstream(), &routes, &http::Method::GET, "/v1/chat", &[]).unwrap();
    assert_eq!(get.route.id, uuid::Uuid::from_u128(0x02));

    let post = select_route(&upstream(), &routes, &http::Method::POST, "/v1/chat", &[]).unwrap();
    assert_eq!(post.route.id, uuid::Uuid::from_u128(0x01));
}

/// `path_suffix_mode: append` splices the unmatched remainder onto the route
/// path.
#[test]
fn append_mode_splices_the_suffix() {
    let selected = select_route(
        &upstream(),
        &[route(0x01, "/v1", 1, &[HttpMethod::Get])],
        &http::Method::GET,
        "/v1/models/curated",
        &[],
    )
    .unwrap();
    assert_eq!(selected.remainder, "/models/curated");
    assert_eq!(selected.outbound_path, "/v1/models/curated");
}

/// An exact hit keeps the route path verbatim and carries no remainder.
#[test]
fn an_exact_match_has_no_remainder() {
    let selected = select_route(
        &upstream(),
        &[route(0x01, "/v1/models", 1, &[HttpMethod::Get])],
        &http::Method::GET,
        "/v1/models",
        &[],
    )
    .unwrap();
    assert_eq!(selected.remainder, "");
    assert_eq!(selected.outbound_path, "/v1/models");
}

/// A root catch-all route forwards the whole proxy path instead of doubling
/// the leading slash.
#[test]
fn a_root_route_forwards_the_whole_path() {
    let selected = select_route(
        &upstream(),
        &[route(0x01, "/", 1, &[HttpMethod::Get])],
        &http::Method::GET,
        "/v1/models",
        &[],
    )
    .unwrap();
    assert_eq!(selected.outbound_path, "/v1/models");
}

/// `path_suffix_mode: disabled` accepts an exact path and refuses a suffix.
#[test]
fn disabled_suffix_mode_rejects_a_suffix() {
    let route = Route {
        r#match: http_match("/v1/models", &[HttpMethod::Get], PathSuffixMode::Disabled),
        ..route(0x01, "/v1/models", 1, &[HttpMethod::Get])
    };
    let exact = select_route(
        &upstream(),
        std::slice::from_ref(&route),
        &http::Method::GET,
        "/v1/models",
        &[],
    );
    assert!(exact.is_ok());

    let error = select_route(
        &upstream(),
        &[route],
        &http::Method::GET,
        "/v1/models/curated",
        &[],
    )
    .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

/// A disabled route never matches.
#[test]
fn a_disabled_route_is_invisible() {
    let mut row = route(0x01, "/v1", 1, &[HttpMethod::Get]);
    row.enabled = false;
    let error =
        select_route(&upstream(), &[row], &http::Method::GET, "/v1/models", &[]).unwrap_err();
    assert!(matches!(error, DomainError::RouteNotFound { .. }));
}

/// A route of another upstream is not a candidate.
#[test]
fn routes_of_other_upstreams_are_ignored() {
    let mut row = route(0x01, "/v1", 1, &[HttpMethod::Get]);
    row.upstream_id = uuid::Uuid::from_u128(0xFF);
    let error =
        select_route(&upstream(), &[row], &http::Method::GET, "/v1/models", &[]).unwrap_err();
    assert!(matches!(error, DomainError::RouteNotFound { .. }));
}

/// A gRPC match rule can never satisfy an HTTP request (`DESIGN` §3.3, gRPC is
/// Phase 3 with no reachable proxy code path).
#[test]
fn a_grpc_route_never_matches_an_http_request() {
    let mut row = route(0x01, "/v1", 1, &[HttpMethod::Get]);
    row.r#match = MatchConfig {
        http: None,
        grpc: Some(crate::domain::model::GrpcMatch {
            service: "svc".to_owned(),
            method: "m".to_owned(),
        }),
    };
    let error =
        select_route(&upstream(), &[row], &http::Method::GET, "/v1/models", &[]).unwrap_err();
    assert!(matches!(error, DomainError::RouteNotFound { .. }));
}

/// Query parameters outside the allowlist are rejected with a `400`.
#[test]
fn unknown_query_parameters_are_rejected() {
    let mut row = route(0x01, "/v1", 1, &[HttpMethod::Get]);
    if let Some(http) = row.r#match.http.as_mut() {
        http.query_allowlist = vec!["api-version".to_owned()];
    }
    let ok = select_route(
        &upstream(),
        &[row],
        &http::Method::GET,
        "/v1/models",
        &[("api-version", "2024-01")],
    );
    assert!(ok.is_ok());
}

/// An empty allowlist allows no query parameter at all.
#[test]
fn an_empty_allowlist_allows_no_query_parameter() {
    let routes = [route(0x01, "/v1", 1, &[HttpMethod::Get])];
    let without = select_route(&upstream(), &routes, &http::Method::GET, "/v1/models", &[]);
    assert!(without.is_ok());

    let with = select_route(
        &upstream(),
        &routes,
        &http::Method::GET,
        "/v1/models",
        &[("api-version", "2024-01")],
    )
    .unwrap_err();
    assert!(matches!(with, DomainError::Validation { .. }));
}

/// The allowlist matches parameter names case-sensitively, as query names are
/// case-sensitive per RFC 3986.
#[test]
fn query_parameter_names_are_compared_verbatim() {
    let mut row = route(0x01, "/v1", 1, &[HttpMethod::Get]);
    if let Some(http) = row.r#match.http.as_mut() {
        http.query_allowlist = vec!["api-version".to_owned()];
    }
    let error = select_route(
        &upstream(),
        &[row],
        &http::Method::GET,
        "/v1/models",
        &[("API-VERSION", "2024-01")],
    )
    .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}
