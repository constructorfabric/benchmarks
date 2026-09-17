use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::HeaderMap;
use bytes::Bytes;
use credstore_sdk::test_util::MockCredStoreClient;
use uuid::Uuid;

use super::{ApiKeyAuthPlugin, DEFAULT_HEADER, DEFAULT_PREFIX};
use crate::domain::plugin::{AuthPlugin, Caller, PluginError, RequestContext};
use crate::infra::credentials::SecretResolver;
use toolkit_security::SecurityContext;

/// Deterministic tenant used by every request in this module.
static TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00ca);

fn caller() -> Caller {
    let context = SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(TENANT)
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"));
    Caller::from_context(&context)
}

fn plugin(secrets: &[(&str, &str)]) -> ApiKeyAuthPlugin {
    ApiKeyAuthPlugin::new(SecretResolver::new(Arc::new(
        MockCredStoreClient::with_secrets(
            secrets
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        ),
    )))
}

fn request(config: serde_json::Value, headers: HeaderMap) -> RequestContext {
    RequestContext {
        caller: caller(),
        config,
        method: "GET".to_owned(),
        path: "/things".to_owned(),
        query: Vec::new(),
        headers,
        body: Bytes::new(),
        attributes: BTreeMap::new(),
    }
}

#[test]
fn plugin_identifies_itself() {
    let plugin = plugin(&[]);
    assert_eq!(plugin.id(), "apikey");
    assert_eq!(plugin.plugin_type(), crate::ids::AUTH_APIKEY);
}

#[test]
fn defaults_are_documented_header_and_prefix() {
    assert_eq!(DEFAULT_HEADER, "authorization");
    assert_eq!(DEFAULT_PREFIX, "Bearer");
}

#[tokio::test]
async fn secret_is_injected_with_the_default_prefix() {
    let plugin = plugin(&[("key", "sk_live_42")]);
    let mut request = request(
        serde_json::json!({"secret_ref": "cred://key"}),
        HeaderMap::new(),
    );
    plugin
        .authenticate(&mut request)
        .await
        .unwrap_or_else(|error| panic!("authenticate: {error}"));
    let value = request
        .headers
        .get(DEFAULT_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("`{DEFAULT_HEADER}` header is missing"));
    assert_eq!(value, "Bearer sk_live_42");
}

#[tokio::test]
async fn the_header_and_prefix_are_configurable() {
    let plugin = plugin(&[("key", "sk_live_42")]);
    let mut request = request(
        serde_json::json!({"api_key_ref": "key", "header": "x-api-key", "prefix": "Key"}),
        HeaderMap::new(),
    );
    plugin
        .authenticate(&mut request)
        .await
        .unwrap_or_else(|error| panic!("authenticate: {error}"));
    assert_eq!(
        request
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("Key sk_live_42")
    );
}

#[tokio::test]
async fn an_empty_prefix_sends_the_raw_secret() {
    let plugin = plugin(&[("key", "raw-key")]);
    let mut request = request(
        serde_json::json!({"secret_ref": "key", "header": "x-api-key", "prefix": ""}),
        HeaderMap::new(),
    );
    plugin
        .authenticate(&mut request)
        .await
        .unwrap_or_else(|error| panic!("authenticate: {error}"));
    assert_eq!(
        request
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("raw-key")
    );
}

#[tokio::test]
async fn missing_configuration_is_a_config_error() {
    let plugin = plugin(&[]);
    let mut request = request(serde_json::json!({}), HeaderMap::new());
    let error = plugin.authenticate(&mut request).await.unwrap_err();
    assert!(
        matches!(error, PluginError::Config(_)),
        "unexpected: {error}"
    );
}

#[tokio::test]
async fn unresolvable_secret_is_a_secret_error() {
    let plugin = plugin(&[]);
    let mut request = request(
        serde_json::json!({"secret_ref": "cred://absent"}),
        HeaderMap::new(),
    );
    let error = plugin.authenticate(&mut request).await.unwrap_err();
    assert!(
        matches!(error, PluginError::Secret(_)),
        "unexpected: {error}"
    );
}

#[tokio::test]
async fn an_invalid_header_name_is_a_config_error() {
    let plugin = plugin(&[("key", "value")]);
    let mut request = request(
        serde_json::json!({"secret_ref": "key", "header": "bad header\nname"}),
        HeaderMap::new(),
    );
    let error = plugin.authenticate(&mut request).await.unwrap_err();
    assert!(
        matches!(error, PluginError::Config(_)),
        "unexpected: {error}"
    );
}

#[tokio::test]
async fn an_existing_header_is_overwritten() {
    let plugin = plugin(&[("key", "fresh")]);
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::HeaderName::from_static("authorization"),
        axum::http::HeaderValue::from_static("Bearer stale"),
    );
    let mut request = request(serde_json::json!({"secret_ref": "key"}), headers);
    plugin
        .authenticate(&mut request)
        .await
        .unwrap_or_else(|error| panic!("authenticate: {error}"));
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer fresh")
    );
}
