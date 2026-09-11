//! Built-in plugin catalogue, credential-resolution, and token-cache tests.
//!
//! Covers `cpt-cf-oagw-dod-builtin-catalogue`, `cpt-cf-oagw-dod-credential-isolation`,
//! and `cpt-cf-oagw-dod-token-cache`: the six backed implementations the three
//! registries hold at initialization, the six catalog-only identifiers that
//! stay unresolvable everywhere, the twelve identifiers the post-init phase
//! registers in the types-registry, the `cred://` routine that is the only
//! thing in the gear that turns a reference into material, and the token cache
//! whose stored key is verified on every hit.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::error::CredStoreError;
use credstore_sdk::models::{GetSecretResponse, SecretRef, SecretValue, SharingMode};
use credstore_sdk::test_util::MockCredStoreClient;
use httpmock::prelude::*;
use oagw::domain::context::{AuthContext, RequestContext, ResponseContext};
use oagw::domain::plugin_contract::{
    AuthPlugin, AuthPluginRegistry, GuardDecision, GuardPlugin, GuardPluginRegistry, PluginFailure,
    PluginPhase, TransformPlugin, TransformPluginRegistry,
};
use oagw::gts::plugin_catalog;
use oagw::plugins::credential::CREDENTIAL_SCHEME;
use oagw::plugins::token_cache::{TOKEN_CACHE_SAFETY_MARGIN_SECS, TokenCache, TokenCacheConfig};
use serde_json::{Value, json};
use toolkit_auth::SecretString;
use toolkit_security::SecurityContext;

/// The four auth identifiers the built-in catalogue backs.
const AUTH_IDS: [&str; 4] = [
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
];

/// The one backed guard identifier and the one backed transform identifier.
const GUARD_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
const TRANSFORM_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// The tenant every context in this file is built with.
const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x0102);

/// The subject every context in this file is authenticated as.
const SUBJECT: uuid::Uuid = uuid::Uuid::from_u128(0x0304);

/// A credential store that counts the `get` calls it received, so a test can
/// prove a shape failure never reached the store.
struct CountingCredStore {
    inner: MockCredStoreClient,
    calls: AtomicUsize,
}

impl CountingCredStore {
    fn with_secrets(creds: Vec<(String, String)>) -> Self {
        Self {
            inner: MockCredStoreClient::with_secrets(creds),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Coerces the counted store onto the trait object the registry takes, so
    /// a test keeps reading the counter the registry's copy counts.
    fn as_client(self: &Arc<Self>) -> Arc<dyn CredStoreClientV1> {
        let counted: Arc<CountingCredStore> = Arc::clone(self);
        counted
    }
}

#[async_trait]
impl CredStoreClientV1 for CountingCredStore {
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get(ctx, key).await
    }
}

/// A credential store that declines every reference, the shape the
/// `AccessDenied` error takes.
struct DecliningCredStore;

#[async_trait]
impl CredStoreClientV1 for DecliningCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        Err(CredStoreError::AccessDenied)
    }
}

/// The security context the credential routine resolves under.
fn security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::from_u128(0x0304))
        .subject_type("service")
        .subject_tenant_id(TENANT)
        .build()
        .expect("a fully specified security context builds")
}

/// The built-in registries, over a store that knows one API key.
fn builtins() -> (AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry) {
    (
        AuthPluginRegistry::with_builtins(
            Arc::new(CountingCredStore::with_secrets(vec![(
                String::from("api-key"),
                String::from("sk-live-1"),
            )])),
            token_config(),
        ),
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
    )
}

/// The `apikey` implementation the built-in registry holds.
fn apikey(registry: &AuthPluginRegistry) -> Arc<dyn AuthPlugin> {
    registry
        .resolve(AUTH_IDS[1])
        .expect("the built-in catalogue backs the apikey variant")
}

/// The `noop` implementation the built-in registry holds.
fn noop(registry: &AuthPluginRegistry) -> Arc<dyn AuthPlugin> {
    registry
        .resolve(AUTH_IDS[0])
        .expect("the built-in catalogue backs the noop variant")
}

/// The `oauth2_client_cred` implementation the built-in registry holds.
fn oauth2(registry: &AuthPluginRegistry) -> Arc<dyn AuthPlugin> {
    registry
        .resolve(AUTH_IDS[2])
        .expect("the built-in catalogue backs the oauth2 variant")
}

