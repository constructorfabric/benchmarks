//! Tests for [`crate::infra::plugin::request_id`].

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use uuid::Uuid;

use super::{REQUEST_ID_HEADER, RequestIdTransformPlugin};
use crate::domain::plugin::{
    ErrorContext, RequestContext, ResponseContext, TRANSFORM_PLUGIN_TYPE_ID, TransformPlugin,
    builtin,
};

fn request() -> RequestContext {
    RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(Uuid::from_u128(0x11))
        .build()
}

#[test]
fn plugin_declares_the_adr_ids() {
    let plugin = RequestIdTransformPlugin;
    assert_eq!(plugin.id(), builtin::REQUEST_ID_TRANSFORM);
    assert_eq!(plugin.plugin_type(), TRANSFORM_PLUGIN_TYPE_ID);
    assert_eq!(
        RequestIdTransformPlugin::PLUGIN_ID,
        builtin::REQUEST_ID_TRANSFORM
    );
    assert_eq!(
        RequestIdTransformPlugin::PLUGIN_TYPE,
        TRANSFORM_PLUGIN_TYPE_ID
    );
    assert_eq!(REQUEST_ID_HEADER, "x-request-id");
}

#[tokio::test]
async fn an_inbound_request_id_is_propagated_unchanged() {
    let mut headers = HeaderMap::new();
    headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("inbound-id"));
    let mut ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(Uuid::from_u128(1))
        .headers(headers)
        .build();

    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");
    assert_eq!(ctx.request_id.as_deref(), Some("inbound-id"));
    assert_eq!(
        ctx.header(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("inbound-id")
    );
    assert_eq!(ctx.injected_headers.len(), 1);
    assert_eq!(ctx.injected_headers[0].0.as_str(), REQUEST_ID_HEADER);
}

#[tokio::test]
async fn a_blank_request_id_is_replaced() {
    let mut headers = HeaderMap::new();
    headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("   "));
    let mut ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(Uuid::from_u128(1))
        .headers(headers)
        .build();
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");
    let minted = ctx.request_id.clone().expect("request id");
    assert!(
        Uuid::parse_str(&minted).is_ok(),
        "'{minted}' must be a UUID"
    );
}

#[tokio::test]
async fn an_absent_request_id_is_minted() {
    let mut ctx = request();
    assert!(ctx.request_id.is_none());
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");
    let minted = ctx.request_id.clone().expect("request id");
    assert!(
        Uuid::parse_str(&minted).is_ok(),
        "'{minted}' must be a UUID"
    );
    assert!(ctx.has_header(REQUEST_ID_HEADER));
    assert_eq!(ctx.injected_headers.len(), 1);
}

#[tokio::test]
async fn a_whitespace_trimmed_id_is_propagated() {
    let mut headers = HeaderMap::new();
    headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("  padded-id  "));
    let mut ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(Uuid::from_u128(1))
        .headers(headers)
        .build();
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");
    assert_eq!(ctx.request_id.as_deref(), Some("padded-id"));
}

#[tokio::test]
async fn an_unusable_inbound_id_is_replaced() {
    let mut headers = HeaderMap::new();
    headers.insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("opaque bytes"),
    );
    let mut ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(Uuid::from_u128(1))
        .headers(headers)
        .build();
    // The header cannot be read as UTF-8, so a fresh id is minted.
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");
    let minted = ctx.request_id.clone().expect("request id");
    assert!(
        Uuid::parse_str(&minted).is_ok(),
        "'{minted}' must be a UUID"
    );
}

#[tokio::test]
async fn the_response_carries_the_propagated_id() {
    let mut ctx = request();
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");
    let request_id = ctx.request_id.clone().expect("request id");

    let mut response = ResponseContext::builder()
        .status(StatusCode::OK)
        .request_id(&request_id)
        .build();
    RequestIdTransformPlugin
        .transform_response(&mut response)
        .await
        .expect("response");
    assert_eq!(
        response
            .header(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id.as_str())
    );
}

#[tokio::test]
async fn an_upstream_response_id_wins_over_the_propagated_one() {
    let mut ctx = request();
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");

    let mut response = ResponseContext::builder().status(StatusCode::OK).build();
    response
        .headers
        .insert(REQUEST_ID_HEADER, HeaderValue::from_static("upstream-id"));
    RequestIdTransformPlugin
        .transform_response(&mut response)
        .await
        .expect("response");
    assert_eq!(
        response
            .header(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("upstream-id")
    );
}

#[tokio::test]
async fn a_response_without_a_request_id_is_left_untouched() {
    let mut response = ResponseContext::builder().status(StatusCode::OK).build();
    RequestIdTransformPlugin
        .transform_response(&mut response)
        .await
        .expect("response");
    assert!(!response.has_header(REQUEST_ID_HEADER));
}

#[tokio::test]
async fn the_problem_document_carries_the_id() {
    let mut ctx = request();
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .expect("transform");
    let request_id = ctx.request_id.clone().expect("request id");

    let mut error = ErrorContext::from_error(crate::domain::error::OagwError::route_not_found(
        "no route matched",
    ));
    error.request_id = Some(request_id.clone());
    RequestIdTransformPlugin
        .transform_error(&mut error)
        .await
        .expect("error");
    assert_eq!(
        error
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id.as_str())
    );
}

#[tokio::test]
async fn an_error_without_a_request_id_is_left_untouched() {
    let mut error = ErrorContext::from_error(crate::domain::error::OagwError::route_not_found(
        "no route matched",
    ));
    RequestIdTransformPlugin
        .transform_error(&mut error)
        .await
        .expect("error");
    assert!(error.headers.get(REQUEST_ID_HEADER).is_none());
}

#[test]
fn the_plugin_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync + Copy + 'static>() {}
    assert_send_sync::<RequestIdTransformPlugin>();
}
