//! Unit tests for route matching.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::model::{
    GrpcMatch, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, PluginsConfig, Route,
};
use uuid::Uuid;

fn route(id: &str, methods: Vec<HttpMethod>, path: &str, priority: i32) -> Route {
    route_with_suffix(id, methods, path, priority, PathSuffixMode::Append)
}

fn route_with_suffix(
    id: &str,
    methods: Vec<HttpMethod>,
    path: &str,
    priority: i32,
    mode: PathSuffixMode,
) -> Route {
    Route {
        id: id.to_owned(),
        tenant_id: Uuid::nil(),
        upstream_id: "u".to_owned(),
        enabled: true,
        priority,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods,
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: mode,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: PluginsConfig::default(),
        tags: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

#[test]
fn a_method_outside_the_allowlist_does_not_match() {
    let candidate = route("r", vec![HttpMethod::Get], "/v1/chat", 0);
    assert_eq!(
        matches_http(&candidate, "POST", "/v1/chat"),
        MatchOutcome::NoMatch
    );
    assert_eq!(
        matches_http(&candidate, "GET", "/v1/chat"),
        MatchOutcome::Matched {
            suffix: String::new()
        }
    );
}

#[test]
fn method_comparison_is_case_insensitive() {
    let candidate = route("r", vec![HttpMethod::Post], "/v1", 0);
    assert_eq!(
        matches_http(&candidate, "post", "/v1"),
        MatchOutcome::Matched {
            suffix: String::new()
        }
    );
}

#[test]
fn longest_prefix_wins_over_priority() {
    let mut candidates = vec![
        route("short", vec![HttpMethod::Get], "/v1", 100),
        route("long", vec![HttpMethod::Get], "/v1/chat/completions", 0),
    ];
    order_candidates(&mut candidates);
    let selected = select(&candidates, "GET", "/v1/chat/completions");
    assert_eq!(selected.expect("a route").0.id, "long");
}

#[test]
fn priority_breaks_equal_prefix_ties() {
    let mut candidates = vec![
        route("low", vec![HttpMethod::Get], "/v1/chat", 1),
        route("high", vec![HttpMethod::Get], "/v1/chat", 9),
    ];
    order_candidates(&mut candidates);
    let selected = select(&candidates, "GET", "/v1/chat");
    assert_eq!(selected.expect("a route").0.id, "high");
}

#[test]
fn append_mode_returns_the_remaining_path() {
    let candidate = route("r", vec![HttpMethod::Get], "/v1/chat", 0);
    assert_eq!(
        matches_http(&candidate, "GET", "/v1/chat/extra/deep"),
        MatchOutcome::Matched {
            suffix: "extra/deep".to_owned()
        }
    );
}

#[test]
fn disabled_mode_requires_an_exact_path() {
    let candidate = route_with_suffix(
        "r",
        vec![HttpMethod::Get],
        "/v1/chat",
        0,
        PathSuffixMode::Disabled,
    );
    assert_eq!(
        matches_http(&candidate, "GET", "/v1/chat/extra"),
        MatchOutcome::NoMatch
    );
    assert_eq!(
        matches_http(&candidate, "GET", "/v1/chat"),
        MatchOutcome::Matched {
            suffix: String::new()
        }
    );
}

#[test]
fn disabled_routes_are_skipped() {
    let mut candidate = route("r", vec![HttpMethod::Get], "/v1", 0);
    candidate.enabled = false;
    assert_eq!(
        matches_http(&candidate, "GET", "/v1/x"),
        MatchOutcome::NoMatch
    );
}

#[test]
fn trailing_slashes_are_normalised() {
    let candidate = route("r", vec![HttpMethod::Get], "/v1/chat/", 0);
    assert_eq!(
        matches_http(&candidate, "GET", "/v1/chat"),
        MatchOutcome::Matched {
            suffix: String::new()
        }
    );
}

#[test]
fn sibling_prefixes_are_not_confused() {
    let candidate = route("r", vec![HttpMethod::Get], "/v1/chat", 0);
    assert_eq!(
        matches_http(&candidate, "GET", "/v1/chatbot"),
        MatchOutcome::NoMatch
    );
}

#[test]
fn an_empty_candidate_list_has_no_match() {
    assert!(select(&[], "GET", "/v1").is_none());
}

#[test]
fn grpc_routes_match_service_and_optional_method() {
    let mut candidate = route("r", vec![HttpMethod::Get], "/v1", 0);
    candidate.match_config.http = None;
    candidate.match_config.grpc = Some(GrpcMatch {
        service: "pkg.Svc".to_owned(),
        method: "Do".to_owned(),
    });
    assert!(matches_grpc(&candidate, "pkg.Svc", "Do"));
    assert!(!matches_grpc(&candidate, "pkg.Other", "Do"));

    candidate.match_config.grpc.as_mut().unwrap().method = String::new();
    assert!(matches_grpc(&candidate, "pkg.Svc", "Anything"));
}