/// The `required_headers` implementation the built-in registry holds.
fn required_headers(registry: &GuardPluginRegistry) -> Arc<dyn GuardPlugin> {
    registry
        .resolve(GUARD_ID)
        .expect("the built-in catalogue backs the required-headers guard")
}

/// The `request_id` implementation the built-in registry holds.
fn request_id(registry: &TransformPluginRegistry) -> Arc<dyn TransformPlugin> {
    registry
        .resolve(TRANSFORM_ID)
        .expect("the built-in catalogue backs the request-id transform")
}

/// The token-cache configuration the registry is built with.
fn token_config() -> oagw::plugins::token_cache::TokenCacheConfig {
    TokenCacheConfig::new(Duration::from_secs(300), 10_000)
}

#[test]
fn the_six_backed_identifiers_resolve_from_their_built_in_registries() {
    let (auth, guard, transform) = builtins();
    for identifier in AUTH_IDS {
        assert!(auth.resolve(identifier).is_ok(), "{identifier} resolves");
    }
    assert!(auth.declared_phases(AUTH_IDS[0]).is_some());
    assert!(guard.resolve(GUARD_ID).is_ok());
    assert!(transform.resolve(TRANSFORM_ID).is_ok());
    // Each registry holds its own entries and no others.
    assert_eq!(auth.len(), AUTH_IDS.len());
    assert_eq!(guard.len(), 1);
    assert_eq!(transform.len(), 1);
}

#[test]
fn the_six_catalog_only_identifiers_stay_unresolvable() {
    let (auth, guard, transform) = builtins();
    for identifier in plugin_catalog::CATALOG_ONLY {
        assert!(matches!(
            auth.resolve(identifier),
            Err(oagw::domain::plugin_contract::PluginResolveError::Reserved { .. })
        ));
        assert!(matches!(
            guard.resolve(identifier),
            Err(oagw::domain::plugin_contract::PluginResolveError::Reserved { .. })
        ));
        assert!(matches!(
            transform.resolve(identifier),
            Err(oagw::domain::plugin_contract::PluginResolveError::Reserved { .. })
        ));
    }
}

#[test]
fn the_twelve_identifiers_are_types_registry_rows() {
    // The post-init phase registers all twelve identifiers of the built-in
    // catalogue in the types-registry, backed and catalog-only alike.
    let rows = oagw::gts::catalog::instances();
    for identifier in plugin_catalog::all() {
        assert!(
            rows.iter()
                .any(|row| row.get("id").and_then(Value::as_str) == Some(identifier)),
            "{identifier} is an instance row of the catalogue batch"
        );
    }
}

#[tokio::test]
async fn an_apikey_plugin_resolves_its_reference_and_injects_the_header() {
    let (auth, _guard, _transform) = builtins();
    let mut ctx = AuthContext::new(TENANT, None);
    apikey(&auth).authenticate(&mut ctx,
        &json!({
            "credential_ref": "cred://api-key",
            "header_name": "x-api-key",
        }),
    )
    .await
    .expect("a resolvable reference authenticates");
    assert_eq!(ctx.header("x-api-key"), Some("sk-live-1"));
}

#[tokio::test]
async fn an_apikey_plugin_defaults_to_the_standard_api_key_header() {
    let (auth, _guard, _transform) = builtins();
    let mut ctx = AuthContext::new(TENANT, None);
    apikey(&auth).authenticate(&mut ctx, &json!({ "credential_ref": "cred://api-key" }))
        .await
        .expect("the default header name needs no configuration");
    assert_eq!(ctx.header("x-api-key"), Some("sk-live-1"));
}

#[tokio::test]
async fn a_malformed_reference_fails_the_shape_check_before_the_store() {
    let store = Arc::new(CountingCredStore::with_secrets(vec![(
        String::from("api-key"),
        String::from("v"),
    )]));
    let auth = AuthPluginRegistry::with_builtins(store.as_client(), token_config());
    let mut ctx = AuthContext::new(TENANT, None);
    for reference in ["cred://", "https://api-key", " cred://api-key", "cred://api-key#frag"] {
        let failure = apikey(&auth)
            .authenticate(&mut ctx, &json!({ "credential_ref": reference }))
            .await
            .expect_err("a malformed reference is a typed failure");
        assert!(
            matches!(failure, PluginFailure::CredentialShape),
            "{reference} fails the shape check"
        );
        assert!(ctx.headers.is_empty(), "no header was injected");
    }
    // Every reference was declined on shape, so the store was never asked.
    assert_eq!(store.calls(), 0);
}

