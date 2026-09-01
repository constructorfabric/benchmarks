//! `RequestIdTransformPlugin` tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;

use serde_json::json;

use crate::domain::plugin::TransformPlugin;
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;

fn context(request_id: Option<&str>) -> crate::domain::dto::ProxyContext {
    crate::domain::dto::ProxyContext {
        alias: "partner-openai".to_owned(),
        method: "GET".to_owned(),
        path: "/".to_owned(),
        query: Vec::new(),
        headers: request_id
            .map(|value| BTreeMap::from([("x-request-id".to_owned(), value.to_owned())]))
            .unwrap_or_default(),
        trace_id: None,
        tenant: uuid::Uuid::nil(),
        subject: uuid::Uuid::nil(),
    }
}

#[tokio::test]
async fn an_inbound_request_id_is_forwarded_untouched() {
    let plugin = RequestIdTransformPlugin::new(None).unwrap();
    let mut request = context(Some("caller-id-1"));
    plugin.transform_request(&mut request).await.unwrap();
    assert_eq!(
        request.headers.get("x-request-id").map(String::as_str),
        Some("caller-id-1")
    );
}

#[tokio::test]
async fn a_missing_request_id_is_minted() {
    let plugin = RequestIdTransformPlugin::new(None).unwrap();
    let mut request = context(None);
    plugin.transform_request(&mut request).await.unwrap();
    let stamped = request.headers.get("x-request-id").cloned().unwrap();
    assert!(
        uuid::Uuid::parse_str(&stamped).is_ok(),
        "{stamped} is not a UUID"
    );
}

#[tokio::test]
async fn a_blank_inbound_request_id_is_replaced() {
    let plugin = RequestIdTransformPlugin::new(None).unwrap();
    let mut request = context(Some("   "));
    plugin.transform_request(&mut request).await.unwrap();
    let stamped = request.headers.get("x-request-id").cloned().unwrap();
    assert!(uuid::Uuid::parse_str(&stamped).is_ok());
}

#[tokio::test]
async fn forwarding_can_be_disabled() {
    let plugin = RequestIdTransformPlugin::new(Some(&json!({"forward_inbound": false}))).unwrap();
    let mut request = context(Some("caller-id-1"));
    plugin.transform_request(&mut request).await.unwrap();
    let stamped = request.headers.get("x-request-id").cloned().unwrap();
    assert_ne!(stamped, "caller-id-1");
}

#[tokio::test]
async fn a_minted_id_becomes_the_trace_id_when_the_request_has_none() {
    let plugin = RequestIdTransformPlugin::new(None).unwrap();
    let mut request = context(None);
    plugin.transform_request(&mut request).await.unwrap();
    assert_eq!(
        request.trace_id.as_deref(),
        request.headers.get("x-request-id").map(String::as_str)
    );
}

#[tokio::test]
async fn an_existing_trace_id_is_left_alone() {
    let plugin = RequestIdTransformPlugin::new(None).unwrap();
    let mut request = context(Some("caller-id-1"));
    request.trace_id = Some("trace-9".to_owned());
    plugin.transform_request(&mut request).await.unwrap();
    assert_eq!(request.trace_id.as_deref(), Some("trace-9"));
}

#[test]
fn a_non_boolean_forward_inbound_is_refused() {
    let error =
        RequestIdTransformPlugin::new(Some(&json!({"forward_inbound": "yes"}))).unwrap_err();
    assert!(matches!(
        error,
        crate::domain::error::DomainError::Validation { .. }
    ));
}

#[test]
fn the_transform_reports_the_documented_gts_id() {
    let plugin = RequestIdTransformPlugin::new(None).unwrap();
    assert_eq!(
        plugin.gts_id(),
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );
}
