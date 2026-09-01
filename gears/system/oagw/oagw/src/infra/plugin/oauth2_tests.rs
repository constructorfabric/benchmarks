//! Tests for [`crate::infra::plugin::oauth2`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::test_util::MockCredStoreClient;
use httpmock::prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    AUTHORIZATION_HEADER, BASIC_AUTH_METHOD_TAG, FORM_AUTH_METHOD_TAG, OAuth2ClientCredAuthPlugin,
    TOKEN_EXPIRY_SAFETY_MARGIN, TokenCacheConfig,
};
use crate::config::OagwConfig;
use crate::domain::plugin::{AUTH_PLUGIN_TYPE_ID, AuthPlugin, RequestContext, builtin};
use crate::infra::plugin::secret::{CredStoreSecretResolver, SecretResolver, StaticSecretResolver};

const TENANT: Uuid = Uuid::from_u128(0x11);
const SUBJECT: Uuid = Uuid::from_u128(0x33);

fn security() -> Arc<SecurityContext> {
    security_for(SUBJECT, TENANT)
}

fn security_for(subject: Uuid, tenant: Uuid) -> Arc<SecurityContext> {
    Arc::new(
        SecurityContext::builder()
            .subject_id(subject)
            .subject_tenant_id(tenant)
            .build()
            .expect("security context"),
    )
}

fn resolver() -> Arc<dyn SecretResolver> {
    Arc::new(StaticSecretResolver::new(HashMap::from([
        ("client-id".to_owned(), "test-client".to_owned()),
        ("client-secret".to_owned(), "test-secret".to_owned()),
        ("other-client-id".to_owned(), "other-client".to_owned()),
        ("other-client-secret".to_owned(), "other-secret".to_owned()),
    ])))
}

fn cache_config() -> TokenCacheConfig {
    TokenCacheConfig {
        ttl: Duration::from_secs(300),
        capacity: 16,
    }
}

fn plugin(
    auth_method: toolkit_auth::ClientAuthMethod,
    config: serde_json::Value,
) -> OAuth2ClientCredAuthPlugin {
    let mut plugin =
        OAuth2ClientCredAuthPlugin::new(resolver(), auth_method, cache_config(), &config)
            .expect("plugin");
    plugin.set_http_config(toolkit_http::HttpClientConfig::for_testing());
    plugin
}

fn form_plugin(config: serde_json::Value) -> OAuth2ClientCredAuthPlugin {
    plugin(toolkit_auth::ClientAuthMethod::Form, config)
}

fn basic_plugin(config: serde_json::Value) -> OAuth2ClientCredAuthPlugin {
    plugin(toolkit_auth::ClientAuthMethod::Basic, config)
}

fn request() -> RequestContext {
    RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(TENANT)
        .security(security())
        .build()
}

fn token_body(token: &str, expires_in: u64) -> String {
    format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
}

fn token_endpoint_config(server: &MockServer) -> serde_json::Value {
    endpoint_config(&format!("http://localhost:{}/token", server.port()))
}

fn endpoint_config(token_endpoint: &str) -> serde_json::Value {
    serde_json::json!({
        "token_endpoint": token_endpoint,
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    })
}

const FIRST_TOKEN_ENDPOINT: &str = "https://idp-one.example.com/token";
const OTHER_TOKEN_ENDPOINT: &str = "https://idp-two.example.com/token";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn plugin_declares_the_adr_ids() {
    let config = serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    });
    assert_eq!(
        form_plugin(config.clone()).id(),
        builtin::OAUTH2_CLIENT_CRED
    );
    assert_eq!(basic_plugin(config).id(), builtin::OAUTH2_CLIENT_CRED_BASIC);
    assert_eq!(
        OAuth2ClientCredAuthPlugin::FORM_PLUGIN_ID,
        builtin::OAUTH2_CLIENT_CRED
    );
    assert_eq!(
        OAuth2ClientCredAuthPlugin::BASIC_PLUGIN_ID,
        builtin::OAUTH2_CLIENT_CRED_BASIC
    );
    assert_eq!(OAuth2ClientCredAuthPlugin::PLUGIN_TYPE, AUTH_PLUGIN_TYPE_ID);
}

