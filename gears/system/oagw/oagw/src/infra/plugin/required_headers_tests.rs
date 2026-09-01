//! Tests for [`crate::infra::plugin::required_headers`].

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use uuid::Uuid;

use super::{
    REQUEST_REJECT_STATUS, REQUIRED_HEADER_MISSING, RESPONSE_REJECT_STATUS,
    RequiredHeadersGuardPlugin,
};
use crate::domain::plugin::{
    GUARD_PLUGIN_TYPE_ID, GuardDecision, GuardPlugin, PluginChain, PluginTier, RequestContext,
    ResponseContext, builtin,
};
use crate::infra::plugin::parse_header_names;

#[test]
fn parse_header_names_normalises_a_comma_separated_list() {
    assert_eq!(
        parse_header_names(" X-Trace-ID , , x-api-key,,"),
        vec!["x-trace-id".to_owned(), "x-api-key".to_owned()]
    );
    assert_eq!(parse_header_names("   "), Vec::<String>::new());
    assert_eq!(parse_header_names(""), Vec::<String>::new());
    assert_eq!(
        parse_header_names("X-Request-ID,CONTENT-TYPE"),
        vec!["x-request-id".to_owned(), "content-type".to_owned()]
    );
}

fn plugin(config: serde_json::Value) -> RequiredHeadersGuardPlugin {
    RequiredHeadersGuardPlugin::new(&config)
}

fn request(required: &'static [&'static str]) -> RequestContext {
    let mut headers = HeaderMap::new();
    for name in required {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static("present"),
        );
    }
    RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(Uuid::from_u128(0x11))
        .headers(headers)
        .build()
}

fn response(required: &'static [&'static str]) -> ResponseContext {
    let mut headers = HeaderMap::new();
    for name in required {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static("present"),
        );
    }
    ResponseContext::builder()
        .status(StatusCode::OK)
        .headers(headers)
        .build()
}

#[test]
fn plugin_declares_the_adr_ids() {
    let built = plugin(serde_json::json!({}));
    assert_eq!(built.id(), builtin::REQUIRED_HEADERS_GUARD);
    assert_eq!(built.plugin_type(), GUARD_PLUGIN_TYPE_ID);
    assert_eq!(
        RequiredHeadersGuardPlugin::PLUGIN_ID,
        builtin::REQUIRED_HEADERS_GUARD
    );
}

#[test]
fn request_headers_are_normalised_from_the_configuration() {
    let built = plugin(serde_json::json!({
        "required_request_headers": " X-Trace-ID ,, x-api-key"
    }));
    assert_eq!(
        built.required_request_headers(),
        &["x-trace-id".to_owned(), "x-api-key".to_owned()]
    );
    assert!(built.required_response_headers().is_empty());
}

#[test]
fn response_headers_are_normalised_from_the_configuration() {
    let built = plugin(serde_json::json!({
        "required_response_headers": "X-Request-ID"
    }));
    assert_eq!(
        built.required_response_headers(),
        &["x-request-id".to_owned()]
    );
    assert!(built.required_request_headers().is_empty());
}

#[test]
fn a_blank_configuration_is_a_no_op() {
    let built = plugin(serde_json::json!({}));
    assert!(built.required_request_headers().is_empty());
    assert!(built.required_response_headers().is_empty());
}

#[test]
fn a_non_string_configuration_is_ignored() {
    let built = plugin(serde_json::json!({ "required_request_headers": 42 }));
    assert!(built.required_request_headers().is_empty());
}

#[test]
fn a_broken_configuration_degrades_to_a_no_op() {
    let built = plugin(serde_json::json!(["not", "an", "object"]));
    assert!(built.required_request_headers().is_empty());
    assert!(built.required_response_headers().is_empty());
}

#[test]
fn the_adr_pins_the_two_rejection_statuses() {
    assert_eq!(REQUEST_REJECT_STATUS, 400);
    assert_eq!(RESPONSE_REJECT_STATUS, 502);
    assert_eq!(REQUIRED_HEADER_MISSING, "REQUIRED_HEADER_MISSING");
}

