use std::sync::atomic::AtomicUsize;

use super::{
    MatchInput, RoundRobin, http_path_suffix, is_target_host_shape, select_endpoint, select_route,
};
use crate::domain::error::ErrorKind;
use crate::domain::model::{
    Endpoint, GrpcMatch, HttpMatch, PathSuffixMode, Route, RouteMatch, RouteSpec, Scheme,
    UpstreamSpec,
};
use uuid::Uuid;

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: host.to_owned(),
        port,
    }
}

fn route(pattern: &str, methods: &[&str], priority: i32, suffix: PathSuffixMode) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::from_u128(1),
        upstream_id: Uuid::from_u128(2),
        spec: RouteSpec {
            r#match: RouteMatch::Http(HttpMatch {
                methods: methods.iter().map(|method| (*method).to_owned()).collect(),
                path: pattern.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: suffix,
            }),
            priority,
            ..RouteSpec::default()
        },
        created_at: 0,
        updated_at: 0,
    }
}

fn grpc_route(service: &str, method: &str) -> Route {
    Route {
        spec: RouteSpec {
            r#match: RouteMatch::Grpc(GrpcMatch {
                service: service.to_owned(),
                method: method.to_owned(),
            }),
            ..RouteSpec::default()
        },
        ..route("/api", &["GET"], 0, PathSuffixMode::Append)
    }
}

