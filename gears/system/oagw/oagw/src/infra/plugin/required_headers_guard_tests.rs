//! `RequiredHeadersGuardPlugin` tests (`ADR`-0009).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;

use serde_json::json;

use crate::domain::plugin::{GuardDecision, GuardPlugin, ResponseContext};
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;

fn context(headers: &[(&str, &str)]) -> crate::domain::dto::ProxyContext {
    crate::domain::dto::ProxyContext {
        alias: "partner-openai".to_owned(),
        method: "GET".to_owned(),
        path: "/".to_owned(),
        query: Vec::new(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_ascii_lowercase(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
        trace_id: None,
        tenant: uuid::Uuid::nil(),
        subject: uuid::Uuid::nil(),
    }
}

#[tokio::test]
async fn a_request_carrying_every_required_header_is_allowed() {
    let plugin = RequiredHeadersGuardPlugin::new(Some(&json!({
        "required_request_headers": "x-tenant-id, x-request-id"
    })));
    let request = context(&[("x-tenant-id", "t1"), ("X-Request-Id", "abc")]);
    assert_eq!(
        plugin.guard_request(&request).await.unwrap(),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn a_missing_request_header_is_denied_naming_only_the_first() {
    let plugin = RequiredHeadersGuardPlugin::new(Some(&json!({
        "required_request_headers": "x-tenant-id, x-request-id"
    })));
    let request = context(&[]);
    let decision = plugin.guard_request(&request).await.unwrap();
    assert_eq!(
        decision,
        GuardDecision::Deny("required request header 'x-tenant-id' missing".to_owned())
    );
}

#[tokio::test]
async fn a_blank_list_is_fail_open() {
    let plugin = RequiredHeadersGuardPlugin::new(Some(&json!({
        "required_request_headers": "   ",
        "required_response_headers": ""
    })));
    let request = context(&[]);
    assert_eq!(
        plugin.guard_request(&request).await.unwrap(),
        GuardDecision::Allow
    );
    let response = ResponseContext::default();
    assert_eq!(
        plugin.guard_response(&response).await.unwrap(),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn an_absent_config_is_fail_open() {
    let plugin = RequiredHeadersGuardPlugin::new(None);
    let request = context(&[]);
    assert_eq!(
        plugin.guard_request(&request).await.unwrap(),
        GuardDecision::Allow
    );
    assert!(plugin.response_headers().is_empty());
}

#[tokio::test]
async fn a_missing_response_header_is_denied() {
    let plugin = RequiredHeadersGuardPlugin::new(Some(&json!({
        "required_response_headers": "x-request-id, x-upstream-trace"
    })));
    let mut response = ResponseContext {
        status: 200,
        ..Default::default()
    };
    response
        .headers
        .insert("x-request-id".to_owned(), "abc".to_owned());
    let decision = plugin.guard_response(&response).await.unwrap();
    assert_eq!(
        decision,
        GuardDecision::Deny("required response header 'x-upstream-trace' missing".to_owned())
    );
}

#[tokio::test]
async fn presence_is_enough_the_value_is_never_inspected() {
    let plugin = RequiredHeadersGuardPlugin::new(Some(&json!({
        "required_request_headers": "x-tenant-id"
    })));
    let request = context(&[("X-Tenant-Id", "")]);
    assert_eq!(
        plugin.guard_request(&request).await.unwrap(),
        GuardDecision::Allow
    );
}

#[test]
fn duplicate_and_blank_entries_collapse() {
    let plugin = RequiredHeadersGuardPlugin::new(Some(&json!({
        "required_request_headers": "X-A, x-a ,, x-b"
    })));
    assert_eq!(plugin.request_headers(), &["x-a", "x-b"]);
    assert!(plugin.response_headers().is_empty());
}

#[test]
fn the_guard_reports_the_documented_gts_id() {
    let plugin = RequiredHeadersGuardPlugin::new(None);
    assert_eq!(
        plugin.gts_id(),
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
}
