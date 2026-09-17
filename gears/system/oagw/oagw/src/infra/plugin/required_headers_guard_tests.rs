use std::collections::BTreeMap;

use axum::http::{HeaderMap, HeaderValue};
use bytes::Bytes;
use uuid::Uuid;

use super::{REQUEST_KEY, RESPONSE_KEY, RequiredHeadersGuardPlugin, configured};
use crate::domain::plugin::{Caller, GuardDecision, GuardPlugin, RequestContext, ResponseContext};
use toolkit_security::SecurityContext;

fn caller() -> Caller {
    let context = SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"));
    Caller::from_context(&context)
}

fn headers_with(entries: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(*name),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

fn request(config: serde_json::Value, headers: HeaderMap) -> RequestContext {
    RequestContext {
        caller: caller(),
        config,
        method: "GET".to_owned(),
        path: "/things".to_owned(),
        query: Vec::new(),
        headers,
        body: Bytes::new(),
        attributes: BTreeMap::new(),
    }
}

fn response(config: serde_json::Value, headers: HeaderMap) -> ResponseContext {
    ResponseContext {
        caller: caller(),
        config,
        status: 200,
        headers,
        body: Bytes::new(),
        attributes: BTreeMap::new(),
    }
}

#[test]
fn configured_splits_and_normalizes() {
    let config = serde_json::json!({ REQUEST_KEY: " X-Trace-Id ,, X-Request-Id " });
    assert_eq!(
        configured(&config, REQUEST_KEY),
        vec!["x-trace-id".to_owned(), "x-request-id".to_owned()]
    );
}

#[test]
fn configured_is_fail_open() {
    assert!(configured(&serde_json::Value::Null, REQUEST_KEY).is_empty());
    assert!(configured(&serde_json::json!({ REQUEST_KEY: " , " }), REQUEST_KEY).is_empty());
    assert!(configured(&serde_json::json!({ REQUEST_KEY: 7 }), REQUEST_KEY).is_empty());
}

#[tokio::test]
async fn request_without_requirements_is_allowed() {
    let plugin = RequiredHeadersGuardPlugin;
    let decision = plugin
        .guard_request(&request(serde_json::Value::Null, headers_with(&[])))
        .await
        .unwrap_or_else(|error| panic!("guard_request: {error}"));
    assert_eq!(decision, GuardDecision::allow());
}

#[tokio::test]
async fn missing_request_header_is_rejected_with_400() {
    let plugin = RequiredHeadersGuardPlugin;
    let config = serde_json::json!({ REQUEST_KEY: "x-trace-id" });
    let decision = plugin
        .guard_request(&request(config, headers_with(&[])))
        .await
        .unwrap_or_else(|error| panic!("guard_request: {error}"));
    assert!(!decision.allowed);
    assert_eq!(decision.status, 400);
    assert_eq!(decision.error_code, "REQUIRED_HEADER_MISSING");
}

#[tokio::test]
async fn present_request_header_is_allowed() {
    let plugin = RequiredHeadersGuardPlugin;
    let config = serde_json::json!({ REQUEST_KEY: "x-trace-id" });
    let headers = headers_with(&[("X-Trace-Id", "trace")]);
    let decision = plugin
        .guard_request(&request(config, headers))
        .await
        .unwrap_or_else(|error| panic!("guard_request: {error}"));
    assert!(decision.allowed);
}

#[tokio::test]
async fn missing_response_header_is_rejected_with_502() {
    let plugin = RequiredHeadersGuardPlugin;
    let config = serde_json::json!({ RESPONSE_KEY: "x-upstream-fingerprint" });
    let decision = plugin
        .guard_response(&response(config, headers_with(&[])))
        .await
        .unwrap_or_else(|error| panic!("guard_response: {error}"));
    assert!(!decision.allowed);
    assert_eq!(decision.status, 502);
    assert_eq!(decision.error_code, "REQUIRED_HEADER_MISSING");
}

#[tokio::test]
async fn response_requirements_are_keyed_separately() {
    let plugin = RequiredHeadersGuardPlugin;
    let config = serde_json::json!({ REQUEST_KEY: "x-request" });
    let decision = plugin
        .guard_response(&response(config, headers_with(&[])))
        .await
        .unwrap_or_else(|error| panic!("guard_response: {error}"));
    assert!(decision.allowed);
}
