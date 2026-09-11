//! Foundational wire-shape tests (T012): the DTOs round-trip the schemas, the
//! `http` endpoint scheme is a legal value at create time and the error enum
//! is safe to match non-exhaustively.

use crate::domain::dto::{
    Endpoint, EndpointScheme, HttpMatch, MatchRule, PathSuffixMode, Protocol, Route,
    SharingMode, Upstream,
};

#[test]
fn the_http_scheme_is_a_legal_endpoint_value() {
    let endpoint = Endpoint {
        scheme: EndpointScheme::Http,
        host: "stub.internal".to_string(),
        port: 8081,
    };
    assert_eq!(endpoint.scheme.default_port(), 80);
    assert_eq!(endpoint.effective_port(), 8081);
    assert_eq!(endpoint.host_with_port(), "stub.internal:8081");
}

#[test]
fn the_https_scheme_is_the_default() {
    let endpoint: Endpoint = serde_json::from_str(r#"{"host": "api.openai.com"}"#).unwrap();
    assert_eq!(endpoint.scheme, EndpointScheme::Https);
    assert_eq!(endpoint.effective_port(), 443);
}

#[test]
fn endpoint_schemes_round_trip() {
    for scheme in [EndpointScheme::Http, EndpointScheme::Https, EndpointScheme::Grpc] {
        let text = serde_json::to_string(&scheme).unwrap();
        let parsed: EndpointScheme = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, scheme);
    }
}

#[test]
fn sharing_modes_round_trip() {
    for mode in [SharingMode::Private, SharingMode::Inherit, SharingMode::Enforce] {
        let text = serde_json::to_string(&mode).unwrap();
        let parsed: SharingMode = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, mode);
    }
}

#[test]
fn an_upstream_round_trips_its_optional_fields() {
    let wire = serde_json::json!({
        "enabled": true,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 8081 } ] },
        "protocol": "http"
    });
    let upstream: Upstream = serde_json::from_value(wire).unwrap();
    assert_eq!(upstream.protocol, Protocol::Http);
    assert_eq!(upstream.server.endpoints.len(), 1);
    assert_eq!(upstream.server.endpoints[0].scheme, EndpointScheme::Http);
    // Absent optionals stay absent, on the wire as well as in memory.
    assert!(upstream.alias.is_none());
    assert!(upstream.cors.is_none());
    let back = serde_json::to_value(&upstream).unwrap();
    assert!(back.get("alias").is_none());
    assert!(back.get("cors").is_none());
}

#[test]
fn a_route_requires_exactly_one_match_rule() {
    let http = Route {
        match_rule: MatchRule {
            http: Some(HttpMatch {
                methods: vec!["GET".to_string()],
                path: "/v1/models".to_string(),
                path_suffix_mode: PathSuffixMode::Append,
                ..HttpMatch::default()
            }),
            grpc: None,
        },
        ..Route::default()
    };
    assert!(serde_json::to_value(&http).is_ok());
    assert!(http.match_rule.http.is_some());
    assert!(http.match_rule.grpc.is_none());
}

#[test]
fn unknown_fields_are_rejected() {
    // `deny_unknown_fields` guards the management API against typos that would
    // otherwise be silently dropped.
    let parsed: Result<Upstream, _> = serde_json::from_str(r#"{"not_a_field": 1}"#);
    assert!(parsed.is_err());
}
