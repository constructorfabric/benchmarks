//! Tests for [`crate::domain::cors`].

use axum::http::{HeaderName, HeaderValue, StatusCode};

use super::{
    ACTUAL_VARY, ALLOW_CREDENTIALS_HEADER, ALLOW_HEADERS_HEADER, ALLOW_METHODS_HEADER,
    ALLOW_ORIGIN_HEADER, CORS_METHOD_NOT_ALLOWED_TYPE, CORS_ORIGIN_NOT_ALLOWED_TYPE,
    EXPOSE_HEADERS_HEADER, EffectiveCorsConfig, MAX_AGE_HEADER, ORIGIN_HEADER, PREFLIGHT_MAX_AGE,
    PREFLIGHT_VARY, PreflightResponse, REQUEST_HEADERS_HEADER, REQUEST_METHOD_HEADER, VARY_HEADER,
    WILDCARD_ORIGIN, actual_response_headers, evaluate_preflight, is_preflight, merge_cors,
    validate_actual_request, validate_cors_config,
};
use crate::domain::model::{CorsConfig, CorsMethod, SharingMode};

fn cors_config(origins: &[&str]) -> CorsConfig {
    CorsConfig {
        enabled: true,
        sharing: SharingMode::Private,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: vec![CorsMethod::Get, CorsMethod::Post],
        allow_headers: vec!["x-trace-id".to_owned()],
        expose_headers: vec!["x-request-id".to_owned()],
        allow_credentials: false,
        max_age: None,
    }
}

fn effective(origins: &[&str]) -> EffectiveCorsConfig {
    EffectiveCorsConfig::from(&cors_config(origins))
}

fn header_value(headers: &[(HeaderName, HeaderValue)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.as_str() == name)
        .map(|(_, value)| value.to_str().unwrap_or_default().to_owned())
}

// ---------------------------------------------------------------------------
// Origin matching
// ---------------------------------------------------------------------------

#[test]
fn origin_match_is_exact_and_case_sensitive() {
    let config = effective(&["https://App.Example.com"]);
    assert!(config.is_origin_allowed("https://App.Example.com"));
    assert!(!config.is_origin_allowed("https://app.example.com"));
    assert!(!config.is_origin_allowed("http://App.Example.com"));
}

#[test]
fn origin_match_is_port_and_protocol_sensitive() {
    let config = effective(&["https://a.example.com"]);
    assert!(!config.is_origin_allowed("http://a.example.com"));
    assert!(!config.is_origin_allowed("https://a.example.com:8443"));
    assert!(!config.is_origin_allowed("https://a.example.com:443"));
}

#[test]
fn origin_matching_ignores_surrounding_whitespace_only() {
    let config = effective(&["  https://a.example.com  "]);
    assert!(config.is_origin_allowed(" https://a.example.com "));
    assert!(!config.is_origin_allowed("https://a.example.com/x"));
}

#[test]
fn empty_origin_is_never_allowed() {
    let config = effective(&["https://a.example.com"]);
    assert!(!config.is_origin_allowed(""));
    assert!(!config.is_origin_allowed("   "));
}

#[test]
fn wildcard_matches_every_origin() {
    let config = effective(&[WILDCARD_ORIGIN]);
    assert!(config.is_origin_allowed("https://anything.example"));
    assert!(config.is_origin_allowed("http://localhost:3000"));
}

#[test]
fn method_matching_is_case_insensitive() {
    let config = effective(&["https://a.example.com"]);
    assert!(config.is_method_allowed("GET"));
    assert!(config.is_method_allowed(" get "));
    assert!(config.is_method_allowed("post"));
    assert!(!config.is_method_allowed("DELETE"));
    assert!(!config.is_method_allowed(""));
}

#[test]
fn omitted_methods_default_to_get_and_post() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.allowed_methods.clear();
    let config = EffectiveCorsConfig::from(&cors);
    assert!(config.is_method_allowed("GET"));
    assert!(config.is_method_allowed("POST"));
    assert!(!config.is_method_allowed("PUT"));
}

