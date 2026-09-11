//! Unit tests for the request-id transform plugin.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{REQUEST_ID_HEADER, RequestIdTransformPlugin};
use crate::domain::plugin::{PluginResponseContext, TransformPlugin};

fn response_context(request_id: &str) -> PluginResponseContext {
    PluginResponseContext {
        request_id: request_id.to_owned(),
        status: 200,
        headers: http::HeaderMap::new(),
    }
}

#[tokio::test]
async fn stamps_the_outbound_request_with_the_correlation_id() {
    let plugin = RequestIdTransformPlugin;
    let mut request = crate::domain::plugin::test_support::context("GET", "/v1/things");
    request.request_id = "abc-123".to_owned();

    plugin
        .transform_request(&mut request, &serde_json::Value::Null)
        .await
        .unwrap();

    assert_eq!(request.headers.get(REQUEST_ID_HEADER).unwrap(), "abc-123");
    assert_eq!(request.request_id, "abc-123", "an existing id is kept");
}

#[tokio::test]
async fn generates_an_id_when_none_was_carried() {
    let plugin = RequestIdTransformPlugin;
    let mut request = crate::domain::plugin::test_support::context("GET", "/v1/things");
    request.request_id = "   ".to_owned();

    plugin
        .transform_request(&mut request, &serde_json::Value::Null)
        .await
        .unwrap();

    assert!(!request.request_id.trim().is_empty());
    assert_eq!(
        request.headers.get(REQUEST_ID_HEADER).unwrap(),
        &request.request_id
    );
}

#[tokio::test]
async fn echoes_the_id_on_the_response() {
    let plugin = RequestIdTransformPlugin;
    let mut response = response_context("abc-123");

    plugin
        .transform_response(&mut response, &serde_json::Value::Null)
        .await
        .unwrap();

    assert_eq!(response.headers.get(REQUEST_ID_HEADER).unwrap(), "abc-123");
}

#[test]
fn the_identifier_is_the_built_in_request_id_transform() {
    assert_eq!(
        RequestIdTransformPlugin.id(),
        crate::gts_helpers::TRANSFORM_REQUEST_ID
    );
}
