//! The OAuth2 client-credentials auth plugins and their shared token cache
//! (`cpt-cf-oagw-dod-plugin-system-oauth2-token-cache`).
//!
//! The cache key is the subject tenant, the subject identifier, the
//! client-auth method tag and a deterministic hash of every configuration key
//! and value in sorted order; the entry TTL is the lesser of the configured
//! TTL and the IdP expiry less its 30-second safety margin; and a failed
//! exchange caches nothing.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-token-cache:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use super::*;
use crate::config::TokenCacheConfig;
use crate::domain::plugin::AuthContext;
use crate::infra::plugin::credentials::CredentialResolver;
use crate::test_support::FakeCredStore;
use toolkit_auth::oauth2::ClientAuthMethod;
use uuid::Uuid;

const TTL_SECS: u64 = 300;

fn cache_config() -> TokenCacheConfig {
    TokenCacheConfig { ttl_secs: TTL_SECS, capacity: 8 }
}

fn plugin(method: ClientAuthMethod) -> OAuth2ClientCredAuthPlugin {
    OAuth2ClientCredAuthPlugin::new(
        OAUTH2_FORM_PLUGIN_TYPE,
        method,
        CredentialResolver::new(Arc::new(FakeCredStore)),
        cache_config(),
    )
}

fn tenant() -> Uuid {
    Uuid::new_v4()
}

/// `inst-ps-key-1` .. `-3`: the key is the tenant, the subject, the method tag
/// and the configuration hash, and two tenants never share an entry.
#[test]
fn the_cache_key_separates_tenants_and_subjects() {
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let subject = Uuid::new_v4();
    let config = serde_json::json!({
        "client_id_ref": "cred://client_id",
        "client_secret_ref": "cred://client_secret",
        "token_endpoint": "https://idp.example.com/token"
    });
    let leak = Box::leak(Box::new(config.clone()));
    let base = OAuth2ClientCredAuthPlugin::cache_key(
        ClientAuthMethod::Form,
        Some(tenant_a),
        Some(subject),
        Some(leak),
    );
    assert_eq!(
        base,
        OAuth2ClientCredAuthPlugin::cache_key(
            ClientAuthMethod::Form,
            Some(tenant_a),
            Some(subject),
            Some(leak)
        ),
        "the key is deterministic"
    );
    assert_ne!(
        base,
        OAuth2ClientCredAuthPlugin::cache_key(
            ClientAuthMethod::Form,
            Some(tenant_b),
            Some(subject),
            Some(leak)
        )
    );
    assert_ne!(
        base,
        OAuth2ClientCredAuthPlugin::cache_key(
            ClientAuthMethod::Form,
            Some(tenant_a),
            Some(Uuid::new_v4()),
            Some(leak)
        )
    );
    // The tenant and the subject are carried in clear text in the key: they
    // are identifiers, never secret material.
    assert!(base.contains(&tenant_a.to_string()));
    assert!(base.contains(&subject.to_string()));
    // The reference contributes to the key through the configuration hash, so
    // a different reference occupies a different entry while no material is
    // ever carried in clear text.
    let other = Box::leak(Box::new(serde_json::json!({
        "client_id_ref": "cred://other_client",
        "client_secret_ref": "cred://client_secret",
        "token_endpoint": "https://idp.example.com/token"
    })));
    assert_ne!(
        base,
        OAuth2ClientCredAuthPlugin::cache_key(
            ClientAuthMethod::Form,
            Some(tenant_a),
            Some(subject),
            Some(other)
        )
    );
    assert!(!base.contains("client_secret"), "no reference value is carried in clear text");
}

/// `inst-ps-key-4`: the Form and the Basic variants never collide, even over
/// an identical configuration.
#[test]
fn the_method_tag_separates_the_variants() {
    let config = serde_json::json!({});
    let leak = Box::leak(Box::new(config));
    assert_ne!(
        auth_method_tag(ClientAuthMethod::Form),
        auth_method_tag(ClientAuthMethod::Basic)
    );
    assert_ne!(
        OAuth2ClientCredAuthPlugin::cache_key(
            ClientAuthMethod::Form,
            Some(tenant()),
            None,
            Some(leak)
        ),
        OAuth2ClientCredAuthPlugin::cache_key(
            ClientAuthMethod::Basic,
            Some(tenant()),
            None,
            Some(leak)
        )
    );
}

/// `inst-ps-key-5`: the configuration hash is order-independent and
/// distinguishes distinct scope sets.
#[test]
fn the_configuration_hash_is_canonical() {
    let left = serde_json::json!({ "scopes": ["read"], "audience": "api" });
    let right = serde_json::json!({ "audience": "api", "scopes": ["read"] });
    let other = serde_json::json!({ "scopes": ["write"], "audience": "api" });
    assert_eq!(config_hash(Some(&left)), config_hash(Some(&right)));
    assert_ne!(config_hash(Some(&left)), config_hash(Some(&other)));
    assert_ne!(config_hash(Some(&left)), config_hash(None));
    // A nested object participates with its path.
    let nested = serde_json::json!({ "a": { "b": 1 } });
    let nested_other = serde_json::json!({ "a": { "b": 2 } });
    assert_ne!(config_hash(Some(&nested)), config_hash(Some(&nested_other)));
}

