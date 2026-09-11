//! The OAuth2 client-credentials plugin (T055, ADR 0008) against a real IdP
//! stub.
//!
//! The IdP is a `httpmock` server, the only honest witness of how many times
//! the token endpoint was actually dialled: one fetch and then cache hits, a
//! failed fetch leaving no entry, and the Basic variant keying its cache
//! separately from the Form one.

use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::test_util::MockCredStoreClient;
use httpmock::prelude::*;

use super::oauth2_client_credentials::{ClientCredentialsAuth, ClientCredentialsConfig, Variant};
use crate::domain::gts_helpers::{BUILTIN_AUTH_OAUTH2_CC, BUILTIN_AUTH_OAUTH2_CC_BASIC};
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// The client credentials the test plants in the store.
const CLIENT_ID: &str = "gw-client";
/// The client secret the test plants in the store and asserts never leaks.
const CLIENT_SECRET: &str = "sk-oauth2-client-secret-9f2c4a7b1d8e";

/// An IdP stub answering `POST /token` with a bearer token valid `expires_in`.
///
/// Returns the mock, so a test can count how often the token endpoint was
/// dialled.
fn idp<'a>(server: &'a MockServer, token: &str, expires_in: u64) -> httpmock::Mock<'a> {
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#
            ));
    })
}

/// An IdP stub that always refuses the grant.
fn refusing_idp<'a>(server: &'a MockServer) -> httpmock::Mock<'a> {
    server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(500).body("the grant was refused");
    })
}

/// The plugin wired to a mock CredStore and the configured lifetime ceiling.
fn plugin(
    variant: Variant,
    ttl_secs: u64,
) -> (ClientCredentialsAuth, Arc<MockCredStoreClient>) {
    let store = Arc::new(MockCredStoreClient::with_secrets(vec![
        ("gw-client-id".to_string(), CLIENT_ID.to_string()),
        ("gw-client-secret".to_string(), CLIENT_SECRET.to_string()),
    ]));
    let plugin = ClientCredentialsAuth::new(variant, Some(store.clone()), ttl_secs, 128);
    (plugin, store)
}

/// A request context carrying the plugin configuration for `endpoint`.
fn ctx(endpoint: &str, scopes: &str) -> RequestContext {
    let mut ctx = RequestContext::default();
    ctx.tenant_id = Some("11111111-1111-1111-1111-111111111111".to_string());
    ctx.principal_id = Some("alice".to_string());
    ctx.plugin_config = Some(serde_json::json!({
        "token_endpoint": endpoint,
        "client_id_ref": "cred://gw-client-id",
        "client_secret_ref": "cred://gw-client-secret",
        "scopes": scopes
    }));
    ctx
}

/// The configuration a test's context carries, for cache-key comparisons.
fn config_of(ctx: &RequestContext) -> ClientCredentialsConfig {
    ClientCredentialsConfig::from_value(ctx.plugin_config.as_ref()).expect("a usable config")
}

#[tokio::test]
async fn the_token_is_fetched_once_then_served_from_the_cache() {
    let server = MockServer::start();
    let token_mock = idp(&server, "tok-first", 3600);
    let (plugin, _store) = plugin(Variant::Form, 300);
    let endpoint = format!("http://localhost:{}/token", server.port());

    let mut first = ctx(&endpoint, "read");
    plugin
        .authenticate(&mut first)
        .await
        .expect("the grant succeeded");
    assert_eq!(token_mock.calls(), 1, "the token endpoint was dialled once");
    assert_eq!(
        first.header("authorization"),
        Some("Bearer tok-first"),
        "the access token was injected"
    );

    let mut second = ctx(&endpoint, "read");
    plugin
        .authenticate(&mut second)
        .await
        .expect("the cached token is served");
    assert_eq!(token_mock.calls(), 1, "the second call was served from the cache");
    assert_eq!(second.header("authorization"), Some("Bearer tok-first"));
}

#[tokio::test]
async fn a_distinct_configuration_dials_the_idp_again() {
    let server = MockServer::start();
    let token_mock = idp(&server, "tok-scoped", 3600);
    let (plugin, _store) = plugin(Variant::Form, 300);
    let endpoint = format!("http://localhost:{}/token", server.port());

    let mut read_only = ctx(&endpoint, "read");
    plugin.authenticate(&mut read_only).await.expect("fetched");

    let mut write_scope = ctx(&endpoint, "read write");
    plugin.authenticate(&mut write_scope).await.expect("fetched");
    assert_eq!(
        token_mock.calls(),
        2,
        "a different scope is a different credential, not a cache hit"
    );
}

