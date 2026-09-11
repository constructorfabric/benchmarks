#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the `apikey` auth plugin (ADR-0008).

use std::sync::Arc;

use bytes::Bytes;
use serde_json::json;
use uuid::Uuid;

use super::{ApiKeyAuthPlugin, normalize_secret_ref};
use crate::domain::gts_helpers::{AUTH_APIKEY, AUTH_NOOP};
use crate::domain::plugin::{AuthPlugin, PluginError, ProxyRequest};
use crate::infra::credentials::{CredStoreResolver, SecretResolver, SecretValue};

const LIVE_KEY: &str = "sk-live-9f2b";

fn request() -> ProxyRequest {
    ProxyRequest {
        method: axum::http::Method::GET,
        path: "/things".to_string(),
        query: String::new(),
        headers: axum::http::HeaderMap::new(),
        body: Bytes::new(),
        tenant_id: Uuid::nil(),
        security: None,
    }
}

fn store_resolver() -> SecretResolver {
    let store = credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
        ("payments-api-key".to_string(), LIVE_KEY.to_string()),
    ]);
    Arc::new(CredStoreResolver::new(Arc::new(store)))
}

#[tokio::test]
async fn it_injects_the_resolved_secret_into_the_configured_header() {
    let plugin = ApiKeyAuthPlugin::new(Some(store_resolver()));
    let mut request = request();
    plugin
        .authenticate(
            &mut request,
            &json!({"secret_ref": "cred://payments-api-key", "header": "X-Api-Key"}),
        )
        .await
        .unwrap();
    assert_eq!(request.header("x-api-key").as_deref(), Some(LIVE_KEY));
}

#[tokio::test]
async fn it_defaults_to_the_x_api_key_header() {
    let plugin = ApiKeyAuthPlugin::new(Some(store_resolver()));
    let mut request = request();
    plugin
        .authenticate(&mut request, &json!({"secret_ref": "payments-api-key"}))
        .await
        .unwrap();
    assert_eq!(request.header("x-api-key").as_deref(), Some(LIVE_KEY));
}

#[tokio::test]
async fn it_can_place_the_secret_in_the_query_instead() {
    let plugin = ApiKeyAuthPlugin::new(Some(store_resolver()));
    let mut request = request();
    request.query = "page=2".to_string();
    plugin
        .authenticate(
            &mut request,
            &json!({"secret_ref": "payments-api-key", "in": "query", "query_param": "key"}),
        )
        .await
        .unwrap();
    assert_eq!(request.query, "page=2&key=sk-live-9f2b");
    assert!(request.headers.is_empty(), "no header may be set in query mode");
}

#[tokio::test]
async fn it_uses_the_default_query_parameter_name_when_none_is_configured() {
    let plugin = ApiKeyAuthPlugin::new(Some(store_resolver()));
    let mut request = request();
    plugin
        .authenticate(&mut request, &json!({"secret_ref": "payments-api-key", "in": "query"}))
        .await
        .unwrap();
    assert_eq!(request.query, "api_key=sk-live-9f2b");
}

#[tokio::test]
async fn a_missing_secret_reference_is_a_configuration_error() {
    let plugin = ApiKeyAuthPlugin::new(Some(store_resolver()));
    let mut request = request();
    let error = plugin.authenticate(&mut request, &json!({"in": "query"})).await.unwrap_err();
    assert!(matches!(error, PluginError::Config(_)), "{error:?}");
}

#[tokio::test]
async fn an_unresolvable_reference_fails_without_leaking_a_value() {
    let plugin = ApiKeyAuthPlugin::default();
    let mut request = request();
    let error = plugin
        .authenticate(&mut request, &json!({"secret_ref": "cred://nope"}))
        .await
        .unwrap_err();
    match error {
        PluginError::SecretNotFound(detail) => {
            assert_eq!(detail, "cred://nope");
            assert!(!detail.contains("sk-live"), "a secret must never be echoed: {detail}");
        }
        other => panic!("expected SecretNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn the_plugin_does_not_render_its_store() {
    let plugin = ApiKeyAuthPlugin::new(Some(store_resolver()));
    let rendered = format!("{plugin:?}");
    assert!(!rendered.contains(LIVE_KEY), "the resolver must not leak its store: {rendered}");
}

#[test]
fn it_advertises_the_catalogued_identifier() {
    assert_eq!(ApiKeyAuthPlugin::default().id(), AUTH_APIKEY);
    assert_eq!(ApiKeyAuthPlugin::default().plugin_type(), "apikey");
    assert_ne!(ApiKeyAuthPlugin::default().id(), AUTH_NOOP);
}

#[test]
fn it_strips_the_cred_scheme_from_references() {
    assert_eq!(normalize_secret_ref("cred://payments-api-key"), "payments-api-key");
    assert_eq!(normalize_secret_ref("cred:///payments-api-key"), "payments-api-key");
    assert_eq!(normalize_secret_ref("payments-api-key"), "payments-api-key");
}

#[test]
fn the_value_wrapper_redacts_itself() {
    let secret = SecretValue::new(LIVE_KEY);
    assert_eq!(format!("{secret:?}"), "[REDACTED]");
    assert_eq!(secret.expose(), LIVE_KEY, "expose() is the controlled read");
}
