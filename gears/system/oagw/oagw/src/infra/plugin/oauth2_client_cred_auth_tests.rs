//! `OAuth2ClientCredAuthPlugin` tests (`ADR`-0008).
//!
//! The exchange itself sits behind [`TokenExchanger`], so the caching rules are
//! covered without a token endpoint; the real `fetch_token` round trip is
//! exercised in the `fips_free` module, which only compiles when the build
//! still allows plaintext mock servers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use toolkit_auth::oauth2::{
    ClientAuthMethod, FetchedToken, OAuthClientConfig, SecretString, TokenError,
};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::plugin::AuthPlugin;
use crate::infra::plugin::oauth2_client_cred_auth::{
    self, CachedToken, OAuth2ClientCredAuthPlugin, OAuth2PluginConfig, TokenCacheConfig,
    TokenExchanger,
};
use crate::infra::plugin::registry::{AuthPluginRegistry, token_cache};
use crate::infra::plugin::secrets::StaticSecretResolver;

const TENANT: uuid::Uuid = uuid::Uuid::nil();
const SUBJECT: uuid::Uuid = uuid::Uuid::nil();

/// A token endpoint double: canned lifetime, counted exchanges.
struct StubExchanger {
    expires_in: Duration,
    outcome: Result<String, TokenError>,
    calls: AtomicUsize,
    seen: std::sync::Mutex<Vec<OAuthClientConfig>>,
}

impl StubExchanger {
    fn serving(token: &'static str, expires_in: Duration) -> Arc<Self> {
        Arc::new(Self {
            expires_in,
            outcome: Ok(token.to_owned()),
            calls: AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            expires_in: Duration::from_mins(30),
            outcome: Err(TokenError::Http("endpoint down".to_owned())),
            calls: AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last(&self) -> OAuthClientConfig {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("an exchange was recorded")
    }
}

#[async_trait::async_trait]
impl TokenExchanger for StubExchanger {
    async fn exchange(&self, config: OAuthClientConfig) -> Result<FetchedToken, TokenError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(config);
        match &self.outcome {
            Ok(token) => Ok(FetchedToken {
                bearer: SecretString::new(token.clone()),
                expires_in: self.expires_in,
            }),
            Err(TokenError::Http(_)) => Err(TokenError::Http("endpoint down".to_owned())),
            Err(error) => Err(TokenError::Http(error.to_string())),
        }
    }
}

fn secrets() -> StaticSecretResolver {
    StaticSecretResolver::new(BTreeMap::from([
        ("client-id".to_owned(), "cid-1".to_owned()),
        ("client-secret".to_owned(), "s3cr3t".to_owned()),
    ]))
}

fn cache_config() -> TokenCacheConfig {
    TokenCacheConfig {
        ttl_secs: 300,
        capacity: 10,
    }
}

fn binding() -> serde_json::Value {
    json!({
        "client_id_ref": "client-id",
        "client_secret_ref": "client-secret",
        "token_endpoint": "https://idp.example.com/token"
    })
}

fn plugin_with(
    exchanger: Arc<dyn TokenExchanger>,
    auth_method: ClientAuthMethod,
    config: Option<&serde_json::Value>,
) -> OAuth2ClientCredAuthPlugin {
    OAuth2ClientCredAuthPlugin::with_exchanger(
        Arc::new(secrets()),
        auth_method,
        exchanger,
        token_cache(cache_config()),
        cache_config(),
        (TENANT, SUBJECT),
        config,
    )
    .unwrap()
}

fn context() -> crate::domain::dto::ProxyContext {
    crate::domain::dto::ProxyContext {
        alias: "partner-openai".to_owned(),
        method: "GET".to_owned(),
        path: "/v1/models".to_owned(),
        query: Vec::new(),
        headers: BTreeMap::new(),
        trace_id: Some("trace-1".to_owned()),
        tenant: TENANT,
        subject: SUBJECT,
    }
}

// ---------------------------------------------------------------- config parse

#[test]
fn config_requires_one_endpoint_and_both_references() {
    let error = OAuth2PluginConfig::parse(Some(&json!({"client_id_ref": "a"}))).unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));