fn allowlisted_route(pattern: &str, allowlist: &[&str]) -> Route {
    Route {
        spec: RouteSpec {
            r#match: RouteMatch::Http(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: pattern.to_owned(),
                query_allowlist: allowlist.iter().map(|key| (*key).to_owned()).collect(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            ..RouteSpec::default()
        },
        ..route(pattern, &["GET"], 0, PathSuffixMode::Append)
    }
}

/// Owned request inputs, borrowed as a `MatchInput` at the call site.
struct Inputs {
    method: String,
    path: String,
    keys: Vec<String>,
}

impl Inputs {
    fn new(method: &str, path: &str, keys: &[&str]) -> Self {
        Self {
            method: method.to_owned(),
            path: path.to_owned(),
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    fn as_input(&self) -> MatchInput<'_> {
        MatchInput {
            method: &self.method,
            path: &self.path,
            query_keys: &self.keys,
        }
    }
}

#[test]
fn the_longest_path_pattern_wins() {
    let routes = [
        route("/api", &["GET"], 0, PathSuffixMode::Append),
        route("/api/orders", &["GET"], 0, PathSuffixMode::Append),
    ];
    let candidates: Vec<&Route> = routes.iter().collect();
    let inputs = Inputs::new("GET", "/api/orders/42", &[]);
    let selection = select_route(&candidates, &inputs.as_input())
        .unwrap_or_else(|error| panic!("select_route: {error}"));
    assert_eq!(selection.path, "/api/orders/42");
}

#[test]
fn priority_breaks_ties_between_equally_long_patterns() {
    let routes = [
        route("/api/orders", &["GET"], 1, PathSuffixMode::Append),
        route("/api/orders", &["GET"], 9, PathSuffixMode::Append),
    ];
    let candidates: Vec<&Route> = routes.iter().collect();
    let inputs = Inputs::new("GET", "/api/orders", &[]);
    let selection = select_route(&candidates, &inputs.as_input())
        .unwrap_or_else(|error| panic!("select_route: {error}"));
    assert_eq!(selection.route.spec.priority, 9);
}

#[test]
fn a_descendant_route_shadows_an_ancestor_one() {
    let routes = [
        route("/api", &["GET"], 9, PathSuffixMode::Append),
        route("/api/orders", &["GET"], 0, PathSuffixMode::Append),
    ];
    let candidates: Vec<&Route> = routes.iter().collect();
    let inputs = Inputs::new("GET", "/api/orders", &[]);
    let selection = select_route(&candidates, &inputs.as_input())
        .unwrap_or_else(|error| panic!("select_route: {error}"));
    assert_eq!(selection.path, "/api/orders");
}

#[test]
fn methods_are_case_insensitive_and_enforced() {
    let routes = [route("/api", &["GET", "POST"], 0, PathSuffixMode::Append)];
    let candidates: Vec<&Route> = routes.iter().collect();
    let lowercase = Inputs::new("get", "/api", &[]);
    assert!(select_route(&candidates, &lowercase.as_input()).is_ok());
    let mismatch = Inputs::new("DELETE", "/api", &[]);
    let error = select_route(&candidates, &mismatch.as_input()).unwrap_err();
    assert_eq!(error.kind, ErrorKind::RouteNotFound);
}

#[test]
fn a_path_suffix_is_appended_by_default() {
    let routes = [route("/api", &["GET"], 0, PathSuffixMode::Append)];
    let candidates: Vec<&Route> = routes.iter().collect();
    let inputs = Inputs::new("GET", "/api/v2/orders", &[]);
    let selection = select_route(&candidates, &inputs.as_input())
        .unwrap_or_else(|error| panic!("select_route: {error}"));
    assert_eq!(selection.path, "/api/v2/orders");
}

#[test]
fn a_path_suffix_is_rejected_when_disabled() {
    let routes = [route("/api", &["GET"], 0, PathSuffixMode::Disabled)];
    let candidates: Vec<&Route> = routes.iter().collect();
    let exact = Inputs::new("GET", "/api", &[]);
    assert!(select_route(&candidates, &exact.as_input()).is_ok());
    let suffixed = Inputs::new("GET", "/api/v2", &[]);
    let error = select_route(&candidates, &suffixed.as_input()).unwrap_err();
    assert_eq!(error.kind, ErrorKind::RouteNotFound);
}

#[test]
fn query_parameters_outside_the_allowlist_are_rejected() {
    let restricted = allowlisted_route("/api", &["page"]);
    let candidates: Vec<&Route> = vec![&restricted];
    let allowed = Inputs::new("GET", "/api", &["page"]);
    assert!(select_route(&candidates, &allowed.as_input()).is_ok());
    let rejected = Inputs::new("GET", "/api", &["page", "debug"]);
    let error = select_route(&candidates, &rejected.as_input()).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Validation);
}

#[test]
fn unmatched_paths_are_a_route_not_found() {
    let routes = [route("/api", &["GET"], 0, PathSuffixMode::Append)];
    let candidates: Vec<&Route> = routes.iter().collect();
    let inputs = Inputs::new("GET", "/other", &[]);
    let error = select_route(&candidates, &inputs.as_input()).unwrap_err();
    assert_eq!(error.kind, ErrorKind::RouteNotFound);
}

#[test]
fn grpc_routes_never_match_http_requests() {
    let grpc = grpc_route("pkg.Svc", "Get");
    let candidates: Vec<&Route> = vec![&grpc];
    let inputs = Inputs::new("GET", "/api", &[]);
    let error = select_route(&candidates, &inputs.as_input()).unwrap_err();
    assert_eq!(error.kind, ErrorKind::RouteNotFound);
}

#[test]
fn path_suffixes_are_split_on_the_route_boundary() {
    let rules = HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/api".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert_eq!(http_path_suffix(&rules, "/api/orders"), Some("/orders"));
    assert_eq!(http_path_suffix(&rules, "/api"), Some(""));
    assert_eq!(http_path_suffix(&rules, "/api/"), Some(""));
    assert_eq!(http_path_suffix(&rules, "/apiangular"), None);
    assert_eq!(http_path_suffix(&rules, "/other"), None);
}

#[test]
fn a_single_endpoint_pool_never_needs_a_target_host() {
    let endpoints = vec![endpoint("one.example.com", 443)];
    let round_robin = RoundRobin::new();
    let selected = select_endpoint(&endpoints, "alias", false, None, &round_robin)
        .unwrap_or_else(|error| panic!("select_endpoint: {error}"));
    assert_eq!(selected.host, "one.example.com");
}

#[test]
fn a_single_endpoint_still_validates_the_target_host() {
    let endpoints = vec![endpoint("one.example.com", 443)];
    let round_robin = RoundRobin::new();
    let error = select_endpoint(
        &endpoints,
        "alias",
        false,
        Some("two.example.com"),
        &round_robin,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::UnknownTargetHost);
    assert_eq!(
        error.extensions.invalid_value.as_deref(),
        Some("two.example.com")
    );
    assert_eq!(
        error.extensions.valid_hosts,
        vec!["one.example.com".to_owned()]
    );
}

#[test]
fn shared_suffix_pools_demand_a_target_host() {
    let endpoints = vec![
        endpoint("one.example.com", 443),
        endpoint("two.example.com", 443),
    ];
    let round_robin = RoundRobin::new();
    let error = select_endpoint(&endpoints, "alias", false, None, &round_robin).unwrap_err();
    assert_eq!(error.kind, ErrorKind::MissingTargetHost);
    assert_eq!(error.extensions.alias.as_deref(), Some("alias"));
    assert_eq!(error.extensions.valid_hosts.len(), 2);
}

#[test]
fn shared_suffix_pools_route_to_the_requested_host() {
    let endpoints = vec![
        endpoint("one.example.com", 443),
        endpoint("two.example.com", 443),
    ];
    let round_robin = RoundRobin::new();
    let selected = select_endpoint(
        &endpoints,
        "alias",
        false,
        Some("two.example.com"),
        &round_robin,
    )
    .unwrap_or_else(|error| panic!("select_endpoint: {error}"));
    assert_eq!(selected.host, "two.example.com");
}

#[test]
fn unknown_hosts_in_a_shared_pool_are_rejected() {
    let endpoints = vec![
        endpoint("one.example.com", 443),
        endpoint("two.example.com", 443),
    ];
    let round_robin = RoundRobin::new();
    let error = select_endpoint(
        &endpoints,
        "alias",
        false,
        Some("three.example.com"),
        &round_robin,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::UnknownTargetHost);
}

#[test]
fn distinct_suffix_pools_round_robin_when_no_host_is_given() {
    let endpoints = vec![endpoint("a.example.com", 443), endpoint("b.other.org", 443)];
    let round_robin = RoundRobin::new();
    let first = select_endpoint(&endpoints, "alias", false, None, &round_robin)
        .unwrap_or_else(|error| panic!("select_endpoint: {error}"));
    let second = select_endpoint(&endpoints, "alias", false, None, &round_robin)
        .unwrap_or_else(|error| panic!("select_endpoint: {error}"));
    assert_ne!(first.host, second.host, "consecutive picks must alternate");
}

#[test]
fn a_trailing_dot_and_case_do_not_change_host_identity() {
    let endpoints = vec![endpoint("a.example.com", 443), endpoint("b.other.org", 443)];
    let round_robin = RoundRobin::new();
    let selected = select_endpoint(
        &endpoints,
        "alias",
        false,
        Some("A.Example.COM."),
        &round_robin,
    )
    .unwrap_or_else(|error| panic!("select_endpoint: {error}"));
    assert_eq!(selected.host, "a.example.com");
}

#[test]
fn a_port_in_the_header_makes_it_invalid() {
    let endpoints = vec![endpoint("a.example.com", 443), endpoint("b.other.org", 443)];
    let round_robin = RoundRobin::new();
    let error = select_endpoint(
        &endpoints,
        "alias",
        false,
        Some("b.other.org:8443"),
        &round_robin,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidTargetHost);
}

#[test]
fn malformed_target_hosts_are_rejected_before_the_lookup() {
    let endpoints = vec![endpoint("a.example.com", 443), endpoint("b.other.org", 443)];
    let round_robin = RoundRobin::new();
    for host in ["", "https://a.example.com", "a.example.com/path", "@host"] {
        let error =
            select_endpoint(&endpoints, "alias", false, Some(host), &round_robin).unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::InvalidTargetHost,
            "unexpected verdict for `{host}`"
        );
    }
}

