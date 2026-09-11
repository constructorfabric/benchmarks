//! Unit tests of the built-in CORS handler (FEATURE entry 2.8).
// @cpt-dod:cpt-cf-oagw-dod-cors-unit-tests:p1

use super::cors::{
    append_vary_origin, canonical_origin, canonicalize, evaluate, is_legal_method, match_origin,
    merge_fields, preflight_response_headers, reflection_allowed, response_header_pairs,
    serveable, CorsOutcome, OriginVerdict, PreflightCors, MAX_AGE, REFLECTION_BOUND_BYTES,
};
use crate::domain::dto::{CorsConfig, SharingMode};
use crate::domain::error::DomainError;

fn config(origins: &[&str], methods: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: Some(origins.iter().map(|origin| (*origin).to_owned()).collect()),
        allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

fn credentials(origins: &[&str], methods: &[&str]) -> CorsConfig {
    let mut cors = config(origins, methods);
    cors.allow_credentials = true;
    cors
}

#[test]
fn a_same_host_origin_on_another_port_is_no_match() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    assert_eq!(match_origin("https://app.example.com:8443", cors.allowed_origins.as_ref().unwrap()), OriginVerdict::NoMatch);
}

#[test]
fn a_same_host_origin_on_another_scheme_is_no_match() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    assert_eq!(match_origin("http://app.example.com", cors.allowed_origins.as_ref().unwrap()), OriginVerdict::NoMatch);
}

#[test]
fn a_lookalike_host_is_no_match() {
    let cors = config(&["https://example.com"], &["GET"]);
    assert_eq!(
        match_origin("https://evil.com.example.com", cors.allowed_origins.as_ref().unwrap()),
        OriginVerdict::NoMatch
    );
    assert_eq!(
        match_origin("https://example.com.evil.com", cors.allowed_origins.as_ref().unwrap()),
        OriginVerdict::NoMatch
    );
}

#[test]
fn a_wildcard_host_is_never_a_relaxation() {
    let cors = config(&["https://*.example.com"], &["GET"]);
    assert_eq!(
        match_origin("https://app.example.com", cors.allowed_origins.as_ref().unwrap()),
        OriginVerdict::NoMatch
    );
}

#[test]
fn an_exact_entry_wins_over_the_wildcard() {
    let cors = config(&["*", "https://app.example.com"], &["GET"]);
    assert_eq!(
        match_origin("https://app.example.com", cors.allowed_origins.as_ref().unwrap()),
        OriginVerdict::Exact("https://app.example.com".to_owned())
    );
    assert_eq!(
        match_origin("https://other.example.com", cors.allowed_origins.as_ref().unwrap()),
        OriginVerdict::Wildcard
    );
}

#[test]
fn an_unparsable_origin_never_matches() {
    let cors = config(&["*"], &["GET"]);
    assert_eq!(match_origin("not an origin", cors.allowed_origins.as_ref().unwrap()), OriginVerdict::NoMatch);
    assert_eq!(match_origin("https://", cors.allowed_origins.as_ref().unwrap()), OriginVerdict::NoMatch);
}

#[test]
fn the_canonical_serialization_compares_across_the_default_port() {
    assert_eq!(canonical_origin("https://app.example.com:443"), canonical_origin("https://APP.example.com"));
    assert_eq!(canonical_origin("http://app.example.com:80"), Some("http://app.example.com".to_owned()));
    assert_ne!(canonical_origin("https://app.example.com:8443"), canonical_origin("https://app.example.com"));
    assert_eq!(canonical_origin("https://app.example.com"), Some("https://app.example.com".to_owned()));
}

#[test]
fn the_wildcard_verdict_is_legal_only_without_credentials() {
    let cors = config(&["*"], &["GET"]);
    assert_eq!(
        response_header_pairs(&cors, &OriginVerdict::Wildcard),
        vec![("access-control-allow-origin".to_owned(), "*".to_owned())]
    );
    let mut credentialed = credentials(&["*"], &["GET"]);
    credentialed.enabled = true;
    assert!(response_header_pairs(&credentialed, &OriginVerdict::Wildcard).is_empty());
}

#[test]
fn only_the_documented_methods_are_legal() {
    for method in ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] {
        assert!(is_legal_method(method), "{method} is documented");
    }
    for method in ["TRACE", "CONNECT", "get", "PROPFIND", ""] {
        assert!(!is_legal_method(method), "{method} is not documented");
    }
}

#[test]
fn the_method_check_uses_the_effective_set_verbatim() {
    let cors = config(&["https://app.example.com"], &["GET", "POST"]);
    let origin = Some("https://app.example.com");
    assert!(evaluate(Some(&cors), origin, "GET", None, None).rejection.is_none());
    let rejected = evaluate(Some(&cors), origin, "DELETE", Some("/v1"), Some("trace"));
    assert_eq!(rejected.outcome, Some(CorsOutcome::MethodNotAllowed));
    assert!(matches!(rejected.rejection, Some(DomainError::CorsMethodNotAllowed { .. })));
    assert!(rejected.headers.is_empty());
}

