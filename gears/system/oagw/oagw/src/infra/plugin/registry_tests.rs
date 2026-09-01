//! Registry tests: a stored reference becomes a plugin, an unknown one is `503`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use serde_json::json;

use crate::domain::error::DomainError;
use crate::domain::error::OagwErrorType;
use crate::domain::gts_helpers::{
    API_KEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::model::{AuthConfig, SharingMode};
use crate::infra::plugin::oauth2_client_cred_auth::CachedToken;
use crate::infra::plugin::oauth2_client_cred_auth::{OAuth2PluginConfig, TokenCacheConfig};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry, token_cache,
};
use crate::infra::plugin::secrets::StaticSecretResolver;

const IDENTITY: (uuid::Uuid, uuid::Uuid) = (uuid::Uuid::nil(), uuid::Uuid::nil());

#[test]
fn the_default_token_cache_config_is_the_documented_one() {
    let config = TokenCacheConfig::default();
    assert_eq!(config.ttl_secs, 300);
    assert_eq!(config.capacity, 10_000);
}

#[test]
fn token_cache_honours_the_configured_capacity() {
    let cache = token_cache(TokenCacheConfig {
        ttl_secs: 300,
        capacity: 1,
    });
    let entry = |key: &str| CachedToken {
        key: key.to_owned(),
        token: toolkit_auth::oauth2::SecretString::new(key),
    };
    cache.put("a", entry("a"), Some(std::time::Duration::from_secs(1)));
    cache.put("b", entry("b"), Some(std::time::Duration::from_secs(1)));
    assert!(cache.get("a").0.is_none(), "the oldest entry is evicted");
    assert!(cache.get("b").0.is_some());
}

#[test]
fn auth_registry_resolves_the_noop_and_apikey_builtins() {
    let registry = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::single("k", "k")),
        None,
        TokenCacheConfig::default(),
    );
    assert_eq!(
        registry
            .resolve_id(NOOP_AUTH_PLUGIN_ID, None, IDENTITY)
            .unwrap()
            .gts_id(),
        NOOP_AUTH_PLUGIN_ID
    );
    let binding = json!({"value_ref": "k"});
    assert_eq!(
        registry
            .resolve_id(API_KEY_AUTH_PLUGIN_ID, Some(&binding), IDENTITY)
            .unwrap()
            .gts_id(),
        API_KEY_AUTH_PLUGIN_ID
    );
}

#[tokio::test]
async fn auth_registry_binds_the_caller_identity_into_the_plugin() {
    let registry = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::single("k", "k")),
        None,
        TokenCacheConfig::default(),
    );
    let auth = AuthConfig {
        plugin_type: Some(API_KEY_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Private,
        config: Some(json!({"value_ref": "k"})),
    };
    let resolved = registry.resolve(Some(&auth), IDENTITY).unwrap();
    let plugin = resolved.expect("the apikey plugin resolves");
    let mut request = crate::domain::dto::ProxyContext {
        alias: "a".to_owned(),
        method: "GET".to_owned(),
        path: "/".to_owned(),
        query: Vec::new(),
        headers: std::collections::BTreeMap::new(),
        trace_id: None,
        tenant: IDENTITY.0,
        subject: IDENTITY.1,
    };
    let outcome = plugin.authenticate(&mut request).await.unwrap();
    assert_eq!(
        request.headers.get("x-api-key").map(String::as_str),
        Some("k")
    );
    assert_eq!(
        outcome.subject.as_deref(),
        Some(IDENTITY.1.to_string().as_str())
    );
}

#[test]
fn auth_registry_carries_the_oauth2_config_into_the_binding() {
    let registry = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::single("s", "v")),
        None,
        TokenCacheConfig::default(),
    );
    let auth = AuthConfig {
        plugin_type: Some(crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Private,
        config: Some(json!({
            "client_id_ref": "c",
            "client_secret_ref": "s",
            "token_endpoint": "https://idp.example.com/token"
        })),
    };
    let resolved = registry.resolve(Some(&auth), IDENTITY).unwrap();
    let plugin = resolved.expect("the oauth2 plugin resolves");
    assert_eq!(
        plugin.gts_id(),
        crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID
    );
    assert!(OAuth2PluginConfig::parse(Some(auth.config.as_ref().unwrap())).is_ok());
}

#[test]
fn guard_registry_resolves_required_headers_and_refuses_the_catalog_only_ids() {
    let guard = GuardPluginRegistry::resolve(
        REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        Some(&json!({"required_request_headers": "x-a"})),
    )
    .unwrap();
    assert_eq!(guard.gts_id(), REQUIRED_HEADERS_GUARD_PLUGIN_ID);
    for unknown in [
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        "gts.cf.core.oagw.guard_plugin.v1~3f2c1b2a-0000-4000-8000-000000000001",
    ] {
        let error = GuardPluginRegistry::resolve(unknown, None)
            .err()
            .unwrap_or_else(|| panic!("{unknown} should not resolve"));
        assert!(
            matches!(error, DomainError::PluginNotFound { .. }),
            "{unknown}"
        );
    }
}

#[test]
fn transform_registry_resolves_request_id_and_refuses_the_catalog_only_ids() {
    let transform = TransformPluginRegistry::resolve(REQUEST_ID_TRANSFORM_PLUGIN_ID, None).unwrap();
    assert_eq!(transform.gts_id(), REQUEST_ID_TRANSFORM_PLUGIN_ID);
    for unknown in [
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ] {
        let error = TransformPluginRegistry::resolve(unknown, None)
            .err()
            .unwrap_or_else(|| panic!("{unknown} should not resolve"));
        assert!(
            matches!(error, DomainError::PluginNotFound { .. }),
            "{unknown}"
        );
    }
}

#[test]
fn plugin_not_found_is_a_proxy_error_with_a_503_status() {
    let error = DomainError::plugin_not_found("gts.cf.core.oagw.guard_plugin.v1~x");
    assert!(matches!(error, DomainError::PluginNotFound { .. }));
    assert_eq!(OagwErrorType::PluginNotFound.status(), 503);
    assert!(OagwErrorType::PluginNotFound.is_proxy_error());
}
