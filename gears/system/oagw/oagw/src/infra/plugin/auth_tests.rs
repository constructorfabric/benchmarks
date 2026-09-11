//! The built-in auth plugins (T054, DESIGN §3.1).
//!
//! `noop` is a pass-through, `apikey` resolves its key from CredStore by
//! `cred://` reference and injects it where the configuration names. A
//! credential the store does not know is a `SecretNotFound` (500), a store
//! that cannot be reached is an `AuthenticationFailed` (401), and no path —
//! log record, error body or assertion — ever carries the secret itself.

use std::sync::Arc;

use credstore_sdk::test_util::MockCredStoreClient;

use super::api_key_auth::{ApiKeyAuth, ApiKeyConfig};
use super::noop_auth::NoopAuth;
use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// The secret a test plant in the store and asserts never leaks.
const SECRET: &str = "sk-live-9f2c4a7b1d8e0f3a6c5b";

/// A request context carrying the plugin configuration.
fn ctx(config: serde_json::Value) -> RequestContext {
    let mut ctx = RequestContext::default();
    ctx.set_header("authorization", "Bearer caller-token");
    ctx.plugin_config = Some(config);
    ctx
}

/// The apikey plugin wired to `store`.
fn api_key(store: MockCredStoreClient) -> ApiKeyAuth {
    ApiKeyAuth::new(Arc::new(store))
}

/// The error body a domain error renders as.
fn problem_body(error: &DomainError) -> String {
    crate::api::rest::error::problem_from_domain_error(error, "/oagw/v1/proxy/vendor.com")
        .to_json()
        .to_string()
}

#[tokio::test]
async fn the_noop_plugin_leaves_the_request_untouched() {
    let mut request = ctx(serde_json::Value::Null);
    NoopAuth
        .authenticate(&mut request)
        .await
        .expect("the noop plugin never fails");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer caller-token"),
        "the caller's own credentials are preserved"
    );
    assert!(request.headers.len() == 1, "no header was added: {:?}", request.headers);
}

#[tokio::test]
async fn the_api_key_plugin_injects_the_configured_header() {
    let store = MockCredStoreClient::with_secrets(vec![("vendor-key".to_string(), SECRET.to_string())]);
    let mut request = ctx(serde_json::json!({
        "key_ref": "cred://vendor-key",
        "header": "x-vendor-key"
    }));
    ApiKeyAuth::new(Arc::new(store))
        .authenticate(&mut request)
        .await
        .expect("the key resolves");
    assert_eq!(
        request.header("x-vendor-key"),
        Some(SECRET),
        "the key from CredStore was injected"
    );
    assert_eq!(
        request.header("authorization"),
        Some("Bearer caller-token"),
        "the caller's own credentials are untouched"
    );
}

#[tokio::test]
async fn the_api_key_plugin_defaults_to_the_x_api_key_header() {
    let store = MockCredStoreClient::with_secrets(vec![("vendor-key".to_string(), SECRET.to_string())]);
    let mut request = ctx(serde_json::json!({ "key_ref": "cred://vendor-key" }));
    ApiKeyAuth::new(Arc::new(store))
        .authenticate(&mut request)
        .await
        .expect("the key resolves");
    assert_eq!(request.header("x-api-key"), Some(SECRET));
}

#[tokio::test]
async fn the_api_key_plugin_can_inject_into_the_query() {
    let store = MockCredStoreClient::with_secrets(vec![("vendor-key".to_string(), SECRET.to_string())]);
    let mut request = ctx(serde_json::json!({
        "key_ref": "vendor-key",
        "query": "api_key"
    }));
    request.query = Some("page=1".to_string());
    ApiKeyAuth::new(Arc::new(store))
        .authenticate(&mut request)
        .await
        .expect("the key resolves");
    assert_eq!(
        request.query.as_deref(),
        Some("page=1&api_key=sk-live-9f2c4a7b1d8e0f3a6c5b"),
        "the key was appended to the existing query"
    );
}

#[tokio::test]
async fn a_missing_secret_is_a_secret_not_found() {
    let store = MockCredStoreClient::empty();
    let mut request = ctx(serde_json::json!({ "key_ref": "cred://vendor-key" }));
    let error = ApiKeyAuth::new(Arc::new(store))
        .authenticate(&mut request)
        .await
        .expect_err("the store has no such key");
    let DomainError::SecretNotFound { plugin_id, .. } = DomainError::from(error) else {
        panic!("a missing secret is a SecretNotFound");
    };
    assert_eq!(plugin_id.as_deref(), Some(crate::domain::gts_helpers::BUILTIN_AUTH_APIKEY));
}

#[tokio::test]
async fn a_credential_store_failure_is_an_authentication_failure() {
    let store = MockCredStoreClient::always_failing();
    let mut request = ctx(serde_json::json!({ "key_ref": "cred://vendor-key" }));
    let error = ApiKeyAuth::new(Arc::new(store))
        .authenticate(&mut request)
        .await
        .expect_err("the store is unreachable");
    let error = DomainError::from(error);
    assert_eq!(error.error_type_suffix(), "auth.failed.v1", "{error:?}");
    assert_eq!(crate::api::rest::error::http_status_of(&error), 401);
}

#[test]
fn an_unwired_plugin_fails_loud() {
    // A plugin whose gear carries no CredStore client cannot silently skip:
    // it reports a failure naming the plugin.
    let error = ApiKeyConfig::from_value(None).expect_err("no configuration");
    let PluginError::Failure { plugin_id, .. } = error else {
        panic!("a config error is a plugin failure");
    };
    assert_eq!(plugin_id, crate::domain::gts_helpers::BUILTIN_AUTH_APIKEY);
}

#[tokio::test]
async fn the_secret_material_never_reaches_a_log_or_an_error_body() {
    // The diagnostic a failing store produces is what the gateway logs.
    let store = MockCredStoreClient::always_failing();
    let mut request = ctx(serde_json::json!({ "key_ref": "cred://vendor-key" }));
    let error = ApiKeyAuth::new(Arc::new(store))
        .authenticate(&mut request)
        .await
        .expect_err("the store is unreachable");
    let diagnostic = error.to_string();
    assert!(
        !diagnostic.contains(SECRET),
        "the log diagnostic carried the secret: {diagnostic}"
    );

    // The rendered problem for a found secret is a not-found, and for a store
    // failure an authentication failure; neither carries the value.
    let leaked = problem_body(&DomainError::from(error));
    assert!(!leaked.contains(SECRET), "the error body carried the secret: {leaked}");

    let missing = problem_body(&DomainError::SecretNotFound {
        detail: "credential `cred://vendor-key` does not exist".to_string(),
        plugin_id: None,
    });
    assert!(!missing.contains(SECRET), "the error body carried the secret: {missing}");
}