#[test]
fn the_origin_check_runs_before_the_method_check() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    let rejected = evaluate(Some(&cors), Some("https://evil.com"), "DELETE", None, None);
    assert_eq!(rejected.outcome, Some(CorsOutcome::OriginNotAllowed));
    assert!(matches!(rejected.rejection, Some(DomainError::CorsOriginNotAllowed { .. })));
}

#[test]
fn a_request_without_an_origin_is_not_a_cors_subject() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    let evaluation = evaluate(Some(&cors), None, "GET", None, None);
    assert_eq!(evaluation.outcome, None);
    assert!(evaluation.headers.is_empty());
    assert!(evaluation.rejection.is_none());
    assert!(!evaluation.vary, "a non-CORS request is not CORS-relevant");
    assert_eq!(evaluate(Some(&cors), Some(""), "GET", None, None).outcome, None);
}

#[test]
fn a_disabled_configuration_forwards_without_an_access_control_header() {
    let mut cors = config(&["https://app.example.com"], &["GET"]);
    cors.enabled = false;
    let evaluation = evaluate(Some(&cors), Some("https://app.example.com"), "GET", None, None);
    assert_eq!(evaluation.outcome, None);
    assert!(evaluation.rejection.is_none());
    assert!(evaluation.headers.is_empty(), "{:?}", evaluation.headers);
    assert!(evaluation.vary);
    for cors in [None, Some(&cors)] {
        let evaluation = evaluate(cors, Some("https://app.example.com"), "GET", None, None);
        assert!(evaluation.headers.iter().all(|(name, _)| !name.starts_with("access-control")));
        assert!(evaluation.vary);
    }
}

#[test]
fn a_merged_configuration_combining_credentials_with_a_wildcard_is_fail_closed() {
    let mut cors = credentials(&["*"], &["GET"]);
    cors.allowed_origins = Some(vec!["*".to_owned()]);
    let evaluation = evaluate(Some(&cors), Some("https://app.example.com"), "GET", None, None);
    assert_eq!(evaluation.outcome, Some(CorsOutcome::MergedConfigRejected));
    assert!(matches!(evaluation.rejection, Some(DomainError::CorsOriginNotAllowed { .. })));
    assert!(!serveable(&cors));
}

#[test]
fn an_allowed_request_carries_the_documented_response_headers() {
    let mut cors = credentials(&["https://app.example.com"], &["GET"]);
    cors.expose_headers = vec!["X-Request-Id".to_owned(), "X-Rate-Limit".to_owned()];
    let evaluation = evaluate(Some(&cors), Some("https://app.example.com"), "GET", None, None);
    assert_eq!(evaluation.outcome, None);
    assert!(evaluation.rejection.is_none());
    assert!(evaluation.vary);
    assert_eq!(
        evaluation.headers,
        vec![
            ("access-control-allow-origin".to_owned(), "https://app.example.com".to_owned()),
            ("access-control-expose-headers".to_owned(), "X-Request-Id, X-Rate-Limit".to_owned()),
            ("access-control-allow-credentials".to_owned(), "true".to_owned()),
        ]
    );
}

#[test]
fn the_expose_headers_header_is_omitted_when_nothing_is_configured() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    let evaluation = evaluate(Some(&cors), Some("https://app.example.com"), "GET", None, None);
    assert!(evaluation
        .headers
        .iter()
        .all(|(name, _)| name != "access-control-expose-headers"));
}

#[test]
fn a_preflight_answer_echoes_within_the_reflection_bounds() {
    let headers = preflight_response_headers(
        Some("https://app.example.com"),
        Some("DELETE"),
        Some("X-Request-Id, Content-Type"),
        Some(PreflightCors { allow_credentials: true, exact: true }),
    );
    assert_eq!(
        headers,
        vec![
            ("access-control-allow-origin".to_owned(), "https://app.example.com".to_owned()),
            ("access-control-allow-methods".to_owned(), "DELETE".to_owned()),
            ("access-control-allow-headers".to_owned(), "X-Request-Id, Content-Type".to_owned()),
            ("access-control-max-age".to_owned(), "86400".to_owned()),
            (
                "vary".to_owned(),
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned()
            ),
            ("access-control-allow-credentials".to_owned(), "true".to_owned()),
        ]
    );
}

#[test]
fn a_preflight_answer_without_an_effective_configuration_stays_credential_free() {
    for cors in [None, Some(PreflightCors { allow_credentials: true, exact: false })] {
        let headers = preflight_response_headers(Some("https://app.example.com"), Some("GET"), None, cors);
        assert!(headers.iter().all(|(name, _)| name != "access-control-allow-credentials"));
        assert!(headers.iter().any(|(name, value)| name == "access-control-allow-origin"
            && value == "https://app.example.com"));
    }
}

