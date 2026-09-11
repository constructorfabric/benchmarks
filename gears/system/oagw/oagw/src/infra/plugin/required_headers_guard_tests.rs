#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the `required_headers` guard plugin (ADR-0009).

use bytes::Bytes;
use serde_json::Value;
use uuid::Uuid;

use super::RequiredHeadersGuardPlugin;
use crate::domain::gts_helpers::{GUARD_CORS, GUARD_REQUIRED_HEADERS, GUARD_TIMEOUT};
use crate::domain::plugin::{GuardPlugin, PluginError, ProxyRequest, ProxyResponse};

fn request(headers: &[(&str, &str)]) -> ProxyRequest {
    let mut map = axum::http::HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    ProxyRequest {
        method: axum::http::Method::GET,
        path: "/things".to_string(),
        query: String::new(),
        headers: map,
        body: Bytes::new(),
        tenant_id: Uuid::nil(),
        security: None,
    }
}

fn response(headers: &[(&str, &str)]) -> ProxyResponse {
    let mut map = axum::http::HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    ProxyResponse {
        status: axum::http::StatusCode::OK,
        headers: map,
        body: Bytes::new(),
    }
}

#[tokio::test]
async fn a_present_header_satisfies_the_guard() {
    let guard = RequiredHeadersGuardPlugin;
    let request = request(&[("x-tenant", "acme")]);
    guard
        .guard_request(&request, &serde_json::json!({"required_request_headers": "x-tenant"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn header_names_are_matched_case_insensitively() {
    let guard = RequiredHeadersGuardPlugin;
    let request = request(&[("X-Tenant", "acme")]);
    guard
        .guard_request(&request, &serde_json::json!({"required_request_headers": "X-TENANT"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn the_first_missing_header_is_reported() {
    let guard = RequiredHeadersGuardPlugin;
    let request = request(&[("x-tenant", "acme")]);
    let error = guard
        .guard_request(
            &request,
            &serde_json::json!({"required_request_headers": "x-tenant,x-signature,x-nonce"}),
        )
        .await
        .unwrap_err();
    match error {
        PluginError::Guard { code, message } => {
            assert_eq!(code, "REQUIRED_HEADER_MISSING");
            assert!(message.contains("x-signature"), "{message}");
            assert!(!message.contains("x-tenant"), "present headers must not be named: {message}");
        }
        other => panic!("expected Guard, got {other:?}"),
    }
}

#[tokio::test]
async fn blank_and_absent_configuration_fails_open() {
    let guard = RequiredHeadersGuardPlugin;
    for config in [Value::Null, serde_json::json!({}), serde_json::json!({"required_request_headers": "  "})] {
        let request = request(&[]);
        guard.guard_request(&request, &config).await.unwrap();
    }
}

#[tokio::test]
async fn the_response_phase_checks_the_upstream_headers() {
    let guard = RequiredHeadersGuardPlugin;
    let config = serde_json::json!({"required_response_headers": "x-request-id"});
    guard.guard_response(&response(&[("x-request-id", "abc")]), &config).await.unwrap();
    let error = guard.guard_response(&response(&[]), &config).await.unwrap_err();
    match error {
        PluginError::ResponseGuard { code, message } => {
            assert_eq!(code, "REQUIRED_HEADER_MISSING");
            assert!(message.contains("x-request-id"), "{message}");
        }
        other => panic!("expected ResponseGuard, got {other:?}"),
    }
}

#[test]
fn it_advertises_the_catalogued_identifier() {
    assert_eq!(RequiredHeadersGuardPlugin.id(), GUARD_REQUIRED_HEADERS);
    assert_eq!(RequiredHeadersGuardPlugin.plugin_type(), "required_headers");
    assert_ne!(RequiredHeadersGuardPlugin.id(), GUARD_TIMEOUT);
    assert_ne!(RequiredHeadersGuardPlugin.id(), GUARD_CORS);
}
