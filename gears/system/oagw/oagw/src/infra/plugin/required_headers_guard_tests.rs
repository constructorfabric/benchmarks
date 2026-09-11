//! Presence enforcement, including its fail-open posture (ADR 0009).

use super::*;
use crate::domain::model::ConfigMap;
use crate::domain::plugin::{PluginScope, ProxyRequest, ProxyResponseHead};
use bytes::Bytes;
use http::{HeaderValue, Method};
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn config(pairs: &[(&str, &str)]) -> ConfigMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), serde_json::Value::from(*v)))
        .collect()
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            http::HeaderName::try_from(*name).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

fn scope(security: &SecurityContext) -> PluginScope<'_> {
    PluginScope {
        security_context: security,
        alias: "api.example.com",
        upstream_id: Uuid::nil(),
        route_id: None,
    }
}

async fn guard_request(config: &ConfigMap, headers: HeaderMap) -> GuardDecision {
    let security = SecurityContext::anonymous();
    let mut request = ProxyRequest {
        method: Method::GET,
        path: "/".to_owned(),
        query: Vec::new(),
        headers,
        body: Bytes::new(),
    };
    let ctx = RequestContext {
        scope: scope(&security),
        config,
        request: &mut request,
    };
    RequiredHeadersGuardPlugin
        .guard_request(&ctx)
        .await
        .unwrap()
}

async fn guard_response(config: &ConfigMap, headers: HeaderMap) -> GuardDecision {
    let security = SecurityContext::anonymous();
    let mut response = ProxyResponseHead {
        status: StatusCode::OK,
        headers,
    };
    let ctx = ResponseContext {
        scope: scope(&security),
        config,
        response: &mut response,
    };
    RequiredHeadersGuardPlugin
        .guard_response(&ctx)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_unconfigured_plugin_is_a_no_op_on_both_phases() {
    let empty = ConfigMap::new();
    assert_eq!(guard_request(&empty, HeaderMap::new()).await, GuardDecision::Allow);
    assert_eq!(guard_response(&empty, HeaderMap::new()).await, GuardDecision::Allow);
}

#[tokio::test]
async fn a_blank_list_is_also_a_no_op() {
    let config = config(&[("required_request_headers", ", , ,")]);
    assert_eq!(guard_request(&config, HeaderMap::new()).await, GuardDecision::Allow);
}

#[tokio::test]
async fn the_request_phase_rejects_with_400_on_the_first_missing_header() {
    let config = config(&[("required_request_headers", "x-correlation-id, accept")]);
    let decision = guard_request(&config, headers(&[("accept", "application/json")])).await;
    match decision {
        GuardDecision::Reject {
            status,
            error_code,
            message,
        } => {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error_code, REQUIRED_HEADER_MISSING);
            assert!(message.contains("x-correlation-id"), "{message}");
            assert!(!message.contains("accept"), "only the first is reported: {message}");
        }
        GuardDecision::Allow => panic!("expected a rejection"),
    }
}

#[tokio::test]
async fn header_names_are_matched_case_insensitively() {
    let config = config(&[("required_request_headers", "X-Correlation-ID")]);
    let decision = guard_request(&config, headers(&[("x-correlation-id", "abc")])).await;
    assert_eq!(decision, GuardDecision::Allow);
}

#[tokio::test]
async fn only_presence_is_checked_not_the_value() {
    let config = config(&[("required_request_headers", "x-correlation-id")]);
    let decision = guard_request(&config, headers(&[("x-correlation-id", "")])).await;
    assert_eq!(decision, GuardDecision::Allow);
}

#[tokio::test]
async fn the_response_phase_rejects_with_502() {
    let config = config(&[("required_response_headers", "content-type")]);
    let decision = guard_response(&config, HeaderMap::new()).await;
    match decision {
        GuardDecision::Reject { status, .. } => assert_eq!(status, StatusCode::BAD_GATEWAY),
        GuardDecision::Allow => panic!("expected a rejection"),
    }
}

#[tokio::test]
async fn the_two_phases_are_configured_independently() {
    // A request-phase config must not silently enforce anything on responses.
    let config = config(&[("required_request_headers", "x-correlation-id")]);
    assert_eq!(guard_response(&config, HeaderMap::new()).await, GuardDecision::Allow);
}

#[test]
fn the_plugin_reports_its_catalogued_identity() {
    assert_eq!(RequiredHeadersGuardPlugin.id(), "required_headers");
    assert_eq!(
        RequiredHeadersGuardPlugin.plugin_type(),
        gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID
    );
}