#[test]
fn a_preflight_answer_reflects_nothing_outside_the_legal_method_set() {
    let headers = preflight_response_headers(Some("https://app.example.com"), Some("TRACE"), None, None);
    assert!(headers.iter().all(|(name, _)| name != "access-control-allow-methods"));
    assert!(headers.iter().any(|(name, value)| name == "access-control-max-age" && value == MAX_AGE));
}

#[test]
fn a_preflight_answer_reflects_no_control_character_and_no_overlong_value() {
    assert!(!reflection_allowed("https://app.example.com\r\nX-Injected: 1"));
    assert!(!reflection_allowed("https://app.example.com\n"));
    assert!(reflection_allowed(&"a".repeat(REFLECTION_BOUND_BYTES)));
    assert!(!reflection_allowed(&"a".repeat(REFLECTION_BOUND_BYTES + 1)));
    let headers = preflight_response_headers(
        Some("https://app.example.com\r\nX-Injected: 1"),
        Some("GET"),
        Some("X-Request-Id"),
        None,
    );
    assert!(headers.iter().all(|(name, _)| name != "access-control-allow-origin"));
    assert!(headers.iter().any(|(name, _)| name == "access-control-allow-headers"));
}

#[test]
fn a_preflight_answer_never_carries_the_expose_headers_header() {
    let headers = preflight_response_headers(Some("https://app.example.com"), Some("GET"), None, None);
    assert!(headers.iter().all(|(name, _)| name != "access-control-expose-headers"));
}

#[test]
fn the_vary_of_a_response_is_appended_to_and_never_overwritten() {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("vary".to_owned(), "Accept-Encoding".to_owned()),
    ];
    append_vary_origin(&mut headers);
    assert_eq!(
        headers,
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("vary".to_owned(), "Accept-Encoding, Origin".to_owned()),
        ]
    );
    append_vary_origin(&mut headers);
    assert_eq!(headers[1].1, "Accept-Encoding, Origin");
}

#[test]
fn a_response_without_a_vary_gains_one() {
    let mut headers: Vec<(String, String)> = Vec::new();
    append_vary_origin(&mut headers);
    assert_eq!(headers, vec![("vary".to_owned(), "Origin".to_owned())]);
}

#[test]
fn the_merge_unions_the_origin_set_add_only() {
    let ancestor = config(&["https://ancestor.example.com"], &["GET"]);
    let descendant = config(&["https://ancestor.example.com", "https://leaf.example.com"], &["GET"]);
    let merged = merge_fields(&ancestor, &descendant);
    assert_eq!(
        merged.allowed_origins,
        Some(vec![
            "https://ancestor.example.com".to_owned(),
            "https://leaf.example.com".to_owned(),
        ])
    );
}

#[test]
fn the_merge_unions_the_methods_and_the_exposed_headers() {
    let ancestor = config(&["*"], &["GET", "POST"]);
    let descendant = config(&["*"], &["POST", "DELETE"]);
    let merged = merge_fields(&ancestor, &descendant);
    assert_eq!(merged.allowed_methods, ["GET", "POST", "DELETE"]);
    let mut exposing = config(&["*"], &["GET"]);
    exposing.expose_headers = vec!["X-A".to_owned()];
    let mut exposing_more = config(&["*"], &["GET"]);
    exposing_more.expose_headers = vec!["X-A".to_owned(), "X-B".to_owned()];
    assert_eq!(merge_fields(&exposing, &exposing_more).expose_headers, ["X-A", "X-B"]);
}

#[test]
fn the_merge_takes_enabled_from_the_more_specific_layer() {
    let mut ancestor = config(&["*"], &["GET"]);
    ancestor.enabled = true;
    let mut descendant = config(&["*"], &["GET"]);
    descendant.enabled = false;
    assert!(!merge_fields(&ancestor, &descendant).enabled);
    descendant.enabled = true;
    assert!(merge_fields(&ancestor, &descendant).enabled);
}

#[test]
fn the_merge_escalates_credentials_monotonically() {
    let mut ancestor = config(&["https://app.example.com"], &["GET"]);
    ancestor.allow_credentials = true;
    let descendant = config(&["https://app.example.com"], &["GET"]);
    assert!(merge_fields(&ancestor, &descendant).allow_credentials);
    let mut overriding = config(&["https://app.example.com"], &["GET"]);
    overriding.allow_credentials = true;
    assert!(merge_fields(&ancestor, &overriding).allow_credentials);
    let plain = config(&["https://app.example.com"], &["GET"]);
    assert!(!merge_fields(&plain, &descendant).allow_credentials);
}

#[test]
fn canonicalization_stores_the_canonical_form() {
    let mut cors = config(&["https://app.example.com:443", "https://other.example.com:8443"], &["GET"]);
    canonicalize(&mut cors);
    assert_eq!(
        cors.allowed_origins,
        Some(vec!["https://app.example.com".to_owned(), "https://other.example.com:8443".to_owned()])
    );
}