#[test]
fn token_endpoint_and_issuer_url_are_mutually_exclusive() {
    let config = serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "issuer_url": "https://idp.example.com",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    });
    let error = OAuth2ClientCredAuthPlugin::new(
        resolver(),
        toolkit_auth::ClientAuthMethod::Form,
        cache_config(),
        &config,
    )
    .expect_err("both set");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn one_of_token_endpoint_or_issuer_url_is_required() {
    let config = serde_json::json!({
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    });
    let error = OAuth2ClientCredAuthPlugin::new(
        resolver(),
        toolkit_auth::ClientAuthMethod::Form,
        cache_config(),
        &config,
    )
    .expect_err("neither set");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn references_must_be_cred_uris() {
    let config = serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "client_id_ref": "client-id",
        "client_secret_ref": "cred://client-secret"
    });
    let error = OAuth2ClientCredAuthPlugin::new(
        resolver(),
        toolkit_auth::ClientAuthMethod::Form,
        cache_config(),
        &config,
    )
    .expect_err("not a cred:// reference");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("client_id_ref"));
}

#[test]
fn unknown_configuration_keys_are_rejected() {
    let config = serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
        "unknown": true
    });
    let error = OAuth2ClientCredAuthPlugin::new(
        resolver(),
        toolkit_auth::ClientAuthMethod::Form,
        cache_config(),
        &config,
    )
    .expect_err("unknown key");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_invalid_token_endpoint_url_is_rejected_with_400() {
    let config = endpoint_config("not a url");
    let mut ctx = request();
    let error = form_plugin(config)
        .authenticate(&mut ctx)
        .await
        .expect_err("400");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("token_endpoint"));
}

#[test]
fn the_method_tags_match_the_adr() {
    assert_eq!(
        super::auth_method_tag(toolkit_auth::ClientAuthMethod::Form),
        FORM_AUTH_METHOD_TAG
    );
    assert_eq!(
        super::auth_method_tag(toolkit_auth::ClientAuthMethod::Basic),
        BASIC_AUTH_METHOD_TAG
    );
    assert_eq!(FORM_AUTH_METHOD_TAG, "form");
    assert_eq!(BASIC_AUTH_METHOD_TAG, "basic");
}

#[test]
fn the_config_fingerprint_is_order_independent_and_stable() {
    let left = form_plugin(endpoint_config(FIRST_TOKEN_ENDPOINT));
    let right = form_plugin(serde_json::json!({
        "client_secret_ref": "cred://client-secret",
        "token_endpoint": FIRST_TOKEN_ENDPOINT,
        "client_id_ref": "cred://client-id"
    }));
    assert_eq!(left.config_fingerprint(), right.config_fingerprint());

    let different = form_plugin(endpoint_config(OTHER_TOKEN_ENDPOINT));
    assert_ne!(left.config_fingerprint(), different.config_fingerprint());
}

#[test]
fn cache_keys_separate_tenants_subjects_methods_and_configs() {
    let base = form_plugin(endpoint_config(FIRST_TOKEN_ENDPOINT));
    let ctx = request();

    let key = base.cache_key(&ctx);
    assert!(
        key.starts_with(&format!("{TENANT}:{SUBJECT}:form:")),
        "key is '{key}'"
    );

    let other_tenant = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(TENANT)
        .security(security_for(SUBJECT, Uuid::from_u128(0x99)))
        .build();
    assert_ne!(key, base.cache_key(&other_tenant));

    let other_subject = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(TENANT)
        .security(security_for(Uuid::from_u128(0x44), TENANT))
        .build();
    assert_ne!(key, base.cache_key(&other_subject));

    let other_method = basic_plugin(endpoint_config(FIRST_TOKEN_ENDPOINT));
    assert_ne!(key, other_method.cache_key(&ctx));

    let other_config = form_plugin(endpoint_config(OTHER_TOKEN_ENDPOINT));
    assert_ne!(key, other_config.cache_key(&ctx));
}

#[test]
fn the_config_cache_ttl_is_derived_from_the_gear_configuration() {
    let config = OagwConfig::default();
    let cache = TokenCacheConfig::from(&config);
    assert_eq!(cache.ttl, Duration::from_secs(config.token_cache_ttl_secs));
    assert_eq!(cache.capacity, config.token_cache_capacity);
    assert_eq!(cache.ttl, Duration::from_secs(300));
    assert_eq!(cache.capacity, 10_000);
}

// ---------------------------------------------------------------------------
// Cache TTL rules (ADR-0008)
// ---------------------------------------------------------------------------

#[test]
fn the_safety_margin_is_thirty_seconds() {
    assert_eq!(TOKEN_EXPIRY_SAFETY_MARGIN, Duration::from_secs(30));
}

