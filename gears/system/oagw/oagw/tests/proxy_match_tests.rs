//! Route matching over the resolved candidate set.
//!
//! Covers `cpt-cf-oagw-algo-route-match` and the acceptance rows of
//! `cpt-cf-oagw-dod-route-matching`: the method allowlist as the first filter,
//! the longest configured path that wins, the ascending `priority` tie-break of
//! §1.5, the `path_suffix_mode` decision and its shipped-schema default, the
//! query allowlist the matched route carries, the disabled and enabled flags
//! the candidates are filtered on, and the two failures the caller answers
//! 404 and 400 with.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::data_plane::match_route::{failure_of, match_route};
use oagw::domain::error::ErrorKind;
use oagw::domain::proxy::{AliasDerivation, ResolvedUpstream, RouteCandidate};
use oagw::domain::route::{HttpMatch, MatchConfig, PathSuffixMode, Route};
use oagw::domain::{Endpoint, EndpointHost, HeadersConfig, Scheme};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x31);
const UPSTREAM: Uuid = Uuid::from_u128(0x41);
const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// One route candidate of the set the match selects from.
fn candidate(route_id: u128, methods: &[&str], path: &str, priority: Option<i64>) -> RouteCandidate {
    candidate_with(route_id, methods, path, priority, None, true)
}