    let both = json!({
        "client_id_ref": "a",
        "client_secret_ref": "b",
        "token_endpoint": "https://idp.example.com/token",
        "issuer_url": "https://idp.example.com"
    });
    let error = OAuth2PluginConfig::parse(Some(&both)).unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[test]
fn config_parses_scopes_keeping_the_binding_spelling() {
    let value = json!({
        "client_id_ref": "a",
        "client_secret_ref": "b",
        "token_endpoint": "https://idp.example.com/token",
        "scopes": "read  write"
    });
    let config = OAuth2PluginConfig::parse(Some(&value)).unwrap();
    assert_eq!(config.scopes, "read  write");
    assert_eq!(config.client_id_ref, "a");
    assert_eq!(config.issuer_url, None);
}

#[test]
fn config_rejects_a_malformed_url() {
    let value = json!({
        "client_id_ref": "a",
        "client_secret_ref": "b",
        "token_endpoint": "not a url"
    });
    let error = OAuth2PluginConfig::parse(Some(&value)).unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

// ------------------------------------------------------------- cache key / ttl

#[test]
fn cache_key_separates_tenant_subject_method_and_config() {
    let plugin = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    let key = plugin.cache_key();
    assert!(key.starts_with(&format!("{TENANT}:{SUBJECT}:form:")));

    let basic = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Basic,
        Some(&binding()),
    );
    assert_ne!(basic.cache_key(), key);

    let other_tenant = OAuth2ClientCredAuthPlugin::with_exchanger(
        Arc::new(secrets()),
        ClientAuthMethod::Form,
        StubExchanger::serving("tok", Duration::from_mins(30)),
        token_cache(cache_config()),
        cache_config(),
        (uuid::Uuid::now_v7(), SUBJECT),
        Some(&binding()),
    )
    .unwrap();
    assert_ne!(other_tenant.cache_key(), key);
}

#[test]
fn cache_key_tracks_the_binding_configuration() {
    let mut config = binding();
    let plugin = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Form,
        Some(&config),
    );
    let first = plugin.cache_key();
    config["scopes"] = json!("read");
    let second = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Form,
        Some(&config),
    )
    .cache_key();
    assert_ne!(first, second);
}

#[test]
fn ttl_is_the_configured_ceiling_when_the_token_outlives_it() {
    let plugin = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    assert_eq!(
        plugin.ttl_for(Duration::from_mins(30)),
        Some(Duration::from_mins(5))
    );
}

#[test]
fn ttl_shrinks_to_the_reported_lifetime_minus_the_margin() {
    let plugin = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    let ttl = plugin.ttl_for(Duration::from_secs(90));
    let expected = Duration::from_mins(1);
    assert_eq!(ttl, Some(expected));
}

#[test]
fn ttl_is_none_when_the_margin_consumes_the_lifetime() {
    let plugin = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    assert_eq!(plugin.ttl_for(Duration::from_secs(30)), None);
    assert_eq!(plugin.ttl_for(Duration::from_secs(0)), None);
}

#[test]
fn the_debug_output_names_the_binding_without_the_secret() {
    let plugin = plugin_with(
        StubExchanger::serving("tok", Duration::from_mins(30)),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    let rendered = format!("{plugin:?}");
    assert!(rendered.contains("OAuth2ClientCredAuthPlugin"));
    assert!(!rendered.contains("s3cr3t"));
}

// -------------------------------------------------------------- token fetching

#[tokio::test]
async fn fetch_exchanges_and_injects_a_bearer_token() {
    let exchanger = StubExchanger::serving("tok-1", Duration::from_mins(30));
    let plugin = plugin_with(
        Arc::<StubExchanger>::clone(&exchanger),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    let mut request = context();
    let outcome = plugin.authenticate(&mut request).await.unwrap();
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer tok-1")
    );
    assert_eq!(
        outcome
            .forwarded_headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer tok-1")
    );
    assert_eq!(
        outcome.subject.as_deref(),
        Some(SUBJECT.to_string().as_str())
    );
    assert_eq!(
        plugin.gts_id(),
        OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        "form bindings report the form id"
    );
    assert_eq!(exchanger.last().client_id, "cid-1");
    assert_eq!(exchanger.last().client_secret.expose(), "s3cr3t");
    assert_eq!(exchanger.last().auth_method, ClientAuthMethod::Form);
}

#[tokio::test]
async fn basic_bindings_report_the_basic_id_and_send_the_basic_method() {
    let exchanger = StubExchanger::serving("tok-basic", Duration::from_mins(30));
    let plugin = plugin_with(
        Arc::<StubExchanger>::clone(&exchanger),
        ClientAuthMethod::Basic,
        Some(&binding()),
    );
    assert_eq!(plugin.gts_id(), OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID);
    let mut request = context();
    plugin.authenticate(&mut request).await.unwrap();
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer tok-basic")
    );
    assert_eq!(exchanger.last().auth_method, ClientAuthMethod::Basic);
}

