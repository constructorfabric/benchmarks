#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the `request_id` transform plugin (ADR-0009).

use bytes::Bytes;
use serde_json::Value;
use uuid::Uuid;

use super::{REQUEST_ID_HEADER, RequestIdTransformPlugin};
use crate::domain::gts_helpers::TRANSFORM_REQUEST_ID;
use crate::domain::plugin::{ProxyRequest, ProxyResponse, TransformPlugin};

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
async fn a_caller_supplied_request_id_is_propagated_untouched() {
    let transform = RequestIdTransformPlugin;
    let mut request = request(&[("x-request-id", "from-the-caller")]);
    transform.transform_request(&mut request, &Value::Null).await.unwrap();
    assert_eq!(request.header(REQUEST_ID_HEADER).as_deref(), Some("from-the-caller"));
}

#[tokio::test]
async fn a_missing_request_id_is_generated() {
    let transform = RequestIdTransformPlugin;
    let mut request = request(&[]);
    transform.transform_request(&mut request, &Value::Null).await.unwrap();
    let generated = request.header(REQUEST_ID_HEADER).unwrap();
    assert_eq!(generated.len(), 32, "a uuid in simple form: {generated}");
    assert!(generated.chars().all(|c| c.is_ascii_hexdigit()), "{generated}");
}

#[tokio::test]
async fn two_generated_ids_differ() {
    let transform = RequestIdTransformPlugin;
    let mut first = request(&[]);
    let mut second = request(&[]);
    transform.transform_request(&mut first, &Value::Null).await.unwrap();
    transform.transform_request(&mut second, &Value::Null).await.unwrap();
    assert_ne!(first.header(REQUEST_ID_HEADER), second.header(REQUEST_ID_HEADER));
}

#[tokio::test]
async fn the_response_gets_an_id_only_when_the_upstream_sent_none() {
    let transform = RequestIdTransformPlugin;
    let mut without = response(&[]);
    transform.transform_response(&mut without, &Value::Null).await.unwrap();
    assert!(without.headers.get(REQUEST_ID_HEADER).is_some(), "an id must be added");

    let mut with = response(&[("x-request-id", "upstream-id")]);
    transform.transform_response(&mut with, &Value::Null).await.unwrap();
    assert_eq!(
        with.headers.get(REQUEST_ID_HEADER).map(|v| v.to_str().unwrap()),
        Some("upstream-id"),
        "the upstream id must win"
    );
}

#[tokio::test]
async fn the_error_phase_is_a_no_op() {
    let transform = RequestIdTransformPlugin;
    let mut detail = String::from("upstream exploded");
    transform.transform_error(&mut detail, &Value::Null).await.unwrap();
    assert_eq!(detail, "upstream exploded", "the detail must not be rewritten");
}

#[tokio::test]
async fn any_configuration_is_accepted() {
    let transform = RequestIdTransformPlugin;
    for config in [Value::Null, serde_json::json!({"whatever": 1})] {
        let mut request = request(&[]);
        transform.transform_request(&mut request, &config).await.unwrap();
        assert!(request.header(REQUEST_ID_HEADER).is_some());
    }
}

#[test]
fn it_advertises_the_catalogued_identifier() {
    assert_eq!(RequestIdTransformPlugin.id(), TRANSFORM_REQUEST_ID);
    assert_eq!(RequestIdTransformPlugin.plugin_type(), "request_id");
}

#[test]
fn the_plugin_is_stateless_so_the_same_instance_serves_every_call() {
    assert_eq!(format!("{:?}", RequestIdTransformPlugin), "RequestIdTransformPlugin");
}