#[tokio::test]
async fn all_present_request_headers_allow_the_request() {
    let built = plugin(serde_json::json!({
        "required_request_headers": "x-trace-id, x-api-key"
    }));
    let ctx = request(&["x-trace-id", "x-api-key"]);
    assert_eq!(
        built.guard_request(&ctx).await.expect("decision"),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn a_missing_request_header_is_rejected_with_400() {
    let built = plugin(serde_json::json!({
        "required_request_headers": "x-trace-id, x-api-key"
    }));
    let ctx = request(&["x-trace-id"]);
    let decision = built.guard_request(&ctx).await.expect("decision");
    assert_eq!(decision.status(), Some(StatusCode::BAD_REQUEST));
    assert_eq!(decision.error_code(), Some(REQUIRED_HEADER_MISSING));
}

#[tokio::test]
async fn only_the_first_missing_header_is_reported() {
    let built = plugin(serde_json::json!({
        "required_request_headers": "x-trace-id, x-api-key, x-third"
    }));
    let ctx = request(&[]);
    let detail = built
        .guard_request(&ctx)
        .await
        .expect("decision")
        .into_error()
        .detail()
        .to_owned();
    assert!(detail.contains("x-trace-id"), "detail is '{detail}'");
    assert!(!detail.contains("x-api-key"), "detail is '{detail}'");
    assert!(!detail.contains("x-third"), "detail is '{detail}'");
}

#[tokio::test]
async fn an_absent_request_list_is_a_no_op() {
    let built = plugin(serde_json::json!({
        "required_response_headers": "x-request-id"
    }));
    let ctx = request(&[]);
    assert_eq!(
        built.guard_request(&ctx).await.expect("decision"),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn an_empty_list_value_is_a_no_op() {
    let built = plugin(serde_json::json!({ "required_request_headers": " , , " }));
    let ctx = request(&[]);
    assert_eq!(
        built.guard_request(&ctx).await.expect("decision"),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn a_missing_response_header_is_rejected_with_502() {
    let built = plugin(serde_json::json!({
        "required_response_headers": "x-request-id"
    }));
    let ctx = response(&[]);
    let decision = built.guard_response(&ctx).await.expect("decision");
    assert_eq!(decision.status(), Some(StatusCode::BAD_GATEWAY));
    assert_eq!(decision.error_code(), Some(REQUIRED_HEADER_MISSING));
}

#[tokio::test]
async fn a_present_response_header_allows_the_response() {
    let built = plugin(serde_json::json!({
        "required_response_headers": "x-request-id"
    }));
    let ctx = response(&["x-request-id"]);
    assert_eq!(
        built.guard_response(&ctx).await.expect("decision"),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn an_absent_response_list_is_a_no_op() {
    let built = plugin(serde_json::json!({
        "required_request_headers": "x-trace-id"
    }));
    let ctx = response(&[]);
    assert_eq!(
        built.guard_response(&ctx).await.expect("decision"),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn only_presence_is_checked_never_the_value() {
    let built = plugin(serde_json::json!({ "required_request_headers": "x-trace-id" }));
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-trace-id"),
        HeaderValue::from_static(""),
    );
    let ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(Uuid::from_u128(1))
        .headers(headers)
        .build();
    assert_eq!(
        built.guard_request(&ctx).await.expect("decision"),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn the_chain_runner_maps_a_rejection_onto_the_taxonomy() {
    let mut chain = PluginChain::new();
    chain.push_guard(
        PluginTier::Upstream,
        0,
        "required_headers",
        Arc::new(plugin(serde_json::json!({
            "required_request_headers": "x-trace-id"
        }))),
    );
    let error = chain.guard_request(&request(&[])).await.expect_err("400");
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    assert!(error.detail().contains(REQUIRED_HEADER_MISSING));
}

#[tokio::test]
async fn the_chain_runner_maps_a_response_rejection_onto_502() {
    let mut chain = PluginChain::new();
    chain.push_guard(
        PluginTier::Upstream,
        0,
        "required_headers",
        Arc::new(plugin(serde_json::json!({
            "required_response_headers": "x-request-id"
        }))),
    );
    let error = chain.guard_response(&response(&[])).await.expect_err("502");
    assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
}