#[tokio::test]
async fn scopes_reach_the_exchange_as_separate_tokens() {
    let exchanger = StubExchanger::serving("tok", Duration::from_mins(30));
    let config = json!({
        "client_id_ref": "client-id",
        "client_secret_ref": "client-secret",
        "token_endpoint": "https://idp.example.com/token",
        "scopes": "read  write"
    });
    let plugin = plugin_with(
        Arc::<StubExchanger>::clone(&exchanger),
        ClientAuthMethod::Form,
        Some(&config),
    );
    plugin.fetch().await.unwrap();
    assert_eq!(
        exchanger.last().scopes,
        vec!["read".to_owned(), "write".to_owned()]
    );
}

#[tokio::test]
async fn a_second_fetch_is_served_from_the_cache() {
    let exchanger = StubExchanger::serving("tok-cached", Duration::from_mins(30));
    let plugin = plugin_with(
        Arc::<StubExchanger>::clone(&exchanger),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    let first = plugin.fetch().await.unwrap();
    let second = plugin.fetch().await.unwrap();
    assert_eq!(first, "tok-cached");
    assert_eq!(second, "tok-cached");
    assert_eq!(exchanger.call_count(), 1, "the exchange happens once");
}

#[tokio::test]
async fn a_shortlived_token_is_not_cached() {
    let exchanger = StubExchanger::serving("tok-brief", Duration::from_secs(20));
    let plugin = plugin_with(
        Arc::<StubExchanger>::clone(&exchanger),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    plugin.fetch().await.unwrap();
    plugin.fetch().await.unwrap();
    assert_eq!(
        exchanger.call_count(),
        2,
        "a token inside the margin is never cached"
    );
}

#[tokio::test]
async fn a_cache_entry_from_a_collided_key_is_rejected() {
    let exchanger = StubExchanger::serving("tok-fresh", Duration::from_mins(30));
    let plugin = plugin_with(
        Arc::<StubExchanger>::clone(&exchanger),
        ClientAuthMethod::Form,
        Some(&binding()),
    );
    let key = plugin.cache_key();
    plugin.cache_arc().put(
        &key,
        CachedToken {
            key: "gts.cf.core.oagw.auth_plugin.v1~other".to_owned(),
            token: SecretString::new("tok-poisoned"),
        },
        Some(Duration::from_mins(5)),
    );
    assert_eq!(plugin.fetch().await.unwrap(), "tok-fresh");
    assert_eq!(exchanger.call_count(), 1);
}

#[tokio::test]
async fn an_unresolvable_credential_is_reported_as_secret_not_found() {
    let exchanger = StubExchanger::serving("tok", Duration::from_mins(30));
    let config = json!({
        "client_id_ref": "absent-id",
        "client_secret_ref": "client-secret",
        "token_endpoint": "https://idp.example.com/token"
    });
    let plugin = plugin_with(exchanger, ClientAuthMethod::Form, Some(&config));
    let error = plugin.fetch().await.unwrap_err();
    assert!(matches!(error, DomainError::SecretNotFound { .. }));
}

#[tokio::test]
async fn a_rejecting_token_endpoint_is_service_unavailable() {
    let exchanger = StubExchanger::failing();
    let plugin = plugin_with(exchanger, ClientAuthMethod::Form, Some(&binding()));
    let error = plugin.fetch().await.unwrap_err();
    assert!(matches!(error, DomainError::ServiceUnavailable { .. }));
}

// ------------------------------------------------------------------- registry

#[test]
fn registry_resolves_both_oauth2_spellings() {
    let registry = AuthPluginRegistry::with_builtins(Arc::new(secrets()), None, cache_config());
    let form = registry
        .resolve_id(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            Some(&binding()),
            (TENANT, SUBJECT),
        )
        .unwrap();
    let basic = registry
        .resolve_id(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            Some(&binding()),
            (TENANT, SUBJECT),
        )
        .unwrap();
    assert_eq!(form.gts_id(), OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID);
    assert_eq!(basic.gts_id(), OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID);
}

#[test]
fn each_registry_owns_one_token_cache() {
    let first = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::default()),
        None,
        cache_config(),
    );
    let second = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::default()),
        None,
        cache_config(),
    );
    assert!(Arc::ptr_eq(&first.token_cache(), &first.token_cache()));
    assert!(!Arc::ptr_eq(&first.token_cache(), &second.token_cache()));
}