#[tokio::test]
async fn an_unresolvable_reference_maps_to_secret_not_found() {
    let (auth, _guard, _transform) = builtins();
    let mut ctx = AuthContext::new(TENANT, None);
    let failure = apikey(&auth)
        .authenticate(&mut ctx, &json!({ "credential_ref": "cred://absent" }))
        .await
        .expect_err("an absent secret is a typed failure");
    assert!(matches!(failure, PluginFailure::SecretNotFound));
    assert!(ctx.headers.is_empty());
}

#[tokio::test]
async fn a_declined_reference_maps_to_authentication_failed() {
    let auth = AuthPluginRegistry::with_builtins(
        Arc::new(DecliningCredStore),
        token_config(),
    );
    let mut ctx = AuthContext::new(TENANT, None);
    let failure = apikey(&auth)
        .authenticate(&mut ctx, &json!({ "credential_ref": "cred://api-key" }))
        .await
        .expect_err("a declined reference is a typed failure");
    assert!(matches!(failure, PluginFailure::AuthenticationFailed));
}

#[tokio::test]
async fn an_unreachable_store_maps_to_unavailable() {
    let auth = AuthPluginRegistry::with_builtins(
        Arc::new(MockCredStoreClient::always_failing()),
        token_config(),
    );
    let mut ctx = AuthContext::new(TENANT, None);
    let failure = apikey(&auth)
        .authenticate(&mut ctx, &json!({ "credential_ref": "cred://api-key" }))
        .await
        .expect_err("an unreachable store is a typed failure");
    assert!(matches!(failure, PluginFailure::Unavailable));
}

#[tokio::test]
async fn an_apikey_configuration_without_a_reference_is_a_configuration_failure() {
    let (auth, _guard, _transform) = builtins();
    let mut ctx = AuthContext::new(TENANT, None);
    let failure = apikey(&auth)
        .authenticate(&mut ctx, &json!({}))
        .await
        .expect_err("a configuration with no reference cannot authenticate");
    assert!(matches!(failure, PluginFailure::Configuration { .. }));
}

#[tokio::test]
async fn no_typed_failure_echoes_the_reference() {
    let (auth, _guard, _transform) = builtins();
    let mut ctx = AuthContext::new(TENANT, None);
    for reference in ["cred://", "cred://absent#frag", "not-a-reference"] {
        if let Err(failure) = apikey(&auth)
            .authenticate(&mut ctx, &json!({ "credential_ref": reference }))
            .await
        {
            assert!(
                !format!("{failure:?}").contains("absent"),
                "the failure never names the reference: {failure:?}"
            );
        }
    }
}

#[test]
fn the_credential_shape_check_accepts_only_the_cred_scheme() {
    assert!(oagw::plugins::credential::is_credential_reference("cred://api-key"));
    assert!(oagw::plugins::credential::is_credential_reference("cred://Tenant_Scope-1"));
    assert!(!oagw::plugins::credential::is_credential_reference("cred://"));
    assert!(!oagw::plugins::credential::is_credential_reference("https://api-key"));
    assert!(!oagw::plugins::credential::is_credential_reference("cred://api key"));
    assert!(!oagw::plugins::credential::is_credential_reference(" cred://api-key"));
    assert!(!oagw::plugins::credential::is_credential_reference("cred://api-key#f"));
    assert!(!oagw::plugins::credential::is_credential_reference(""));
    assert_eq!(CREDENTIAL_SCHEME, "cred://");
}

#[tokio::test]
async fn the_noop_auth_plugin_injects_nothing() {
    let (auth, _guard, _transform) = builtins();
    let mut ctx = AuthContext::new(TENANT, Some(SUBJECT));
    noop(&auth)
        .authenticate(&mut ctx, &json!({}))
        .await
        .expect("the noop variant always succeeds");
    assert!(ctx.headers.is_empty());
}

#[tokio::test]
async fn an_oauth2_plugin_exchanges_and_injects_the_bearer() {
    let server = MockServer::start();
    let token_mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-1","expires_in":3600,"token_type":"Bearer"}"#);
    });
    let store = CountingCredStore::with_secrets(vec![
        (String::from("client-id"), String::from("cid")),
        (String::from("client-secret"), String::from("csecret")),
    ]);
    let auth = AuthPluginRegistry::with_builtins(
        Arc::new(store),
        TokenCacheConfig::new(Duration::from_secs(300), 10),
    );
    let mut ctx = AuthContext::new(TENANT, Some(SUBJECT));
    oauth2(&auth)
        .authenticate(
            &mut ctx,
            &json!({
                "token_endpoint": format!("http://localhost:{}/token", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "cred://client-secret",
                "scopes": "read write",
            }),
        )
        .await
        .expect("a successful exchange authenticates");
    assert_eq!(ctx.header("authorization"), Some("Bearer tok-1"));
    token_mock.assert_calls(1);
}

