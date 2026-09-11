#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the OAGW-rendering JSON body extractor.

use axum::extract::FromRequest;
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::*;

#[derive(Debug, serde::Deserialize)]
struct Payload {
    pub name: String,
}

async fn extract(body: &'static str) -> Result<Payload, axum::response::Response> {
    let request = axum::extract::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap();
    JsonBody::<Payload>::from_request(request, &()).await.map(|JsonBody(payload)| payload)
}

#[tokio::test]
async fn a_well_formed_body_parses() {
    let payload = extract(r#"{"name":"gw"}"#).await.unwrap();
    assert_eq!(payload.name, "gw");
}

#[tokio::test]
async fn an_empty_body_is_rejected_as_a_validation_problem() {
    let rejection = extract("").await.unwrap_err();
    assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
    let text = axum::body::to_bytes(rejection.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&text).unwrap();
    assert_eq!(body["code"], "invalid-payload");
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_VALIDATION);
}

#[tokio::test]
async fn a_malformed_body_is_a_problem_not_a_panic() {
    let rejection = extract("{not json").await.unwrap_err();
    assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
    let text = axum::body::to_bytes(rejection.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&text).unwrap();
    assert_eq!(body["code"], "invalid-payload");
    assert!(
        body["detail"].as_str().unwrap().contains("invalid-payload"),
        "{}",
        body["detail"]
    );
}

#[tokio::test]
async fn a_type_mismatch_is_reported_as_a_problem() {
    let rejection = extract(r#"{"name": 42}"#).await.unwrap_err();
    assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn the_public_tenant_is_the_nil_uuid() {
    assert_eq!(PUBLIC_TENANT, uuid::Uuid::nil());
}

#[tokio::test]
async fn the_router_carries_both_surfaces() {
    let stores = std::sync::Arc::new(crate::infra::storage::memory::MemoryStores::new());
    let settings = crate::infra::proxy::service::DataPlaneSettings::default();
    let resolver: crate::infra::credentials::SecretResolver =
        std::sync::Arc::new(crate::infra::credentials::MissingResolver);
    let data = std::sync::Arc::new(crate::infra::proxy::service::DataPlane::new(
        stores,
        crate::infra::plugin::AuthPluginRegistry::with_builtins(std::sync::Arc::clone(&resolver)),
        crate::infra::plugin::GuardPluginRegistry::with_builtins(),
        crate::infra::plugin::TransformPluginRegistry::with_builtins(),
        settings,
        Some(resolver),
    ));
    let control = std::sync::Arc::new(crate::domain::ControlPlane::new(
        std::sync::Arc::clone(&data.stores().upstreams) as _,
        std::sync::Arc::clone(&data.stores().routes) as _,
        std::sync::Arc::clone(&data.stores().plugins) as _,
        true,
    ));
    assert!(
        router(crate::api::ApiState::new(control, data, std::sync::Arc::new(super::NoAncestors)))
            .has_routes(),
        "management and proxy routes must both be mounted"
    );
}

#[test]
fn the_proxy_prefix_is_the_mount_point_of_the_data_plane() {
    assert_eq!(crate::api::proxy::PROXY_PREFIX, "/oagw/v1/proxy/");
}

#[test]
fn json_response_stamps_the_status() {
    let response = json_response(StatusCode::CREATED, &json!({"ok": true}));
    assert_eq!(response.status(), StatusCode::CREATED);
    let headers = response.headers().clone();
    let content_type = headers.get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(content_type.starts_with("application/json"), "{content_type}");
}