#[test]
fn disabled_configuration_reports_itself_as_disabled() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.enabled = false;
    assert!(!EffectiveCorsConfig::from(&cors).enabled);
}

// ---------------------------------------------------------------------------
// Configuration validation
// ---------------------------------------------------------------------------

#[test]
fn wildcard_with_credentials_is_rejected() {
    let mut cors = cors_config(&[WILDCARD_ORIGIN]);
    cors.allow_credentials = true;
    let error = validate_cors_config(&cors).expect_err("must be rejected");
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn credentials_with_explicit_origins_are_accepted() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.allow_credentials = true;
    assert!(validate_cors_config(&cors).is_ok());
}

// ---------------------------------------------------------------------------
// Preflight detection
// ---------------------------------------------------------------------------

#[test]
fn preflight_detection_requires_method_and_origin() {
    assert!(is_preflight(
        "OPTIONS",
        Some("https://a.example.com"),
        Some("GET")
    ));
    assert!(!is_preflight(
        "GET",
        Some("https://a.example.com"),
        Some("GET")
    ));
    assert!(!is_preflight("OPTIONS", None, Some("GET")));
    assert!(!is_preflight(
        "OPTIONS",
        Some("https://a.example.com"),
        None
    ));
    assert!(!is_preflight("OPTIONS", Some("  "), Some("GET")));
}

// ---------------------------------------------------------------------------
// Preflight evaluation
// ---------------------------------------------------------------------------

#[test]
fn preflight_for_allowed_origin_returns_204_and_headers() {
    let response = evaluate_preflight(
        &effective(&["https://a.example.com"]),
        Some("https://a.example.com"),
        Some("POST"),
        Some("x-trace-id, content-type"),
    );
    assert_eq!(response.status, 204);
    assert!(response.is_allowed());
    assert_eq!(
        header_value(&response.headers, ALLOW_ORIGIN_HEADER).as_deref(),
        Some("https://a.example.com")
    );
    assert_eq!(
        header_value(&response.headers, ALLOW_METHODS_HEADER).as_deref(),
        Some("POST")
    );
    assert_eq!(
        header_value(&response.headers, ALLOW_HEADERS_HEADER).as_deref(),
        Some("x-trace-id, content-type")
    );
    assert_eq!(
        header_value(&response.headers, MAX_AGE_HEADER).as_deref(),
        Some(PREFLIGHT_MAX_AGE)
    );
    assert_eq!(
        header_value(&response.headers, VARY_HEADER).as_deref(),
        Some(PREFLIGHT_VARY)
    );
}

#[test]
fn preflight_for_unknown_origin_still_echoes_but_is_not_allowed() {
    let response = evaluate_preflight(
        &effective(&["https://a.example.com"]),
        Some("https://evil.example.com"),
        Some("POST"),
        None,
    );
    // ADR-0004: the preflight is permissive and echoes the request, while the
    // verdict is carried by `allowed` and enforced on the actual request.
    assert_eq!(response.status, 204);
    assert!(!response.is_allowed());
    assert_eq!(
        response.header(ALLOW_ORIGIN_HEADER),
        Some("https://evil.example.com")
    );
}

#[test]
fn preflight_without_origin_is_not_a_cors_request() {
    let response = evaluate_preflight(&effective(&["https://a.example.com"]), None, None, None);
    assert!(!response.is_allowed());
    assert_eq!(header_value(&response.headers, ALLOW_METHODS_HEADER), None);
    assert_eq!(
        header_value(&response.headers, VARY_HEADER).as_deref(),
        Some(PREFLIGHT_VARY)
    );
}

#[test]
fn preflight_vary_names_all_three_request_headers() {
    let response = evaluate_preflight(
        &effective(&["https://a.example.com"]),
        Some("https://a.example.com"),
        Some("DELETE"),
        None,
    );
    let vary = response.header(VARY_HEADER).expect("vary");
    for token in [
        "Origin",
        "Access-Control-Request-Method",
        "Access-Control-Request-Headers",
    ] {
        assert!(vary.contains(token), "vary must contain {token}");
    }
}