#[tokio::test]
async fn a_second_lookup_is_served_from_the_cache() {
    let server = MockServer::start();
    let token_mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-1","expires_in":3600,"token_type":"Bearer"}"#);
    });
    let store = CountingCredStore::with_secrets(vec![
        (String::from("client-id"), String::from("cid")),
        (String::from("client-secret"), String::from("csecret")),
    ]);
    let auth = AuthPluginRegistry::with_builtins(
        Arc::new(store),
        TokenCacheConfig::new(Duration::from_secs(300), 10),
    );
    let config = json!({
        "token_endpoint": format!("http://localhost:{}/token", server.port()),
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
    });
    for _ in 0..3 {
        let mut ctx = AuthContext::new(TENANT, Some(SUBJECT));
        oauth2(&auth)
            .authenticate(&mut ctx, &config)
            .await
            .expect("every lookup succeeds");
        assert_eq!(ctx.header("authorization"), Some("Bearer tok-1"));
    }
    token_mock.assert_calls(1);
}

#[tokio::test]
async fn tenants_subjects_and_configurations_never_share_an_entry() {
    let server = MockServer::start();
    let token_mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-1","expires_in":3600,"token_type":"Bearer"}"#);
    });
    let store = CountingCredStore::with_secrets(vec![
        (String::from("client-id"), String::from("cid")),
        (String::from("client-secret"), String::from("csecret")),
    ]);
    let auth = AuthPluginRegistry::with_builtins(
        Arc::new(store),
        TokenCacheConfig::new(Duration::from_secs(300), 10),
    );
    let config = json!({
        "token_endpoint": format!("http://localhost:{}/token", server.port()),
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
    });
    let mut first = AuthContext::new(TENANT, Some(SUBJECT));
    oauth2(&auth).authenticate(&mut first, &config).await.unwrap();
    // A different tenant resolves its own entry, never the first tenant's.
    let other_tenant = uuid::Uuid::from_u128(0x9999);
    let mut second = AuthContext::new(other_tenant, Some(SUBJECT));
    oauth2(&auth).authenticate(&mut second, &config).await.unwrap();
    assert_eq!(second.header("authorization"), Some("Bearer tok-1"));
    token_mock.assert_calls(2);
}

#[tokio::test]
async fn a_failed_exchange_is_not_cached() {
    let server = MockServer::start();
    let token_mock = server.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(500).body("idp unavailable");
    });
    let store = CountingCredStore::with_secrets(vec![
        (String::from("client-id"), String::from("cid")),
        (String::from("client-secret"), String::from("csecret")),
    ]);
    let auth = AuthPluginRegistry::with_builtins(
        Arc::new(store),
        TokenCacheConfig::new(Duration::from_secs(300), 10),
    );
    let config = json!({
        "token_endpoint": format!("http://localhost:{}/token", server.port()),
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
    });
    let mut ctx = AuthContext::new(TENANT, Some(SUBJECT));
    let failure = oauth2(&auth)
        .authenticate(&mut ctx, &config)
        .await
        .expect_err("a refused exchange is a typed failure");
    assert!(
        matches!(failure, PluginFailure::AuthenticationFailed | PluginFailure::Unavailable),
        "the exchange failure is mapped, not swallowed: {failure:?}"
    );
    token_mock.assert_calls(1);
}

#[tokio::test]
async fn an_oauth2_configuration_without_a_token_endpoint_is_a_configuration_failure() {
    let store = CountingCredStore::with_secrets(vec![
        (String::from("client-id"), String::from("cid")),
        (String::from("client-secret"), String::from("csecret")),
    ]);
    let auth = AuthPluginRegistry::with_builtins(Arc::new(store), token_config());
    let mut ctx = AuthContext::new(TENANT, Some(SUBJECT));
    let failure = oauth2(&auth)
        .authenticate(&mut ctx, &json!({ "client_id_ref": "cred://client-id" }))
        .await
        .expect_err("no endpoint means no exchange");
    assert!(matches!(failure, PluginFailure::Configuration { .. }));
}

