//! Tests for the built-in `required_headers` guard (`docs/ADR/0009`).
use serde_json::json;
use uuid::Uuid;

use super::RequiredHeadersGuardPlugin;
use crate::domain::plugin::{GuardDecision, GuardPlugin, RequestContext, ResponseContext};

const GUARD: RequiredHeadersGuardPlugin = RequiredHeadersGuardPlugin;

fn request(config: serde_json::Value) -> RequestContext {
    let mut ctx = RequestContext::new("GET", "/v1", None, Uuid::new_v4(), Uuid::new_v4());
    ctx.config = config;
    ctx
}

fn response(config: serde_json::Value) -> ResponseContext {
    let mut ctx = ResponseContext::new(200);
    ctx.config = config;
    ctx
}

fn decision(
    result: Result<GuardDecision, crate::domain::plugin::PluginError>,
) -> (bool, Option<u16>, String) {
    match result {
        Ok(GuardDecision::Allow) => (true, None, String::new()),
        Ok(GuardDecision::Reject { status, detail, .. }) => (false, Some(status), detail),
        Err(error) => (false, Some(error.status), error.detail),
    }
}

#[tokio::test]
async fn a_present_required_request_header_passes() {
    let mut ctx = request(json!({ "required_request_headers": "x-correlation-id" }));
    ctx.set_header("X-Correlation-Id", "corr-1");
    let (allowed, status, detail) = decision(GUARD.guard_request(&ctx).await);
    assert!(allowed, "the header is present: {detail}");
    assert_eq!(status, None);
}

#[tokio::test]
async fn an_absent_required_request_header_is_a_400() {
    let ctx = request(json!({ "required_request_headers": "x-correlation-id" }));
    let (allowed, status, detail) = decision(GUARD.guard_request(&ctx).await);
    assert!(!allowed);
    assert_eq!(status, Some(400));
    assert_eq!(
        detail,
        "required request header 'x-correlation-id' is absent"
    );
}

#[tokio::test]
async fn the_first_absent_header_of_the_list_is_reported() {
    let mut ctx = request(json!({ "required_request_headers": "x-a, x-b ,x-c" }));
    ctx.set_header("x-b", "1");
    let (allowed, status, detail) = decision(GUARD.guard_request(&ctx).await);
    assert!(!allowed);
    assert_eq!(status, Some(400));
    assert!(detail.contains("'x-a'"), "{detail}");
}

#[tokio::test]
async fn a_blank_configuration_is_fail_open() {
    let ctx = request(json!({ "required_request_headers": "  " }));
    let (allowed, status, _) = decision(GUARD.guard_request(&ctx).await);
    assert!(allowed, "a blank list must not wedge the route");
    assert_eq!(status, None);
}

#[tokio::test]
async fn an_absent_configuration_is_fail_open() {
    let ctx = request(json!({}));
    let (allowed, _, _) = decision(GUARD.guard_request(&ctx).await);
    assert!(allowed);
}

#[tokio::test]
async fn an_unparseable_header_name_is_treated_as_satisfied() {
    let ctx = request(json!({ "required_request_headers": "not a header name" }));
    let (allowed, _, detail) = decision(GUARD.guard_request(&ctx).await);
    assert!(
        allowed,
        "a name no client can send must not wedge the route: {detail}"
    );
}

#[tokio::test]
async fn an_absent_required_response_header_is_a_502() {
    let mut ctx = response(json!({ "required_response_headers": "x-request-id" }));
    ctx.headers.insert(
        http::HeaderName::from_static("content-type"),
        http::HeaderValue::from_static("application/json"),
    );
    let (allowed, status, detail) = decision(GUARD.guard_response(&ctx).await);
    assert!(!allowed);
    assert_eq!(status, Some(502));
    assert!(
        detail.contains("missing required header 'x-request-id'"),
        "{detail}"
    );
}

#[tokio::test]
async fn a_present_required_response_header_passes() {
    let mut ctx = response(json!({ "required_response_headers": "x-request-id" }));
    ctx.headers.insert(
        http::HeaderName::from_static("x-request-id"),
        http::HeaderValue::from_static("abc"),
    );
    let (allowed, status, _) = decision(GUARD.guard_response(&ctx).await);
    assert!(allowed);
    assert_eq!(status, None);
}

#[tokio::test]
async fn the_request_and_response_keys_are_independent() {
    // A binding that only configures the response key must not police the
    // request, and the other way round.
    let ctx = request(json!({ "required_response_headers": "x-request-id" }));
    let (allowed, _, _) = decision(GUARD.guard_request(&ctx).await);
    assert!(allowed, "the request phase reads only its own key");

    let ctx = response(json!({ "required_request_headers": "x-correlation-id" }));
    let (allowed, _, _) = decision(GUARD.guard_response(&ctx).await);
    assert!(allowed, "the response phase reads only its own key");
}

#[test]
fn the_plugin_reports_its_identity() {
    assert_eq!(GUARD.id(), "required_headers");
    assert_eq!(
        GUARD.plugin_type(),
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
}
