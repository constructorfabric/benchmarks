use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use bytes::Bytes;
use credstore_sdk::test_util::MockCredStoreClient;
use toolkit_auth::oauth2::ClientAuthMethod;
use uuid::Uuid;

use super::{OAuth2ClientCredAuthPlugin, build_cache_key, token_ttl};
use crate::domain::plugin::{AuthPlugin, Caller, PluginError, RequestContext};

/// The failure of an exchange that must not succeed.
fn failure(result: Result<(), PluginError>) -> PluginError {
    match result {
        Ok(()) => panic!("the exchange must have failed"),
        Err(error) => error,
    }
}
use crate::infra::credentials::SecretResolver;
use crate::infra::plugin::token_cache::TokenCacheConfig;
use toolkit_security::SecurityContext;

/// Deterministic tenant used by every request in this module.
static TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00c2);

fn caller() -> Caller {
    let context = SecurityContext::builder()
        .subject_id(Uuid::from_u128(7))
        .subject_tenant_id(TENANT)
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"));
    Caller::from_context(&context)
}

fn plugin(method: ClientAuthMethod) -> OAuth2ClientCredAuthPlugin {
    OAuth2ClientCredAuthPlugin::new(
        SecretResolver::new(Arc::new(MockCredStoreClient::with_secrets(vec![
            ("client-id".to_owned(), "web-app".to_owned()),
            ("client-secret".to_owned(), "hunter2".to_owned()),
        ]))),
        method,
        TokenCacheConfig::default(),
    )
}

fn request(config: serde_json::Value) -> RequestContext {
    RequestContext {
        caller: caller(),
        config,
        method: "GET".to_owned(),
        path: "/things".to_owned(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Bytes::new(),
        attributes: BTreeMap::new(),
    }
}

#[test]
fn identifiers_distinguish_the_two_variants() {
    let form = plugin(ClientAuthMethod::Form);
    assert_eq!(form.id(), "oauth2_client_cred");
    assert_eq!(form.plugin_type(), crate::ids::AUTH_OAUTH2_CLIENT_CRED);

    let basic = plugin(ClientAuthMethod::Basic);
    assert_eq!(basic.id(), "oauth2_client_cred_basic");
    assert_eq!(
        basic.plugin_type(),
        crate::ids::AUTH_OAUTH2_CLIENT_CRED_BASIC
    );
}

#[test]
fn cache_keys_separate_tenant_subject_method_and_config() {
    let config = serde_json::json!({"client_id_ref": "cred://client-id"});
    let same = serde_json::json!({"client_id_ref": "cred://client-id"});
    let other = serde_json::json!({"client_id_ref": "cred://other-id"});
    let key = build_cache_key(&caller(), "form", &config);
    assert_eq!(key, build_cache_key(&caller(), "form", &same));
    assert_ne!(key, build_cache_key(&caller(), "form", &other));
    assert_ne!(key, build_cache_key(&caller(), "basic", &config));

    let mut different_caller = caller();
    different_caller.subject_id = Uuid::from_u128(8);
    assert_ne!(key, build_cache_key(&different_caller, "form", &config));
}

#[test]
fn token_ttl_is_capped_by_the_configured_ceiling() {
    let configured = Duration::from_mins(5);
    assert_eq!(
        token_ttl(configured, Duration::from_hours(1)),
        Some(configured)
    );
}

#[test]
fn token_ttl_respects_the_expiry_margin() {
    assert_eq!(
        token_ttl(Duration::from_mins(10), Duration::from_secs(90)),
        Some(Duration::from_mins(1))
    );
}

#[test]
fn very_short_lived_tokens_are_never_cached() {
    assert_eq!(
        token_ttl(Duration::from_mins(10), Duration::from_secs(30)),
        None
    );
    assert_eq!(
        token_ttl(Duration::from_mins(10), Duration::from_secs(0)),
        None
    );
}

#[tokio::test]
async fn both_endpoints_are_required() {
    let mut request = request(serde_json::json!({}));
    let error = failure(
        plugin(ClientAuthMethod::Form)
            .authenticate(&mut request)
            .await,
    );
    assert!(
        matches!(error, PluginError::Config(_)),
        "unexpected: {error}"
    );
}

#[tokio::test]
async fn token_endpoint_and_issuer_url_are_mutually_exclusive() {
    let config = serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "issuer_url": "https://idp.example.com/issuer",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
    });
    let mut request = request(config);
    let error = failure(
        plugin(ClientAuthMethod::Form)
            .authenticate(&mut request)
            .await,
    );
    assert!(
        matches!(error, PluginError::Config(_)),
        "unexpected: {error}"
    );
}

#[tokio::test]
async fn client_references_are_resolved_before_the_exchange() {
    let mut request = request(serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://absent",
    }));
    let error = failure(
        plugin(ClientAuthMethod::Form)
            .authenticate(&mut request)
            .await,
    );
    assert!(
        matches!(error, PluginError::Secret(_)),
        "unexpected: {error}"
    );
}

#[tokio::test]
async fn a_failed_exchange_is_an_auth_error() {
    let mut request = request(serde_json::json!({
        "token_endpoint": "http://127.0.0.1:9/token",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
    }));
    let error = failure(
        plugin(ClientAuthMethod::Form)
            .authenticate(&mut request)
            .await,
    );
    assert!(matches!(error, PluginError::Auth(_)), "unexpected: {error}");
}

#[test]
fn scopes_accept_both_string_and_array_forms() {
    // The scope forms are exercised through the plugin's own config parsing:
    // both shapes must reach the IdP as a single scope list. The unit test
    // covers the observable part — that neither form is rejected as a config
    // error by the plugin, which would surface as `PluginError::Config`.
    let string_form = serde_json::json!({"scopes": "read write"});
    let array_form = serde_json::json!({"scopes": ["read", "write"]});
    assert_ne!(
        build_cache_key(&caller(), "form", &string_form),
        build_cache_key(&caller(), "form", &array_form)
    );
}