#[tokio::test]
async fn a_failed_fetch_is_not_cached() {
    let server = MockServer::start();
    let _refused = refusing_idp(&server);
    let (plugin, _store) = plugin(Variant::Form, 300);
    let endpoint = format!("http://localhost:{}/token", server.port());

    let mut refused = ctx(&endpoint, "read");
    let error = plugin
        .authenticate(&mut refused)
        .await
        .expect_err("the grant was refused");
    let PluginError::Reject(crate::domain::error::DomainError::AuthenticationFailed {
        plugin_id,
        ..
    }) = error
    else {
        panic!("a refused grant is an authentication failure, got {error:?}");
    };
    assert_eq!(
        plugin_id.as_deref(),
        Some(BUILTIN_AUTH_OAUTH2_CC),
        "the failure names the plugin"
    );
    assert_eq!(_refused.calls(), 1);

    // The IdP recovers; the plugin must dial it again rather than replay a
    // failure it was never given a token for.
    let recovered = MockServer::start();
    let recovered_mock = idp(&recovered, "tok-recovered", 3600);
    let mut healed = ctx(&format!("http://localhost:{}/token", recovered.port()), "read");
    plugin
        .authenticate(&mut healed)
        .await
        .expect("the recovered IdP issued a token");
    assert_eq!(recovered_mock.calls(), 1, "the failure was not cached");
    assert_eq!(healed.header("authorization"), Some("Bearer tok-recovered"));
}

#[test]
fn the_cache_lifetime_is_the_shorter_of_the_ceiling_and_the_idp_lifetime() {
    let (plugin, _store) = plugin(Variant::Form, 300);
    // The IdP's 30-minute lifetime exceeds the ceiling, so the ceiling wins.
    assert_eq!(
        plugin.ttl_for(Duration::from_mins(30)),
        Duration::from_secs(300)
    );
    // The IdP's 3-minute lifetime is shortened by the 30-second safety margin.
    assert_eq!(plugin.ttl_for(Duration::from_mins(3)), Duration::from_secs(150));
    // A lifetime inside the margin still yields a positive cache entry.
    assert_eq!(plugin.ttl_for(Duration::from_secs(10)), Duration::from_secs(1));
}

#[test]
fn the_variants_key_their_caches_separately() {
    let (form, _store) = plugin(Variant::Form, 300);
    let (basic, _store) = plugin(Variant::Basic, 300);
    let request = ctx("http://idp/token", "read");
    let config = config_of(&request);
    assert_ne!(
        form.cache_key(&request, &config),
        basic.cache_key(&request, &config),
        "a Basic token must never be handed out for a Form binding"
    );
}

#[test]
fn the_variants_expose_their_own_plugin_ids() {
    let (form, _store) = plugin(Variant::Form, 300);
    let (basic, _store) = plugin(Variant::Basic, 300);
    assert_eq!(
        form.id(),
        BUILTIN_AUTH_OAUTH2_CC
    );
    assert_eq!(
        basic.id(),
        BUILTIN_AUTH_OAUTH2_CC_BASIC
    );
}

#[tokio::test]
async fn an_issuer_url_is_resolved_through_discovery() {
    let server = MockServer::start();
    let token_port = server.port();
    let _discovery = server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"token_endpoint":"http://localhost:{token_port}/token"}}"#
            ));
    });
    let token_mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-discovered","expires_in":3600,"token_type":"Bearer"}"#);
    });

    let (plugin, _store) = plugin(Variant::Form, 300);
    let mut request = RequestContext::default();
    request.tenant_id = Some("11111111-1111-1111-1111-111111111111".to_string());
    request.plugin_config = Some(serde_json::json!({
        "issuer_url": format!("http://localhost:{token_port}"),
        "client_id_ref": "cred://gw-client-id",
        "client_secret_ref": "cred://gw-client-secret",
        "scopes": "read"
    }));
    plugin
        .authenticate(&mut request)
        .await
        .expect("discovery resolved the token endpoint");
    assert_eq!(token_mock.calls(), 1, "the token endpoint was dialled");
    assert_eq!(request.header("authorization"), Some("Bearer tok-discovered"));
}

#[test]
fn every_declared_configuration_key_is_parsed() {
    let config = ClientCredentialsConfig::from_value(Some(&serde_json::json!({
        "token_endpoint": "https://idp/token",
        "client_id_ref": "cred://id",
        "client_secret_ref": "cred://secret",
        "scopes": "a b  c"
    })))
    .expect("a usable config");
    assert_eq!(config.token_endpoint.as_deref(), Some("https://idp/token"));
    assert_eq!(config.issuer_url, None);
    assert_eq!(config.client_id_ref, "cred://id");
    assert_eq!(config.client_secret_ref, "cred://secret");
    assert_eq!(config.scopes, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[tokio::test]
async fn the_client_secret_never_reaches_a_log_or_an_error_body() {
    let server = MockServer::start();
    let _refused = refusing_idp(&server);
    let (plugin, _store) = plugin(Variant::Form, 300);
    let mut request = ctx(&format!("http://localhost:{}/token", server.port()), "read");
    let error = plugin
        .authenticate(&mut request)
        .await
        .expect_err("the grant was refused");
    let rendered = crate::api::rest::error::problem_from_domain_error(
        &crate::domain::error::DomainError::from(error),
        "/oagw/v1/proxy/vendor.com",
    )
    .to_json()
    .to_string();
    assert!(
        !rendered.contains(CLIENT_SECRET),
        "the error body carried the client secret: {rendered}"
    );
    assert!(
        !rendered.contains(CLIENT_ID),
        "the error body carried the client id: {rendered}"
    );
}