/// The same candidate with the suffix mode and the enabled flag stated.
fn candidate_with(
    route_id: u128,
    methods: &[&str],
    path: &str,
    priority: Option<i64>,
    suffix_mode: Option<PathSuffixMode>,
    enabled: bool,
) -> RouteCandidate {
    let mut http = HttpMatch {
        methods: methods.iter().map(|method| String::from(*method)).collect(),
        path: String::from(path),
        query_allowlist: Vec::new(),
        path_suffix_mode: None,
    };
    http.path_suffix_mode = suffix_mode;
    http.query_allowlist = vec![String::from("model")];
    RouteCandidate {
        tenant_id: TENANT,
        depth: 0,
        route: Route {
            id: Uuid::from_u128(route_id),
            upstream_id: UPSTREAM,
            match_config: MatchConfig {
                http: Some(http),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            tags: Vec::new(),
            cors: None,
            priority,
            enabled: Some(enabled),
        },
    }
}

/// A resolved upstream whose candidate set the caller supplies.
fn resolved(candidates: Vec<RouteCandidate>) -> ResolvedUpstream {
    ResolvedUpstream {
        cors: None,
        tenant_id: TENANT,
        upstream_id: UPSTREAM,
        alias: String::from("api.example.com"),
        alias_derivation: AliasDerivation::Explicit,
        endpoints: vec![Endpoint {
            scheme: Scheme::Https,
            host: EndpointHost::parse("api.example.com").expect("a valid endpoint host"),
            port: Some(8443),
        }],
        protocol: String::from(PROTOCOL_HTTP),
        enabled: true,
        headers: HeadersConfig::default(),
        rate_limit: None,
        plugins: None,
        route_candidates: candidates,
    }
}

#[test]
fn a_method_the_route_does_not_declare_is_never_a_candidate() {
    let upstream = resolved(vec![candidate(0x51, &["GET"], "/v1/chat", None)]);
    let outcome = match_route(&upstream, None, "POST", "/v1/chat", None);
    assert!(matches!(outcome, oagw::data_plane::MatchOutcome::NoMatch));
    let failure = failure_of(&outcome).expect("the no-match outcome is a failure");
    assert_eq!(failure.kind, ErrorKind::RouteNotFound);
}

#[test]
fn a_path_no_candidate_prefixes_is_never_a_match() {
    let upstream = resolved(vec![candidate(0x51, &["GET"], "/v1/chat", None)]);
    let outcome = match_route(&upstream, None, "GET", "/v2/other", None);
    assert!(matches!(outcome, oagw::data_plane::MatchOutcome::NoMatch));
}

#[test]
fn a_path_that_prefixes_only_at_a_non_boundary_is_never_a_match() {
    // `/v1` addresses `/v1/chat` and never `/v1chat`.
    let upstream = resolved(vec![candidate(0x51, &["GET"], "/v1", None)]);
    let outcome = match_route(&upstream, None, "GET", "/v1chat", None);
    assert!(matches!(outcome, oagw::data_plane::MatchOutcome::NoMatch));
}

#[test]
fn a_disabled_route_is_skipped_whatever_its_path() {
    let upstream = resolved(vec![candidate_with(
        0x51,
        &["GET"],
        "/v1/chat",
        None,
        None,
        false,
    )]);
    let outcome = match_route(&upstream, None, "GET", "/v1/chat", None);
    assert!(matches!(outcome, oagw::data_plane::MatchOutcome::NoMatch));
}

#[test]
fn the_longest_configured_prefix_wins() {
    let upstream = resolved(vec![
        candidate(0x51, &["GET"], "/v1", Some(1)),
        candidate(0x52, &["GET"], "/v1/chat", Some(9)),
    ]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat/completions", None);
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("the longer prefix addresses the request");
    };
    assert_eq!(route.route_id, Uuid::from_u128(0x52));
    assert_eq!(route.outbound_path, "/v1/chat");
}

#[test]
fn the_ascending_priority_breaks_a_tie_of_one_prefix() {
    let upstream = resolved(vec![
        candidate(0x52, &["GET"], "/v1/chat", Some(5)),
        candidate(0x51, &["GET"], "/v1/chat", Some(2)),
    ]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat", None);
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("one of the tied candidates matches");
    };
    assert_eq!(route.route_id, Uuid::from_u128(0x51));
    assert_eq!(route.priority, Some(2));
}

#[test]
fn a_route_that_declares_no_priority_is_the_least_specific_of_its_group() {
    let upstream = resolved(vec![
        candidate(0x51, &["GET"], "/v1/chat", None),
        candidate(0x52, &["GET"], "/v1/chat", Some(7)),
    ]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat", None);
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("one of the tied candidates matches");
    };
    assert_eq!(route.route_id, Uuid::from_u128(0x52));
}

#[test]
fn a_route_with_no_priority_never_beats_one_that_declares_a_smaller_value() {
    let upstream = resolved(vec![
        candidate(0x51, &["GET"], "/v1/chat", None),
        candidate(0x52, &["GET"], "/v1/chat", Some(1)),
    ]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat", None);
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("one of the tied candidates matches");
    };
    assert_eq!(route.route_id, Uuid::from_u128(0x52));
}

#[test]
fn the_shipped_schema_default_of_the_suffix_mode_is_append() {
    let upstream = resolved(vec![candidate(0x51, &["GET"], "/v1/chat", None)]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat/completions", Some("completions"));
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("an append route admits the suffix");
    };
    assert_eq!(route.outbound_path, "/v1/chat/completions");
}

#[test]
fn a_suffix_of_only_slashes_appends_nothing() {
    let upstream = resolved(vec![candidate(0x51, &["GET"], "/v1/chat", None)]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat/", Some("///"));
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("the route still matches");
    };
    assert_eq!(route.outbound_path, "/v1/chat");
}

#[test]
fn a_disabled_suffix_mode_rejects_the_suffix_the_route_received() {
    let upstream = resolved(vec![candidate_with(
        0x51,
        &["GET"],
        "/v1/chat",
        None,
        Some(PathSuffixMode::Disabled),
        true,
    )]);
    let outcome = match_route(&upstream, None, "GET", "/v1/chat", Some("completions"));
    assert!(matches!(outcome, oagw::data_plane::MatchOutcome::SuffixRejected));
    let failure = failure_of(&outcome).expect("the rejected suffix is a failure");
    assert_eq!(failure.kind, ErrorKind::ValidationError);
}

#[test]
fn a_disabled_suffix_mode_still_admits_the_alias_alone() {
    let upstream = resolved(vec![candidate_with(
        0x51,
        &["GET"],
        "/v1/chat",
        None,
        Some(PathSuffixMode::Disabled),
        true,
    )]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat", None);
    assert!(matches!(matched, oagw::data_plane::MatchOutcome::Matched(_)));
}

#[test]
fn the_matched_route_carries_the_query_allowlist_of_its_route() {
    let upstream = resolved(vec![candidate(0x51, &["GET"], "/v1/chat", None)]);
    let matched = match_route(&upstream, None, "GET", "/v1/chat", None);
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("the route matches");
    };
    assert_eq!(route.query_allowlist, vec![String::from("model")]);
}

#[test]
fn a_request_that_addresses_the_alias_alone_supplies_no_suffix() {
    let upstream = resolved(vec![candidate(0x51, &["GET"], "/v1", None)]);
    let matched = match_route(&upstream, None, "GET", "/v1", None);
    let oagw::data_plane::MatchOutcome::Matched(route) = matched else {
        panic!("the route matches itself");
    };
    assert_eq!(route.outbound_path, "/v1");
}
