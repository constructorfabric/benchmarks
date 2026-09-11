//! Unit tests for the required-headers guard plugin (ADR 0009).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    MISSING_HEADER_CODE, REQUIRED_REQUEST_HEADERS, REQUIRED_RESPONSE_HEADERS,
    RequiredHeadersGuardPlugin,
};
use crate::domain::plugin::test_support::context;
use crate::domain::plugin::{GuardPlugin, PluginResponseContext};
use crate::gts_helpers;
use http::HeaderValue;

fn config(entries: &[(&str, &str)]) -> serde_json::Value {
    serde_json::Value::Object(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), serde_json::Value::from(*value)))
            .collect(),
    )
}

fn response_context() -> PluginResponseContext {
    PluginResponseContext {
        request_id: "abc-123".to_owned(),
        status: 200,
        headers: http::HeaderMap::new(),
    }
}

#[tokio::test]
async fn allows_a_request_carrying_every_required_header() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut request = context("GET", "/v1/things");
    request
        .headers
        .insert("x-tenant", HeaderValue::from_static("acme"));

    let decision = plugin
        .guard_request(&request, &config(&[(REQUIRED_REQUEST_HEADERS, "x-tenant")]))
        .await
        .unwrap();

    assert_eq!(decision, super::GuardDecision::Allow);
}

#[tokio::test]
async fn rejects_a_request_missing_a_required_header_with_a_400() {
    let plugin = RequiredHeadersGuardPlugin;
    let request = context("GET", "/v1/things");

    let decision = plugin
        .guard_request(
            &request,
            &config(&[(REQUIRED_REQUEST_HEADERS, "x-tenant, x-trace")]),
        )
        .await
        .unwrap();

    let super::GuardDecision::Reject {
        status,
        code,
        detail,
    } = decision
    else {
        panic!("the missing header must reject the request");
    };
    assert_eq!(status, 400);
    assert_eq!(code, MISSING_HEADER_CODE);
    assert!(detail.contains("x-tenant"), "names the missing header");
}

#[tokio::test]
async fn a_missing_response_header_is_a_502() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut response = response_context();
    response
        .headers
        .insert("x-request-id", HeaderValue::from_static("abc-123"));

    let decision = plugin
        .guard_response(
            &response,
            &config(&[(REQUIRED_RESPONSE_HEADERS, "x-request-id, x-upstream-node")]),
        )
        .await
        .unwrap();

    let super::GuardDecision::Reject { status, .. } = decision else {
        panic!("the missing response header must reject the response");
    };
    assert_eq!(status, 502);
}

#[tokio::test]
async fn an_unconfigured_guard_fails_open() {
    let plugin = RequiredHeadersGuardPlugin;
    let request = context("GET", "/v1/things");

    assert_eq!(
        plugin.guard_request(&request, &config(&[])).await.unwrap(),
        super::GuardDecision::Allow
    );
    assert_eq!(
        plugin
            .guard_response(&response_context(), &serde_json::Value::Null)
            .await
            .unwrap(),
        super::GuardDecision::Allow
    );
}

#[test]
fn the_identifier_is_the_built_in_required_headers_guard() {
    assert_eq!(
        RequiredHeadersGuardPlugin.id(),
        gts_helpers::GUARD_REQUIRED_HEADERS
    );
    assert!(!crate::domain::plugin::registry::is_catalogue_only(
        RequiredHeadersGuardPlugin.id()
    ));
}
