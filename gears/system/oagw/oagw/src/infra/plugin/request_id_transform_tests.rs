use std::collections::BTreeMap;

use axum::http::HeaderMap;
use bytes::Bytes;
use uuid::Uuid;

use super::{REQUEST_ID_TRANSFORM_HEADER, RequestIdTransformPlugin};
use crate::domain::plugin::{
    Caller, ErrorContext, RequestContext, ResponseContext, TransformPlugin,
};
use toolkit_security::SecurityContext;

fn caller() -> Caller {
    let context = SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"));
    Caller::from_context(&context)
}

fn request() -> RequestContext {
    RequestContext {
        caller: caller(),
        config: serde_json::Value::Null,
        method: "GET".to_owned(),
        path: "/things".to_owned(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Bytes::new(),
        attributes: BTreeMap::new(),
    }
}

fn response() -> ResponseContext {
    ResponseContext {
        caller: caller(),
        config: serde_json::Value::Null,
        status: 200,
        headers: HeaderMap::new(),
        body: Bytes::new(),
        attributes: BTreeMap::new(),
    }
}

fn error() -> ErrorContext {
    ErrorContext {
        caller: caller(),
        config: serde_json::Value::Null,
        status: 502,
        body: Bytes::new(),
        attributes: BTreeMap::new(),
    }
}

#[test]
fn plugin_identifies_itself() {
    let plugin = RequestIdTransformPlugin;
    assert_eq!(plugin.id(), "request_id");
    assert_eq!(plugin.plugin_type(), crate::ids::TRANSFORM_REQUEST_ID);
}

#[tokio::test]
async fn request_id_is_minted_when_absent() {
    let plugin = RequestIdTransformPlugin;
    let mut request = request();
    plugin
        .transform_request(&mut request)
        .await
        .unwrap_or_else(|error| panic!("transform_request: {error}"));
    let minted = request
        .attributes
        .get("request_id")
        .unwrap_or_else(|| panic!("request_id attribute is missing"));
    assert!(minted.starts_with("req_"), "unexpected id: {minted}");
    assert_eq!(
        request
            .headers
            .get(REQUEST_ID_TRANSFORM_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(minted.as_str())
    );
}

#[tokio::test]
async fn client_request_id_is_propagated() {
    let plugin = RequestIdTransformPlugin;
    let mut request = request();
    request.headers.insert(
        axum::http::HeaderName::from_static("x-request-id"),
        axum::http::HeaderValue::from_static("trace-42"),
    );
    plugin
        .transform_request(&mut request)
        .await
        .unwrap_or_else(|error| panic!("transform_request: {error}"));
    assert_eq!(
        request.attributes.get("request_id").map(String::as_str),
        Some("trace-42")
    );
    assert_eq!(
        request
            .headers
            .get(REQUEST_ID_TRANSFORM_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("trace-42")
    );
}

#[tokio::test]
async fn response_echoes_the_request_id() {
    let plugin = RequestIdTransformPlugin;
    let mut context = response();
    context
        .attributes
        .insert("request_id".to_owned(), "trace-42".to_owned());
    plugin
        .transform_response(&mut context)
        .await
        .unwrap_or_else(|error| panic!("transform_response: {error}"));
    assert_eq!(
        context
            .headers
            .get(REQUEST_ID_TRANSFORM_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("trace-42")
    );
}

#[tokio::test]
async fn error_context_always_gains_a_request_id() {
    let plugin = RequestIdTransformPlugin;
    let mut context = error();
    plugin
        .transform_error(&mut context)
        .await
        .unwrap_or_else(|error| panic!("transform_error: {error}"));
    assert!(context.attributes.contains_key("request_id"));
}

#[tokio::test]
async fn error_context_keeps_the_existing_request_id() {
    let plugin = RequestIdTransformPlugin;
    let mut context = error();
    context
        .attributes
        .insert("request_id".to_owned(), "trace-42".to_owned());
    plugin
        .transform_error(&mut context)
        .await
        .unwrap_or_else(|error| panic!("transform_error: {error}"));
    assert_eq!(
        context.attributes.get("request_id").map(String::as_str),
        Some("trace-42")
    );
}