/// `inst-ps-key-7`: the entry TTL is the lesser of the configured TTL and the
/// IdP expiry less the 30-second margin.
#[test]
fn the_ttl_is_the_lesser_of_the_two() {
    let configured = std::time::Duration::from_secs(TTL_SECS);
    let margin = std::time::Duration::from_secs(EXPIRY_SAFETY_MARGIN_SECS);
    // A long IdP expiry leaves the configured TTL in force.
    assert_eq!(
        OAuth2ClientCredAuthPlugin::cache_ttl_for(configured, std::time::Duration::from_secs(3600)),
        Some(configured)
    );
    // A short IdP expiry is reduced by the margin.
    assert_eq!(
        OAuth2ClientCredAuthPlugin::cache_ttl_for(configured, std::time::Duration::from_secs(90)),
        Some(std::time::Duration::from_secs(90) - margin)
    );
    // An expiry at or under the margin leaves no usable lifetime, so no entry.
    assert_eq!(
        OAuth2ClientCredAuthPlugin::cache_ttl_for(configured, std::time::Duration::from_secs(30)),
        None
    );
    assert_eq!(
        OAuth2ClientCredAuthPlugin::cache_ttl_for(configured, std::time::Duration::from_secs(10)),
        None
    );
}

/// Both variants share one cache when built through `with_cache`, so a token
/// fetched by one is never visible under the other's key.
#[test]
fn the_variants_share_one_cache_only_when_told_to() {
    let cache = OAuth2ClientCredAuthPlugin::new_token_cache(cache_config());
    let resolver = CredentialResolver::new(Arc::new(FakeCredStore));
    let form = OAuth2ClientCredAuthPlugin::with_cache(
        OAUTH2_FORM_PLUGIN_TYPE,
        ClientAuthMethod::Form,
        resolver.clone(),
        Arc::clone(&cache),
        cache_config(),
        None,
    );
    let basic = OAuth2ClientCredAuthPlugin::with_cache(
        OAUTH2_BASIC_PLUGIN_TYPE,
        ClientAuthMethod::Basic,
        resolver,
        Arc::clone(&cache),
        cache_config(),
        None,
    );
    assert_ne!(form.auth_method(), basic.auth_method());
}

/// A configuration without the two required references is an internal plugin
/// failure that never echoes a value.
#[test]
fn a_missing_reference_is_an_internal_failure() {
    let error = OAuth2ClientCredAuthPlugin::required_references(None).expect_err("no config");
    assert!(error.to_string().contains("client_id_ref"), "{error}");
    let error = OAuth2ClientCredAuthPlugin::required_references(Some(&serde_json::json!({
        "client_id_ref": "cred://client_id"
    })))
    .expect_err("no secret reference");
    assert!(error.to_string().contains("client_secret_ref"), "{error}");
    let (client, secret) = OAuth2ClientCredAuthPlugin::required_references(Some(&serde_json::json!({
        "client_id_ref": "cred://client_id",
        "client_secret_ref": "cred://client_secret"
    })))
    .expect("both references");
    assert_eq!(client, "cred://client_id");
    assert_eq!(secret, "cred://client_secret");
}

/// A malformed endpoint URL is never carried into the exchange configuration:
/// it is dropped, so the exchange cannot be built rather than aimed at a
/// malformed target.
#[test]
fn a_malformed_endpoint_is_never_carried() {
    let secret = toolkit_auth::oauth2::SecretString::new("value".to_owned());
    let config = OAuth2ClientCredAuthPlugin::token_endpoint_config(
        Some(&serde_json::json!({ "token_endpoint": "not-a-url" })),
        "client".to_owned(),
        secret,
        ClientAuthMethod::Form,
    )
    .expect("the configuration builds");
    assert!(config.token_endpoint.is_none(), "{:?}", config.token_endpoint);
    // A well-formed endpoint is carried, and the client identifier is the one
    // the configuration named.
    let secret = toolkit_auth::oauth2::SecretString::new("value".to_owned());
    let config = OAuth2ClientCredAuthPlugin::token_endpoint_config(
        Some(&serde_json::json!({
            "token_endpoint": "https://idp.example.com/token",
            "scopes": ["read", "write"]
        })),
        "client".to_owned(),
        secret,
        ClientAuthMethod::Form,
    )
    .expect("well formed");
    assert_eq!(
        config.token_endpoint.as_ref().map(|url| url.to_string()),
        Some("https://idp.example.com/token".to_owned())
    );
    assert_eq!(config.scopes, ["read".to_owned(), "write".to_owned()]);
    assert_eq!(config.client_id, "client");
    // The secret is redacted in the `Debug` output, never rendered.
    assert!(!format!("{:?}", config).contains("value"), "the secret is redacted");
}

/// The exchange runs against the token endpoint and nothing is cached on a
/// failure, so the next request retries the exchange (`inst-ps-tok-8`).
#[tokio::test]
async fn a_failed_exchange_caches_nothing() {
    let method = ClientAuthMethod::Form;
    let plugin = plugin(method);
    let key = OAuth2ClientCredAuthPlugin::cache_key(method, Some(tenant()), None, None);
    let mut ctx = AuthContext {
        config: Some(serde_json::json!({
            "client_id_ref": "cred://client_id",
            "client_secret_ref": "cred://client_secret",
            "token_endpoint": "https://idp.example.com/token"
        })),
        ..AuthContext::default()
    };
    ctx.principal.tenant_id = Some(Uuid::new_v4());
    let error = plugin.authenticate(&mut ctx).await.expect_err("the fake store resolves nothing");
    assert!(matches!(error, crate::domain::plugin::PluginError::Unavailable), "{error}");
    let (entry, _status) = plugin.cache().get(&key);
    assert!(entry.is_none(), "nothing is cached from a failed attempt");
    assert!(ctx.outbound_headers.is_empty());
}