#[test]
fn ttl_is_capped_by_the_configured_ttl() {
    let base = form_plugin(endpoint_config(FIRST_TOKEN_ENDPOINT));
    assert_eq!(
        base.cache_ttl_for(Duration::from_secs(3_600)),
        Some(Duration::from_secs(300))
    );
}

#[test]
fn ttl_is_reduced_by_the_safety_margin() {
    let base = form_plugin(endpoint_config(FIRST_TOKEN_ENDPOINT));
    assert_eq!(
        base.cache_ttl_for(Duration::from_secs(45)),
        Some(Duration::from_secs(15))
    );
}

#[test]
fn tokens_expiring_within_the_margin_are_not_cached() {
    let base = form_plugin(endpoint_config(FIRST_TOKEN_ENDPOINT));
    assert_eq!(base.cache_ttl_for(Duration::from_secs(30)), None);
    assert_eq!(base.cache_ttl_for(Duration::from_secs(29)), None);
    assert_eq!(base.cache_ttl_for(Duration::from_secs(0)), None);
}

#[test]
fn a_zero_configured_ttl_disables_the_cache() {
    let plugin = OAuth2ClientCredAuthPlugin::new(
        resolver(),
        toolkit_auth::ClientAuthMethod::Form,
        TokenCacheConfig {
            ttl: Duration::ZERO,
            capacity: 16,
        },
        &token_endpoint_config(&MockServer::start()),
    )
    .expect("plugin");
    assert_eq!(plugin.cache_ttl_for(Duration::from_secs(3_600)), None);
}

#[tokio::test]
async fn an_unavailable_credential_store_rejects_with_500() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok", 3_600));
    });
    let mut plugin = OAuth2ClientCredAuthPlugin::new(
        Arc::new(CredStoreSecretResolver::new(Arc::new(
            MockCredStoreClient::always_failing(),
        ))),
        toolkit_auth::ClientAuthMethod::Form,
        cache_config(),
        &token_endpoint_config(&server),
    )
    .expect("plugin");
    plugin.set_http_config(toolkit_http::HttpClientConfig::for_testing());
    let mut ctx = request();
    let error = plugin.authenticate(&mut ctx).await.expect_err("500");
    assert_eq!(
        error.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

#[tokio::test]
async fn form_credentials_travel_in_the_request_body() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/token")
            .body_includes("client_id=test-client")
            .body_includes("client_secret=test-secret");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-form", 3_600));
    });

    let mut ctx = request();
    form_plugin(token_endpoint_config(&server))
        .authenticate(&mut ctx)
        .await
        .expect("authenticated");
    mock.assert();
    assert_eq!(
        ctx.header(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer tok-form")
    );
    assert_eq!(ctx.injected_headers.len(), 1);
    assert_eq!(ctx.injected_headers[0].0.as_str(), AUTHORIZATION_HEADER);
}

#[tokio::test]
async fn basic_credentials_travel_in_the_authorization_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/token")
            .header("authorization", "Basic dGVzdC1jbGllbnQ6dGVzdC1zZWNyZXQ=");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-basic", 3_600));
    });

    let mut ctx = request();
    basic_plugin(token_endpoint_config(&server))
        .authenticate(&mut ctx)
        .await
        .expect("authenticated");
    mock.assert();
    assert_eq!(
        ctx.header(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer tok-basic")
    );
}

#[tokio::test]
async fn scopes_are_forwarded_to_the_token_endpoint() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/token")
            .body_includes("scope=read+write");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-scoped", 3_600));
    });

    let config = serde_json::json!({
        "token_endpoint": format!("http://localhost:{}/token", server.port()),
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
        "scopes": "read write"
    });
    let mut ctx = request();
    form_plugin(config)
        .authenticate(&mut ctx)
        .await
        .expect("authenticated");
    mock.assert();
}

#[tokio::test]
async fn a_second_request_is_served_from_the_cache() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-cached", 3_600));
    });

    let cache = form_plugin(token_endpoint_config(&server));
    let mut first = request();
    cache.authenticate(&mut first).await.expect("first");
    let mut second = request();
    cache.authenticate(&mut second).await.expect("second");
    mock.assert();
    assert_eq!(
        second
            .header(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer tok-cached")
    );
}