#[test]
fn preflight_omitting_the_method_falls_back_to_the_configuration() {
    let response = evaluate_preflight(
        &effective(&["https://a.example.com"]),
        Some("https://a.example.com"),
        None,
        None,
    );
    assert!(response.is_allowed());
    assert_eq!(
        header_value(&response.headers, ALLOW_METHODS_HEADER).as_deref(),
        Some("GET, POST")
    );
}

#[test]
fn preflight_max_age_uses_the_configuration_override() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.max_age = Some(600);
    let response = evaluate_preflight(
        &EffectiveCorsConfig::from(&cors),
        Some("https://a.example.com"),
        Some("GET"),
        None,
    );
    assert_eq!(
        header_value(&response.headers, MAX_AGE_HEADER).as_deref(),
        Some("600")
    );
}

#[test]
fn disabled_cors_answers_no_preflight_grant() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.enabled = false;
    let response = evaluate_preflight(
        &EffectiveCorsConfig::from(&cors),
        Some("https://a.example.com"),
        Some("GET"),
        None,
    );
    assert!(!response.is_allowed());
    assert_eq!(header_value(&response.headers, ALLOW_ORIGIN_HEADER), None);
}

#[test]
fn wildcard_preflight_emits_the_requested_origin() {
    let response = evaluate_preflight(
        &effective(&[WILDCARD_ORIGIN]),
        Some("https://a.example.com"),
        Some("GET"),
        None,
    );
    assert!(response.is_allowed());
    assert_eq!(
        header_value(&response.headers, ALLOW_ORIGIN_HEADER).as_deref(),
        Some("https://a.example.com")
    );
}