#[tokio::test]
async fn registry_auth_is_absent_when_the_upstream_declares_none() {
    let registry = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::default()),
        None,
        cache_config(),
    );
    let resolved = registry.resolve(None, (TENANT, SUBJECT)).unwrap();
    assert!(resolved.is_none());
}

#[test]
fn registry_refuses_an_auth_config_without_a_type() {
    let registry = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::default()),
        None,
        cache_config(),
    );
    let auth = crate::domain::model::AuthConfig {
        plugin_type: None,
        sharing: crate::domain::model::SharingMode::Private,
        config: None,
    };
    let error = registry
        .resolve(Some(&auth), (TENANT, SUBJECT))
        .err()
        .expect("an auth config without a type is refused");
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[test]
fn registry_maps_an_unresolvable_plugin_to_plugin_not_found() {
    let registry = AuthPluginRegistry::with_builtins(
        Arc::new(StaticSecretResolver::default()),
        None,
        cache_config(),
    );
    for unknown in [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~3f2c1b2a-0000-4000-8000-000000000001",
    ] {
        let error = registry
            .resolve_id(unknown, None, (TENANT, SUBJECT))
            .err()
            .unwrap_or_else(|| panic!("{unknown} should not resolve"));
        assert!(
            matches!(error, DomainError::PluginNotFound { .. }),
            "{unknown}"
        );
    }
}

#[test]
fn the_module_exposes_the_margin_and_the_auth_method_tags() {
    assert_eq!(oauth2_client_cred_auth::EXPIRY_MARGIN_SECS, 30);
    assert_eq!(
        oauth2_client_cred_auth::auth_method_tag(ClientAuthMethod::Form),
        "form"
    );
    assert_eq!(
        oauth2_client_cred_auth::auth_method_tag(ClientAuthMethod::Basic),
        "basic"
    );
}

/// The real `fetch_token` wiring against a plaintext mock server. Compiles only
/// when the build allows insecure transport: `--features fips` forbids it, so
/// these tests are skipped there and the port tests above carry the coverage.
#[cfg(not(feature = "fips"))]
mod fips_free {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn the_plugin_reaches_the_token_endpoint_through_toolkit_auth() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-wire","expires_in":1800,"token_type":"Bearer"}"#);
        });
        let config = json!({
            "client_id_ref": "client-id",
            "client_secret_ref": "client-secret",
            "token_endpoint": format!("http://localhost:{}/token", server.port())
        });
        let plugin = OAuth2ClientCredAuthPlugin::new(
            Arc::new(secrets()),
            ClientAuthMethod::Form,
            Some(toolkit_http::HttpClientConfig::for_testing()),
            token_cache(cache_config()),
            cache_config(),
            (TENANT, SUBJECT),
            Some(&config),
        )
        .unwrap();
        assert_eq!(plugin.fetch().await.unwrap(), "tok-wire");
        assert_eq!(
            plugin.fetch().await.unwrap(),
            "tok-wire",
            "the second read is cached"
        );
    }

    #[tokio::test]
    async fn the_default_exchanger_supplies_its_http_config() {
        let exchanger =
            FetchTokenExchanger::new(Some(toolkit_http::HttpClientConfig::for_testing()));
        let config = OAuthClientConfig {
            token_endpoint: Some(url::Url::parse("https://idp.example.com/token").unwrap()),
            client_id: "cid".to_owned(),
            client_secret: SecretString::new("secret"),
            ..OAuthClientConfig::default()
        };
        let error = exchanger.exchange(config).await.unwrap_err();
        assert!(!error.to_string().is_empty());
    }
}