#[tokio::test]
async fn different_plugins_do_not_share_a_cached_token() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-isolated", 3_600));
    });

    let left = form_plugin(token_endpoint_config(&server));
    let mut right = form_plugin(token_endpoint_config(&server));
    right.set_http_config(toolkit_http::HttpClientConfig::for_testing());

    let mut first = request();
    left.authenticate(&mut first).await.expect("first");
    let mut second = request();
    right.authenticate(&mut second).await.expect("second");
    assert_eq!(
        mock.calls(),
        2,
        "each plugin instance fetches its own token"
    );
}

#[tokio::test]
async fn a_short_lived_token_is_not_cached() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-short", 20));
    });

    let cache = form_plugin(token_endpoint_config(&server));
    let mut first = request();
    cache.authenticate(&mut first).await.expect("first");
    let mut second = request();
    cache.authenticate(&mut second).await.expect("second");
    assert_eq!(
        mock.calls(),
        2,
        "tokens within the safety margin are never cached"
    );
}

#[tokio::test]
async fn a_failed_fetch_is_never_cached() {
    let server = MockServer::start();
    let mut failing = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(500).body("idp unavailable");
    });
    let succeeding = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-recovered", 3_600));
    });

    let cache = form_plugin(token_endpoint_config(&server));
    let mut first = request();
    let error = cache.authenticate(&mut first).await.expect_err("502");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(failing.calls(), 1);

    // Retire the failing mock so the retry reaches the succeeding one.
    failing.delete();
    let mut second = request();
    cache.authenticate(&mut second).await.expect("retried");
    assert_eq!(succeeding.calls(), 1);
}

#[tokio::test]
async fn an_idp_error_response_maps_to_502() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(400).body(r#"{"error":"invalid_client"}"#);
    });
    let mut ctx = request();
    let error = form_plugin(token_endpoint_config(&server))
        .authenticate(&mut ctx)
        .await
        .expect_err("502");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert!(
        error
            .detail()
            .contains("oauth2 token endpoint request failed")
    );
}

#[tokio::test]
async fn a_non_bearer_token_type_maps_to_502() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok","token_type":"mac"}"#);
    });
    let mut ctx = request();
    let error = form_plugin(token_endpoint_config(&server))
        .authenticate(&mut ctx)
        .await
        .expect_err("502");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_missing_security_context_rejects_with_401() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok", 3_600));
    });
    let mut ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(TENANT)
        .build();
    let error = form_plugin(token_endpoint_config(&server))
        .authenticate(&mut ctx)
        .await
        .expect_err("401");
    assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_inaccessible_credential_rejects_with_401() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok", 3_600));
    });
    let unavailable = Arc::new(StaticSecretResolver::new(HashMap::new()));
    let mut plugin = OAuth2ClientCredAuthPlugin::new(
        unavailable,
        toolkit_auth::ClientAuthMethod::Form,
        cache_config(),
        &token_endpoint_config(&server),
    )
    .expect("plugin");
    plugin.set_http_config(toolkit_http::HttpClientConfig::for_testing());
    let mut ctx = request();
    let error = plugin.authenticate(&mut ctx).await.expect_err("401");
    assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert!(error.detail().contains("client-id"));
}

#[tokio::test]
async fn oidc_discovery_resolves_the_token_endpoint() {
    let server = MockServer::start();
    let token_endpoint = format!("http://localhost:{}/oauth/token", server.port());
    let discovery = server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(r#"{{"token_endpoint":"{token_endpoint}"}}"#));
    });
    let exchange = server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-discovered", 3_600));
    });

    let config = serde_json::json!({
        "issuer_url": format!("http://localhost:{}", server.port()),
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    });
    let mut ctx = request();
    form_plugin(config)
        .authenticate(&mut ctx)
        .await
        .expect("authenticated");
    discovery.assert();
    exchange.assert();
    assert_eq!(
        ctx.header(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer tok-discovered")
    );
}

#[tokio::test]
async fn a_failed_discovery_maps_to_502() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(500).body("idp unavailable");
    });
    let config = serde_json::json!({
        "issuer_url": format!("http://localhost:{}", server.port()),
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    });
    let mut ctx = request();
    let error = form_plugin(config)
        .authenticate(&mut ctx)
        .await
        .expect_err("502");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn the_bearer_token_is_injected_exactly_once() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-once", 3_600));
    });
    let cache = form_plugin(token_endpoint_config(&server));
    let mut ctx = request();
    cache.authenticate(&mut ctx).await.expect("first");
    cache.authenticate(&mut ctx).await.expect("second");
    assert_eq!(ctx.injected_headers.len(), 1);
}
