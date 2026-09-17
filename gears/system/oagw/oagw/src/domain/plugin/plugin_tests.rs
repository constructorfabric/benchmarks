use axum::http::HeaderMap;
use bytes::Bytes;
use uuid::Uuid;

use super::{
    Caller, ErrorContext, GuardDecision, PluginError, RequestContext, ResponseContext,
    guard_rejection,
};
use crate::domain::error::ErrorKind;
use toolkit_security::SecurityContext;

fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"))
}

fn request() -> RequestContext {
    RequestContext {
        caller: Caller::from_context(&context()),
        config: serde_json::json!({"header": "x-api-key"}),
        method: "GET".to_owned(),
        path: "/things".to_owned(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Bytes::new(),
        attributes: std::collections::BTreeMap::new(),
    }
}

#[test]
fn caller_carries_the_subject_identity() {
    let caller = Caller::from_context(&context());
    assert_eq!(caller.tenant_id, Uuid::from_u128(2));
    assert_eq!(caller.subject_id, Uuid::from_u128(1));
}

#[test]
fn request_context_reads_headers_case_insensitively() {
    let mut request = request();
    request.headers.insert(
        axum::http::HeaderName::from_static("x-api-key"),
        axum::http::HeaderValue::from_static("secret"),
    );
    assert_eq!(request.header("X-API-KEY"), Some("secret"));
    assert_eq!(request.header("missing"), None);
}

#[test]
fn request_context_reads_config_keys() {
    let request = request();
    assert_eq!(request.config_str("header"), Some("x-api-key"));
    assert_eq!(request.config_str("absent"), None);
}

#[test]
fn allow_decision_has_no_error_surface() {
    let decision = GuardDecision::allow();
    assert!(decision.allowed);
    assert_eq!(decision.error_code, "");
}

#[test]
fn reject_decision_carries_status_and_code() {
    let decision = GuardDecision::reject(400, "REQUIRED_HEADER_MISSING", "header missing");
    assert!(!decision.allowed);
    assert_eq!(decision.status, 400);
    assert_eq!(decision.error_code, "REQUIRED_HEADER_MISSING");
    assert_eq!(decision.detail, "header missing");
}

#[test]
fn plugin_errors_map_onto_the_catalog() {
    assert_eq!(
        PluginError::Config("bad".to_owned()).to_domain_error().kind,
        ErrorKind::Validation
    );
    assert_eq!(
        PluginError::Secret("gone".to_owned())
            .to_domain_error()
            .kind,
        ErrorKind::SecretNotFound
    );
    assert_eq!(
        PluginError::Auth("nope".to_owned()).to_domain_error().kind,
        ErrorKind::AuthenticationFailed
    );
    assert_eq!(
        PluginError::Internal("boom".to_owned())
            .to_domain_error()
            .kind,
        ErrorKind::ProtocolError
    );
}

#[test]
fn plugin_errors_render_as_their_detail() {
    assert_eq!(PluginError::Auth("nope".to_owned()).to_string(), "nope");
}

#[test]
fn guard_rejection_maps_status_to_the_catalog() {
    let missing = guard_rejection(&GuardDecision::reject(
        400,
        "REQUIRED_HEADER_MISSING",
        "header missing",
    ));
    assert_eq!(missing.kind, ErrorKind::Validation);
    assert_eq!(
        missing.extensions.invalid_value.as_deref(),
        Some("REQUIRED_HEADER_MISSING")
    );

    let upstream = guard_rejection(&GuardDecision::reject(
        502,
        "REQUIRED_HEADER_MISSING",
        "response header missing",
    ));
    assert_eq!(upstream.kind, ErrorKind::ProtocolError);
}

#[test]
fn error_context_keeps_the_rendered_body() {
    let context = ErrorContext {
        caller: Caller::from_context(&context()),
        config: serde_json::Value::Null,
        status: 502,
        body: Bytes::from_static(b"{\"type\":\"about:blank\"}"),
        attributes: std::collections::BTreeMap::new(),
    };
    assert_eq!(context.status, 502);
    assert_eq!(
        context.body,
        Bytes::from_static(b"{\"type\":\"about:blank\"}")
    );
}

#[test]
fn response_context_carries_status_and_body() {
    let context = ResponseContext {
        caller: Caller::from_context(&context()),
        config: serde_json::Value::Null,
        status: 201,
        headers: HeaderMap::new(),
        body: Bytes::from_static(b"{}"),
        attributes: std::collections::BTreeMap::new(),
    };
    assert_eq!(context.status, 201);
    assert_eq!(context.body.len(), 2);
}