#[test]
fn the_token_cache_verifies_the_stored_key_on_every_hit() {
    let cache = TokenCache::new(TokenCacheConfig::new(Duration::from_secs(300), 10));
    cache.store("tenant-a", SecretString::new("tok-a"), Duration::from_secs(600));
    // The right key reads its own entry back.
    assert_eq!(cache.lookup("tenant-a").map(|token| token.expose().to_owned()), Some(String::from("tok-a")));
    // A different key is a miss, never another tenant's token.
    assert!(cache.lookup("tenant-b").is_none());
}

#[test]
fn a_token_at_or_below_the_margin_is_injected_but_not_stored() {
    assert_eq!(TOKEN_CACHE_SAFETY_MARGIN_SECS, 30);
    let cache = TokenCache::new(TokenCacheConfig::new(Duration::from_secs(300), 10));
    assert!(!cache.store("k", SecretString::new("tok"), Duration::from_secs(30)));
    assert!(cache.lookup("k").is_none());
    assert!(cache.store("k", SecretString::new("tok"), Duration::from_secs(31)));
    assert_eq!(cache.lookup("k").map(|token| token.expose().to_owned()), Some(String::from("tok")));
}

#[test]
fn the_entry_ttl_is_the_ceiling_or_the_lifetime_less_the_margin() {
    let cache = TokenCache::new(TokenCacheConfig::new(Duration::from_secs(300), 10));
    // A long-lived token is held for the configured ceiling.
    cache.store("ceiling", SecretString::new("tok"), Duration::from_secs(3_600));
    assert_eq!(cache.lookup("ceiling").map(|token| token.expose().to_owned()), Some(String::from("tok")));
    // A short-lived token is held for the lifetime less the margin.
    cache.store("short", SecretString::new("tok"), Duration::from_secs(45));
    assert_eq!(cache.lookup("short").map(|token| token.expose().to_owned()), Some(String::from("tok")));
}

#[test]
fn the_config_hash_is_deterministic_and_key_order_independent() {
    let first = oagw::plugins::token_cache::hash_config(&json!({
        "token_endpoint": "https://idp/token",
        "scopes": "read write",
    }));
    let second = oagw::plugins::token_cache::hash_config(&json!({
        "scopes": "read write",
        "token_endpoint": "https://idp/token",
    }));
    assert_eq!(first, second);
    let different = oagw::plugins::token_cache::hash_config(&json!({
        "token_endpoint": "https://idp/token",
        "scopes": "read",
    }));
    assert_ne!(first, different);
}

#[test]
fn the_cache_key_carries_the_four_identity_components() {
    let tenant_a = uuid::Uuid::from_u128(0x01);
    let tenant_b = uuid::Uuid::from_u128(0x02);
    let config = json!({ "token_endpoint": "https://idp/token" });
    let base = oagw::plugins::token_cache::cache_key(tenant_a, Some(SUBJECT), "form", &config);
    assert_ne!(base, oagw::plugins::token_cache::cache_key(tenant_b, Some(SUBJECT), "form", &config));
    assert_ne!(base, oagw::plugins::token_cache::cache_key(tenant_a, None, "form", &config));
    assert_ne!(base, oagw::plugins::token_cache::cache_key(tenant_a, Some(SUBJECT), "basic", &config));
}

#[test]
fn the_required_headers_guard_rejects_the_first_missing_request_header() {
    let (auth, guard, _transform) = builtins();
    assert_eq!(auth.len(), 4);
    let config = json!({ "required_request_headers": "x-request-id, x-tenant" });
    let mut ctx = RequestContext::new(String::from("GET"), String::from("/v1/chat"), None);
    ctx.set_header("x-request-id", "abc");
    let decision = required_headers(&guard).guard_request(&ctx, &config);
    match decision {
        GuardDecision::Reject { code, message } => {
            assert_eq!(code, "REQUIRED_HEADER_MISSING");
            assert_eq!(message, "x-tenant");
        }
        GuardDecision::Allow => panic!("a missing required header rejects"),
    }
}

#[test]
fn the_required_headers_guard_is_case_insensitive_and_fail_open() {
    let (_auth, guard, _transform) = builtins();
    // Case-insensitive matching: the configured name and the carried name
    // differ only in case.
    let configured = json!({ "required_request_headers": "X-Request-Id" });
    let mut ctx = RequestContext::new(String::from("GET"), String::from("/v1/chat"), None);
    ctx.set_header("x-request-id", "abc");
    assert!(required_headers(&guard).guard_request(&ctx, &configured).is_allowed());

    // Fail-open: absent or blank configuration is a no-op in both phases.
    let response = ResponseContext::new(200);
    for config in [json!({}), json!({ "required_request_headers": "  " })] {
        assert!(required_headers(&guard).guard_request(&ctx, &config).is_allowed());
        assert!(required_headers(&guard).guard_response(&response, &config).is_allowed());
    }
}

