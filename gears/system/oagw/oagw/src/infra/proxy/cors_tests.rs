//! Tests for the built-in CORS handler (`docs/ADR/0004`).
use super::*;

fn config() -> crate::domain::model::CorsConfig {
    crate::domain::model::CorsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://console.example".to_owned()],
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        expose_headers: vec!["x-trace-id".to_owned()],
        allow_credentials: false,
    }
}

fn cross_origin() -> CorsRequest {
    CorsRequest {
        method: "GET".to_owned(),
        origin: Some("https://console.example".to_owned()),
        request_method: None,
        request_headers: None,
    }
}

fn preflight() -> CorsRequest {
    CorsRequest {
        method: "OPTIONS".to_owned(),
        origin: Some("https://console.example".to_owned()),
        request_method: Some("POST".to_owned()),
        request_headers: Some("content-type, x-trace-id".to_owned()),
    }
}

fn header<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, value)| value.as_str())
}

#[test]
fn a_request_without_origin_is_not_cors() {
    let mut request = cross_origin();
    request.origin = None;
    assert!(matches!(
        evaluate(None, &request),
        CorsOutcome::NotApplicable
    ));
}

#[test]
fn a_disabled_policy_leaves_the_request_alone() {
    let mut policy = config();
    policy.enabled = false;
    assert!(matches!(
        evaluate(Some(&policy), &cross_origin()),
        CorsOutcome::NotApplicable
    ));
}

#[test]
fn an_absent_policy_leaves_the_request_alone() {
    assert!(matches!(
        evaluate(None, &cross_origin()),
        CorsOutcome::NotApplicable
    ));
}

#[test]
fn a_preflight_is_answered_locally_without_dialing() {
    let CorsOutcome::Preflight(headers) = evaluate(Some(&config()), &preflight()) else {
        panic!("an enabled preflight must be answered locally");
    };
    assert_eq!(
        header(&headers, ALLOW_ORIGIN),
        Some("https://console.example")
    );
    assert_eq!(header(&headers, ALLOW_METHODS), Some("POST"));
    assert_eq!(
        header(&headers, ALLOW_HEADERS),
        Some("content-type, x-trace-id")
    );
    assert_eq!(header(&headers, MAX_AGE), Some(PREFLIGHT_MAX_AGE));
    assert_eq!(header(&headers, ALLOW_CREDENTIALS), None);
    assert!(header(&headers, VARY).is_some_and(|value| value.contains("Origin")));
}

#[test]
fn a_preflight_echoes_credentials_when_configured() {
    let mut policy = config();
    policy.allow_credentials = true;
    let CorsOutcome::Preflight(headers) = evaluate(Some(&policy), &preflight()) else {
        panic!("preflight");
    };
    assert_eq!(header(&headers, ALLOW_CREDENTIALS), Some("true"));
}

#[test]
fn an_origin_outside_the_allowlist_is_rejected_with_403() {
    let mut request = cross_origin();
    request.origin = Some("https://evil.example".to_owned());
    let outcome = evaluate(Some(&config()), &request);
    let CorsOutcome::Rejected(failure) = outcome else {
        panic!("a disallowed origin must be rejected");
    };
    assert_eq!(failure.status, 403);
    assert_eq!(
        failure.type_uri,
        crate::domain::plugin::problem_type(crate::domain::plugin::CORS_ORIGIN_NOT_ALLOWED)
    );
    assert!(failure.detail.contains("https://evil.example"));
}

#[test]
fn a_method_outside_the_allowlist_is_rejected_with_403() {
    let mut request = cross_origin();
    request.method = "DELETE".to_owned();
    request.origin = Some("https://console.example".to_owned());
    let outcome = evaluate(Some(&config()), &request);
    let CorsOutcome::Rejected(failure) = outcome else {
        panic!("a disallowed method must be rejected");
    };
    assert_eq!(failure.status, 403);
    assert_eq!(
        failure.type_uri,
        crate::domain::plugin::problem_type(crate::domain::plugin::CORS_METHOD_NOT_ALLOWED)
    );
}

#[test]
fn an_allowed_actual_request_gets_the_cors_headers() {
    let mut policy = config();
    policy.allowed_origins = vec!["https://console.example".to_owned()];
    policy.allowed_methods = vec!["GET".to_owned(), "POST".to_owned()];
    let mut request = cross_origin();
    request.origin = Some("https://console.example".to_owned());
    let CorsOutcome::Allowed(headers) = evaluate(Some(&policy), &request) else {
        panic!("an allowed origin must proceed");
    };
    assert_eq!(
        header(&headers, ALLOW_ORIGIN),
        Some("https://console.example")
    );
    assert_eq!(header(&headers, VARY), Some("Origin"));
    assert_eq!(header(&headers, EXPOSE_HEADERS), Some("x-trace-id"));
}

#[test]
fn origin_matching_is_case_insensitive_and_exact() {
    let mut policy = config();
    policy.allowed_origins = vec!["https://Console.Example".to_owned()];
    assert!(origin_allowed(&policy, "https://console.example"));
    assert!(!origin_allowed(&policy, "https://console.example.evil"));
    assert!(!origin_allowed(&policy, "https://sub.console.example"));
}

#[test]
fn a_wildcard_origin_admits_everything() {
    let mut policy = config();
    policy.allowed_origins = vec!["*".to_owned()];
    assert!(origin_allowed(&policy, "https://anything.example"));
}

#[test]
fn method_matching_is_case_insensitive() {
    assert!(method_allowed(&config(), "get"));
    assert!(method_allowed(&config(), "POST"));
    assert!(!method_allowed(&config(), "TRACE"));
}

#[test]
fn preflight_detection_needs_all_three_markers() {
    assert!(is_preflight(&preflight()));
    let mut request = preflight();
    request.request_method = None;
    assert!(!is_preflight(&request));
    let mut request = preflight();
    request.origin = None;
    assert!(!is_preflight(&request));
    let mut request = preflight();
    request.method = "GET".to_owned();
    assert!(!is_preflight(&request));
}

#[test]
fn preflight_response_is_a_204() {
    let headers = vec![(ALLOW_ORIGIN, "https://console.example".to_owned())];
    let response = crate::infra::proxy::response::ProxyResponse::preflight(headers);
    assert_eq!(response.status, 204);
    assert_eq!(
        response
            .headers
            .get(ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("https://console.example")
    );
}