#[test]
fn bare_hostnames_and_ip_literals_are_valid_target_hosts() {
    assert!(is_target_host_shape("a.example.com"));
    assert!(is_target_host_shape("10.0.0.1"));
    assert!(is_target_host_shape("[::1]"));
    assert!(!is_target_host_shape("a.example.com:443"));
    assert!(!is_target_host_shape("a.example.com/x"));
    assert!(!is_target_host_shape(""));
}

#[test]
fn the_round_robin_cursor_advances_over_the_pool() {
    let cursor = RoundRobin {
        cursor: AtomicUsize::new(0),
    };
    assert_eq!(cursor.next(3), 0);
    assert_eq!(cursor.next(3), 1);
    assert_eq!(cursor.next(3), 2);
    assert_eq!(cursor.next(3), 0);
    assert_eq!(cursor.next(0), 0);
}

#[test]
fn an_endpoint_pool_of_one_is_returned_for_every_request() {
    let endpoints = vec![endpoint("solo.example.com", 8080)];
    let round_robin = RoundRobin::new();
    for _ in 0..3 {
        let selected = select_endpoint(&endpoints, "alias", false, None, &round_robin)
            .unwrap_or_else(|error| panic!("select_endpoint: {error}"));
        assert_eq!(selected.host, "solo.example.com");
    }
}

#[test]
fn an_empty_endpoint_pool_reports_a_missing_target_host() {
    let spec = UpstreamSpec::default();
    let round_robin = RoundRobin::new();
    let error =
        select_endpoint(&spec.server.endpoints, "alias", false, None, &round_robin).unwrap_err();
    assert_eq!(error.kind, ErrorKind::MissingTargetHost);
    assert!(spec.server.endpoints.is_empty());
}