#[test]
fn preflight_header_values_are_valid_header_values() {
    let response = evaluate_preflight(
        &effective(&["https://a.example.com"]),
        Some("https://a.example.com"),
        Some("PUT"),
        Some("x-trace-id"),
    );
    for (name, value) in &response.headers {
        assert!(HeaderName::from_bytes(name.as_str().as_bytes()).is_ok());
        assert!(!value.as_bytes().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Actual requests
// ---------------------------------------------------------------------------

#[test]
fn actual_request_from_allowed_origin_is_accepted() {
    let config = effective(&["https://a.example.com"]);
    assert!(validate_actual_request(&config, Some("https://a.example.com"), "GET").is_ok());
    assert!(validate_actual_request(&config, Some("https://a.example.com"), "DELETE").is_err());
}

#[test]
fn actual_request_from_unknown_origin_is_rejected_with_403() {
    let error = validate_actual_request(
        &effective(&["https://a.example.com"]),
        Some("https://evil.example.com"),
        "GET",
    )
    .expect_err("403");
    assert_eq!(error.status(), StatusCode::FORBIDDEN);
    assert_eq!(error.problem_body().r#type, CORS_ORIGIN_NOT_ALLOWED_TYPE);
    assert_eq!(
        error.problem_body().context.invalid_value.as_deref(),
        Some("https://evil.example.com")
    );
}

#[test]
fn actual_request_with_disallowed_method_is_rejected_with_403() {
    let error = validate_actual_request(
        &effective(&["https://a.example.com"]),
        Some("https://a.example.com"),
        "TRACE",
    )
    .expect_err("403");
    assert_eq!(error.problem_body().r#type, CORS_METHOD_NOT_ALLOWED_TYPE);
    assert_eq!(
        error.problem_body().context.invalid_value,
        Some("TRACE".to_owned())
    );
}

#[test]
fn actual_request_without_origin_is_not_a_cors_request() {
    let config = effective(&["https://a.example.com"]);
    assert!(validate_actual_request(&config, None, "DELETE").is_ok());
    assert!(validate_actual_request(&config, Some("  "), "DELETE").is_ok());
}

#[test]
fn disabled_cors_never_rejects_an_actual_request() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.enabled = false;
    let config = EffectiveCorsConfig::from(&cors);
    assert!(validate_actual_request(&config, Some("https://evil.example.com"), "GET").is_ok());
}

#[test]
fn actual_response_headers_expose_the_configured_list() {
    let headers = super::actual_response_headers(
        &effective(&["https://a.example.com"]),
        Some("https://a.example.com"),
    );
    assert_eq!(
        header_value(&headers, ALLOW_ORIGIN_HEADER).as_deref(),
        Some("https://a.example.com")
    );
    assert_eq!(
        header_value(&headers, EXPOSE_HEADERS_HEADER).as_deref(),
        Some("x-request-id")
    );
    assert_eq!(
        header_value(&headers, VARY_HEADER).as_deref(),
        Some(ACTUAL_VARY)
    );
}

#[test]
fn wildcard_actual_response_emits_the_star() {
    let headers = super::actual_response_headers(
        &effective(&[WILDCARD_ORIGIN]),
        Some("https://a.example.com"),
    );
    assert_eq!(
        header_value(&headers, ALLOW_ORIGIN_HEADER).as_deref(),
        Some(WILDCARD_ORIGIN)
    );
    assert_eq!(header_value(&headers, ALLOW_CREDENTIALS_HEADER), None);
}

#[test]
fn actual_response_headers_are_empty_when_disabled() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.enabled = false;
    let headers = super::actual_response_headers(
        &EffectiveCorsConfig::from(&cors),
        Some("https://a.example.com"),
    );
    assert!(headers.is_empty());
}

#[test]
fn actual_response_headers_are_empty_for_unknown_origins() {
    let headers = super::actual_response_headers(
        &effective(&["https://a.example.com"]),
        Some("https://evil.example.com"),
    );
    assert!(headers.is_empty());
}

#[test]
fn credentials_actual_response_emits_the_allow_credentials_header() {
    let mut cors = cors_config(&["https://a.example.com"]);
    cors.allow_credentials = true;
    let headers = super::actual_response_headers(
        &EffectiveCorsConfig::from(&cors),
        Some("https://a.example.com"),
    );
    assert_eq!(
        header_value(&headers, ALLOW_CREDENTIALS_HEADER).as_deref(),
        Some("true")
    );
}

// ---------------------------------------------------------------------------
// Inheritance
// ---------------------------------------------------------------------------

#[test]
fn private_parent_is_replaced_by_the_child() {
    let mut parent = cors_config(&["https://parent.example.com"]);
    parent.sharing = SharingMode::Private;
    let child = cors_config(&["https://child.example.com"]);
    let merged = merge_cors(&[&parent, &child]).expect("effective");
    assert_eq!(merged.allowed_origins, vec!["https://child.example.com"]);
}

#[test]
fn inherit_unions_parent_and_child_origins() {
    let mut parent = cors_config(&["https://parent.example.com"]);
    parent.sharing = SharingMode::Inherit;
    let child = cors_config(&["https://child.example.com"]);
    let merged = merge_cors(&[&parent, &child]).expect("effective");
    assert_eq!(
        merged.allowed_origins,
        vec!["https://parent.example.com", "https://child.example.com"]
    );
    assert_eq!(merged.allow_headers, vec!["x-trace-id".to_owned()]);
}

#[test]
fn enforce_keeps_the_parent_set() {
    let mut parent = cors_config(&["https://parent.example.com"]);
    parent.sharing = SharingMode::Enforce;
    let child = cors_config(&["https://child.example.com"]);
    let merged = merge_cors(&[&parent, &child]).expect("effective");
    assert_eq!(merged.allowed_origins, vec!["https://parent.example.com"]);
}

#[test]
fn disabled_entries_are_skipped() {
    let mut parent = cors_config(&["https://parent.example.com"]);
    parent.sharing = SharingMode::Inherit;
    parent.enabled = false;
    let child = cors_config(&["https://child.example.com"]);
    let merged = merge_cors(&[&parent, &child]).expect("effective");
    assert_eq!(merged.allowed_origins, vec!["https://child.example.com"]);
}

#[test]
fn empty_chain_yields_none() {
    let chain: Vec<&CorsConfig> = Vec::new();
    assert!(merge_cors(&chain).is_none());
}

#[test]
fn inherited_chain_survives_three_levels() {
    let mut root = cors_config(&["https://root.example.com"]);
    root.sharing = SharingMode::Inherit;
    let mut middle = cors_config(&["https://middle.example.com"]);
    middle.sharing = SharingMode::Inherit;
    let leaf = cors_config(&["https://leaf.example.com"]);
    let merged = merge_cors(&[&root, &middle, &leaf]).expect("effective");
    assert_eq!(
        merged.allowed_origins,
        vec![
            "https://root.example.com",
            "https://middle.example.com",
            "https://leaf.example.com"
        ]
    );
}

/// A merged wildcard must never be paired with a credential grant (ADR-0004
/// "Security defaults"): the upstream wildcard and the route credentials are
/// each valid on their own, but the union of the two is the combination the
/// write-time validation forbids.
#[test]
fn a_merged_wildcard_never_carries_credentials() {
    let mut upstream = cors_config(&[WILDCARD_ORIGIN]);
    upstream.sharing = SharingMode::Inherit;
    let mut route = cors_config(&["https://app.example.com"]);
    route.allow_credentials = true;

    let merged = merge_cors(&[&upstream, &route]).expect("effective");
    assert!(
        merged.is_wildcard(),
        "the union keeps the wildcard: {:?}",
        merged.allowed_origins
    );
    assert!(
        !merged.allow_credentials,
        "the wildcard drops the credential grant"
    );

    // An attacker-chosen origin: the answer may not name it *and* grant
    // credentials for it.
    let attacker = "https://attacker.example";
    let headers = actual_response_headers(&merged, Some(attacker));
    assert!(
        header_value(&headers, ALLOW_CREDENTIALS_HEADER).is_none(),
        "no merged answer may grant credentials: {headers:?}"
    );
    assert_eq!(
        header_value(&headers, ALLOW_ORIGIN_HEADER).as_deref(),
        Some(WILDCARD_ORIGIN),
        "the wildcard answers with the literal, never with the echoed origin"
    );

    let preflight = evaluate_preflight(&merged, Some(attacker), Some("POST"), None);
    assert!(preflight.header(ALLOW_CREDENTIALS_HEADER).is_none());
}

/// The reversed merge — credentials on the parent, wildcard on the child — is
/// equally forbidden, so the route's origins keep working without credentials.
#[test]
fn a_merged_wildcard_from_the_child_drops_the_inherited_credentials() {
    let mut upstream = cors_config(&["https://app.example.com"]);
    upstream.sharing = SharingMode::Inherit;
    upstream.allow_credentials = true;
    let route = cors_config(&[WILDCARD_ORIGIN]);

    let merged = merge_cors(&[&upstream, &route]).expect("effective");
    assert!(merged.is_wildcard());
    assert!(!merged.allow_credentials);
    let headers = actual_response_headers(&merged, Some("https://app.example.com"));
    assert!(header_value(&headers, ALLOW_CREDENTIALS_HEADER).is_none());
}

// ---------------------------------------------------------------------------
// Header plumbing
// ---------------------------------------------------------------------------
#[test]
fn header_names_are_usable_as_axum_names() {
    for name in [
        ALLOW_ORIGIN_HEADER,
        ALLOW_METHODS_HEADER,
        ALLOW_HEADERS_HEADER,
        ALLOW_CREDENTIALS_HEADER,
        EXPOSE_HEADERS_HEADER,
        MAX_AGE_HEADER,
        VARY_HEADER,
        ORIGIN_HEADER,
        REQUEST_METHOD_HEADER,
        REQUEST_HEADERS_HEADER,
    ] {
        assert!(
            HeaderName::from_bytes(name.as_bytes()).is_ok(),
            "header name '{name}' must be valid"
        );
    }
}

#[test]
fn effective_config_defaults_max_age_to_the_adr_constant() {
    assert_eq!(
        effective(&["https://a.example.com"]).max_age(),
        PREFLIGHT_MAX_AGE
    );
}

#[test]
fn preflight_response_is_debug_and_clone() {
    let response = PreflightResponse {
        status: 204,
        headers: Vec::new(),
        allowed: false,
    };
    assert_eq!(response.clone(), response);
    assert!(format!("{response:?}").contains("204"));
}