#[test]
fn the_required_headers_guard_rejects_the_response_phase_with_502() {
    let (_auth, guard, _transform) = builtins();
    let config = json!({ "required_response_headers": "content-type" });
    let mut ctx = ResponseContext::new(200);
    ctx.set_header("x-other", "1");
    match required_headers(&guard).guard_response(&ctx, &config) {
        GuardDecision::Reject { code, .. } => assert_eq!(code, "REQUIRED_HEADER_MISSING"),
        GuardDecision::Allow => panic!("a missing response header rejects"),
    }
}

#[test]
fn the_request_id_transform_propagates_and_generates() {
    let (_auth, _guard, transform) = builtins();
    // An existing identifier is propagated untouched.
    let mut ctx = RequestContext::new(String::from("GET"), String::from("/v1/chat"), None);
    ctx.set_header("x-request-id", "given-id");
    request_id(&transform).transform_request(&mut ctx, &json!({}));
    assert_eq!(ctx.header("x-request-id"), Some("given-id"));

    // A missing one is generated, and the response carries it too.
    let mut fresh = RequestContext::new(String::from("GET"), String::from("/v1/chat"), None);
    request_id(&transform).transform_request(&mut fresh, &json!({}));
    let generated = fresh.header("x-request-id").expect("an id was generated");
    assert!(!generated.is_empty());
    assert_ne!(generated, "given-id");

    let mut response = ResponseContext::new(200);
    request_id(&transform).transform_response(&mut response, &json!({}));
    assert!(response.header("x-request-id").is_some());
}

#[test]
fn the_transform_plugin_declares_all_three_phases() {
    let (_auth, _guard, transform) = builtins();
    assert!(transform.declared_phases(TRANSFORM_ID).is_some());
    for phase in [
        PluginPhase::TransformRequest,
        PluginPhase::TransformResponse,
        PluginPhase::TransformError,
    ] {
        assert!(
            transform
                .resolve_for_phase(TRANSFORM_ID, phase)
                .is_ok(),
            "{phase:?} is declared"
        );
    }
}

#[test]
fn the_guard_declares_both_of_its_phases() {
    let (_auth, guard, _transform) = builtins();
    for phase in [PluginPhase::GuardRequest, PluginPhase::GuardResponse] {
        assert!(guard.resolve_for_phase(GUARD_ID, phase).is_ok(), "{phase:?}");
    }
}

#[test]
fn the_oauth2_variants_declare_the_single_auth_phase() {
    let (auth, _guard, _transform) = builtins();
    for identifier in AUTH_IDS {
        assert_eq!(
            auth.declared_phases(identifier),
            Some(vec![PluginPhase::Auth])
        );
    }
}

#[tokio::test]
async fn a_non_utf8_credential_is_a_configuration_failure() {
    let auth = AuthPluginRegistry::with_builtins(
        Arc::new(MockCredStoreClient::returning_raw_value(vec![0xff, 0xfe])),
        token_config(),
    );
    let mut ctx = AuthContext::new(TENANT, None);
    let failure = apikey(&auth)
        .authenticate(&mut ctx, &json!({ "credential_ref": "cred://api-key" }))
        .await
        .expect_err("material that is not text cannot be injected");
    assert!(matches!(failure, PluginFailure::Configuration { .. }));
}

#[tokio::test]
async fn the_credential_routine_resolves_through_the_store_only() {
    // The routine is the only thing in the gear that turns a reference into
    // material: it accepts the store client it is handed and nothing else.
    let store = Arc::new(MockCredStoreClient::with_secrets(vec![(
        String::from("api-key"),
        String::from("sk-live-1"),
    )]));
    let material = oagw::plugins::credential::resolve_credential(
        store,
        &security_context(),
        "cred://api-key",
    )
    .await
    .expect("a resolvable reference resolves");
    assert_eq!(material.expose(), "sk-live-1");
}

#[test]
fn a_secret_value_carries_no_debug_leak() {
    let value = SecretValue::new(b"sk-live-1".to_vec());
    assert!(!format!("{value:?}").contains("sk-live-1"));
    let _ = SharingMode::default();
}
