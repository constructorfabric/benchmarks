//! Tests for [`crate::infra::plugin::apikey`].

use std::collections::HashMap;
use std::sync::Arc;

use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{ApiKeyAuthPlugin, DEFAULT_API_KEY_HEADER, api_key_reference_key};
use crate::domain::plugin::{AUTH_PLUGIN_TYPE_ID, AuthPlugin, RequestContext, builtin};
use crate::infra::plugin::secret::{SecretResolver, StaticSecretResolver};

const TENANT: Uuid = Uuid::from_u128(0x11);

fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x33))
        .subject_tenant_id(TENANT)
        .build()
        .expect("security context")
}

fn request() -> RequestContext {
    RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(TENANT)
        .security(Arc::new(context()))
        .build()
}

fn static_resolver() -> Arc<dyn SecretResolver> {
    Arc::new(StaticSecretResolver::new(HashMap::from([
        ("payments-key".to_owned(), "resolved-key".to_owned()),
        ("empty-key".to_owned(), "   ".to_owned()),
    ])))
}

fn literal_plugin(config: serde_json::Value) -> ApiKeyAuthPlugin {
    ApiKeyAuthPlugin::new(static_resolver(), &config).expect("plugin")
}

#[test]
fn plugin_declares_the_adr_ids() {
    let plugin = literal_plugin(serde_json::json!({ "key": "abc" }));
    assert_eq!(plugin.id(), builtin::APIKEY_AUTH);
    assert_eq!(plugin.plugin_type(), AUTH_PLUGIN_TYPE_ID);
    assert_eq!(ApiKeyAuthPlugin::PLUGIN_ID, builtin::APIKEY_AUTH);
}

#[test]
fn configuration_requires_a_key_or_a_reference() {
    let error = ApiKeyAuthPlugin::new(static_resolver(), &serde_json::json!({}))
        .expect_err("no credential source");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn configuration_rejects_unknown_keys() {
    let error = ApiKeyAuthPlugin::new(
        static_resolver(),
        &serde_json::json!({ "key": "abc", "unknown": true }),
    )
    .expect_err("unknown key");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn configuration_rejects_a_non_object_payload() {
    let error = ApiKeyAuthPlugin::new(static_resolver(), &serde_json::json!(["key"]))
        .expect_err("not an object");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn a_null_payload_is_treated_as_an_empty_object() {
    let error = ApiKeyAuthPlugin::new(static_resolver(), &serde_json::Value::Null)
        .expect_err("no credential source");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn default_header_is_x_api_key() {
    assert_eq!(DEFAULT_API_KEY_HEADER, "x-api-key");
}

#[tokio::test]
async fn literal_key_is_injected_into_the_default_header() {
    let mut ctx = request();
    literal_plugin(serde_json::json!({ "key": "raw-key" }))
        .authenticate(&mut ctx)
        .await
        .expect("injected");
    assert_eq!(
        ctx.header(DEFAULT_API_KEY_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("raw-key")
    );
    assert_eq!(ctx.injected_headers.len(), 1);
    assert_eq!(ctx.injected_headers[0].0.as_str(), DEFAULT_API_KEY_HEADER);
    assert!(ctx.injected_query.is_empty());
}

#[tokio::test]
async fn custom_header_name_is_honoured() {
    let mut ctx = request();
    literal_plugin(serde_json::json!({ "key": "raw-key", "header_name": "x-vendor-key" }))
        .authenticate(&mut ctx)
        .await
        .expect("injected");
    assert_eq!(
        ctx.header("x-vendor-key")
            .and_then(|value| value.to_str().ok()),
        Some("raw-key")
    );
    assert!(!ctx.has_header(DEFAULT_API_KEY_HEADER));
}

#[tokio::test]
async fn query_name_routes_the_key_into_the_query_string() {
    let mut ctx = request();
    literal_plugin(serde_json::json!({ "key": "raw-key", "query_name": "apiKey" }))
        .authenticate(&mut ctx)
        .await
        .expect("injected");
    assert!(
        ctx.injected_query
            .iter()
            .any(|(name, value)| name == "apiKey" && value == "raw-key")
    );
    assert!(!ctx.has_header(DEFAULT_API_KEY_HEADER));
}

#[tokio::test]
async fn reference_key_is_resolved_at_request_time() {
    let mut ctx = request();
    literal_plugin(serde_json::json!({ "secret_ref": "cred://payments-key" }))
        .authenticate(&mut ctx)
        .await
        .expect("injected");
    assert_eq!(
        ctx.header(DEFAULT_API_KEY_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("resolved-key")
    );
}

#[tokio::test]
async fn an_unresolvable_reference_rejects_with_401() {
    let mut ctx = request();
    let plugin = ApiKeyAuthPlugin::new(
        static_resolver(),
        &serde_json::json!({ "secret_ref": "cred://absent-key" }),
    )
    .expect("plugin");
    let error = plugin.authenticate(&mut ctx).await.expect_err("401");
    assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert!(error.detail().contains("absent-key"));
}

#[tokio::test]
async fn an_empty_resolved_value_rejects_with_401() {
    let mut ctx = request();
    let plugin = literal_plugin(serde_json::json!({ "secret_ref": "cred://empty-key" }));
    let error = plugin.authenticate(&mut ctx).await.expect_err("401");
    assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_empty_literal_rejects_with_401() {
    let mut ctx = request();
    let plugin = literal_plugin(serde_json::json!({ "key": "  " }));
    let error = plugin.authenticate(&mut ctx).await.expect_err("401");
    assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_missing_security_context_rejects_with_401() {
    let mut ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(TENANT)
        .build();
    let plugin = literal_plugin(serde_json::json!({ "secret_ref": "cred://payments-key" }));
    let error = plugin.authenticate(&mut ctx).await.expect_err("401");
    assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert!(error.detail().contains("no security context"));
}

#[tokio::test]
async fn a_failing_credential_store_surfaces_the_store_error() {
    let mut ctx = request();
    let plugin = ApiKeyAuthPlugin::new(
        Arc::new(CredStoreFailing),
        &serde_json::json!({ "secret_ref": "cred://payments-key" }),
    )
    .expect("plugin");
    let error = plugin.authenticate(&mut ctx).await.expect_err("500");
    assert_eq!(
        error.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[test]
fn reference_keys_strip_the_scheme() {
    assert_eq!(api_key_reference_key("cred://payments-key"), "payments-key");
}

/// Resolver that always fails, exercising the store-error path.
struct CredStoreFailing;

#[async_trait::async_trait]
impl SecretResolver for CredStoreFailing {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<Option<String>, crate::domain::error::OagwError> {
        Err(crate::domain::error::OagwError::secret_not_found(format!(
            "credential store lookup for '{secret_ref}' failed"
        )))
    }
}
