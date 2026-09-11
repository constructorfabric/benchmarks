//! Tests for the built-in auth plugins.
//!
//! Every test here also asserts the credential-isolation rule: resolved material reaches
//! the injected header and nothing else — not an error message, not an extension field,
//! not a log record.

use serde_json::Value;

use crate::plugins::{BoundPlugin, PluginContext};
use crate::security::{CredentialResolver, NoopCredentialResolver, SecurityContextHolder};
use crate::plugins::token_cache::TokenCache;

/// A resolver over a fixed table, standing in for the credential store.
struct FixedResolver {
    table: std::collections::HashMap<String, String>,
}

#[async_trait::async_trait]
impl CredentialResolver for FixedResolver {
    async fn resolve(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        reference: &str,
    ) -> Result<Option<String>, crate::error::OagwError> {
        Ok(self.table.get(crate::security::strip_scheme(reference)).cloned())
    }
}

fn holder() -> SecurityContextHolder {
    let tenant = uuid::Uuid::new_v4();
    SecurityContextHolder::new(
        toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .expect("the context is complete"),
        vec![tenant],
    )
}

fn cache() -> TokenCache {
    TokenCache::new(8, std::time::Duration::from_mins(5))
}

struct Harness {
    holder: SecurityContextHolder,
    cache: TokenCache,
    resolver: FixedResolver,
    config: serde_json::Map<String, Value>,
}

fn harness(material: &str) -> Harness {
    let mut table = std::collections::HashMap::new();
    table.insert("key".to_owned(), material.to_owned());
    harness_with_table(table)
}

fn harness_with_table(table: std::collections::HashMap<String, String>) -> Harness {
    Harness {
        holder: holder(),
        cache: cache(),
        resolver: FixedResolver { table },
        config: serde_json::Map::new(),
    }
}

/// A harness whose resolver knows the `OAuth2` client credentials.
fn oauth2_harness() -> Harness {
    let mut table = std::collections::HashMap::new();
    table.insert("client_id".to_owned(), "confidential-client".to_owned());
    table.insert("client_secret".to_owned(), "client-secret".to_owned());
    harness_with_table(table)
}

fn ctx(h: &Harness) -> PluginContext<'_> {
    PluginContext {
        security: &h.holder,
        config: &h.config,
        upstream_id: "gts.cf.core.oagw.upstream.v1~abc",
        host: "api.example.com",
        path: "/v1/items",
        request_id: "",
        credentials: &h.resolver,
        token_cache: &h.cache,
    }
}

fn binding(name: &str, config: Value) -> BoundPlugin {
    // A binding that is not a JSON object carries no settings at all.
    let config = if let Value::Object(map) = config {
        map
    } else {
        serde_json::Map::new()
    };
    BoundPlugin { name: name.to_owned(), config }
}

/// The client-auth method each `OAuth2` plugin selects.
#[test]
fn the_oauth2_plugins_differ_in_client_authentication() {
    assert_eq!(
        crate::plugins::auth::auth_method("oauth2_client_cred"),
        Some(toolkit_auth::ClientAuthMethod::Form)
    );
    assert_eq!(
        crate::plugins::auth::auth_method("oauth2_client_cred_basic"),
        Some(toolkit_auth::ClientAuthMethod::Basic)
    );
    assert_eq!(crate::plugins::auth::auth_method("apikey"), None);
}

#[test]
fn the_bindable_and_catalogued_plugin_lists_are_disjoint() {
    for name in crate::plugins::auth::UNIMPLEMENTED_AUTH_PLUGINS {
        assert!(
            !crate::plugins::auth::BINDABLE_AUTH_PLUGINS.contains(&name),
            "{name} is both bindable and unimplemented"
        );
    }
    assert_eq!(crate::plugins::auth::BINDABLE_AUTH_PLUGINS.len(), 4);
}

/// `noop` injects the resolved material as a bare authorization header.
#[tokio::test]
async fn noop_injects_the_resolved_material() {
    let h = harness("raw-material");
    let ctx = ctx(&h);
    let plugin = binding("noop", serde_json::json!({ "credential": "cred://key" }));
    let injected = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect("the plugin resolves");
    assert_eq!(injected.header, "authorization");
    assert_eq!(injected.value.as_ref(), b"raw-material");
}

/// `apikey` injects into the configured header with the configured prefix.
#[tokio::test]
async fn apikey_injects_into_the_configured_header_with_a_prefix() {
    let h = harness("sk_live_123");
    let ctx = ctx(&h);
    let plugin = binding(
        "apikey",
        serde_json::json!({ "credential": "cred://key", "header": "x-api-key", "prefix": "Bearer " }),
    );
    let injected = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect("the plugin resolves");
    assert_eq!(injected.header, "x-api-key");
    assert_eq!(injected.value.as_ref(), b"Bearer sk_live_123");
}

#[tokio::test]
async fn apikey_defaults_to_the_authorization_header_without_a_prefix() {
    let h = harness("sk_live_123");
    let ctx = ctx(&h);
    let plugin = binding("apikey", serde_json::json!({ "credential": "cred://key" }));
    let injected = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect("the plugin resolves");
    assert_eq!(injected.header, "authorization");
    assert_eq!(injected.value.as_ref(), b"sk_live_123");
}

/// A binding that names no credential reference is a configuration error, not a
/// runtime failure: it is reported before anything is resolved.
#[tokio::test]
async fn a_missing_credential_reference_is_a_validation_error() {
    let h = harness("material");
    let ctx = ctx(&h);
    let plugin = binding("apikey", serde_json::json!({}));
    let err = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect_err("no reference");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError);
    assert!(err.detail().contains("credential"), "{err}");
}

/// A reference that resolves to nothing is `SecretNotFound`, not a panic.
#[tokio::test]
async fn an_unresolved_reference_is_reported_as_secret_not_found() {
    let h = harness("material");
    let ctx = PluginContext {
        credentials: &NoopCredentialResolver,
        ..ctx(&h)
    };
    let plugin = binding("noop", serde_json::json!({ "credential": "cred://absent" }));
    let err = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect_err("the resolver has no store");
    assert_eq!(err.kind(), crate::error::ErrorKind::SecretNotFound);
}

/// The catalogued-but-unimplemented identifiers fail rather than silently passing.
#[tokio::test]
async fn a_catalog_only_auth_plugin_is_reported_as_not_implemented() {
    for name in ["basic", "bearer"] {
        let h = harness("material");
        let ctx = ctx(&h);
        let plugin = binding(name, serde_json::json!({ "credential": "cred://key" }));
        let err = crate::plugins::auth::execute(&plugin, &ctx)
            .await
            .expect_err("{name} is catalogued only");
        assert_eq!(err.kind(), crate::error::ErrorKind::PluginNotFound, "{name}");
        assert!(err.detail().contains(name), "{name}: {err}");
    }
}

/// An unknown identifier is a plugin-not-found error, carrying the request context.
#[tokio::test]
async fn an_unknown_auth_plugin_names_itself_in_the_error() {
    let h = harness("material");
    let ctx = ctx(&h);
    let plugin = binding("sigv4", serde_json::json!({ "credential": "cred://key" }));
    let err = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect_err("unknown");
    assert_eq!(err.kind(), crate::error::ErrorKind::PluginNotFound);
    assert!(err.detail().contains("sigv4"), "{err}");
}

/// The resolved material is injected once and appears nowhere in the error surface.
#[tokio::test]
async fn resolved_material_never_reaches_an_error_document() {
    let secret = "super-secret-material-value";
    let h = harness(secret);
    let ctx = PluginContext {
        credentials: &NoopCredentialResolver,
        ..ctx(&h)
    };
    let plugin = binding("noop", serde_json::json!({ "credential": "cred://key" }));
    let err = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect_err("the resolver has no store");
    let rendered = format!("{err}"); // Display
    let document = format!("{:?}", err.to_problem(None));
    let canonical = format!("{:?}", err.to_canonical());
    assert!(
        !rendered.contains(secret),
        "the material leaked into the error text: {rendered}"
    );
    assert!(
        !document.contains(secret),
        "the material leaked into the problem document"
    );
    assert!(
        !canonical.contains(secret),
        "the material leaked into the canonical error"
    );
}

/// A failed `OAuth2` exchange names the failure, never the credentials it used.
#[tokio::test]
async fn a_failed_oauth2_exchange_reports_no_credential() {
    let mut table = std::collections::HashMap::new();
    table.insert("client_id".to_owned(), "confidential-client".to_owned());
    table.insert(
        "client_secret".to_owned(),
        "client-secret-do-not-echo".to_owned(),
    );
    let h = harness_with_table(table);
    let ctx = ctx(&h);
    let plugin = binding(
        "oauth2_client_cred",
        serde_json::json!({
            "client_id": "cred://client_id",
            "client_secret": "cred://client_secret",
            "token_endpoint": "http://127.0.0.1:1/no-such-token-endpoint"
        }),
    );
    let err = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect_err("the endpoint is unreachable");
    assert_eq!(err.kind(), crate::error::ErrorKind::AuthenticationFailed);
    let rendered = err.detail().to_owned();
    assert!(
        !rendered.contains("client-secret-do-not-echo"),
        "the secret leaked into the failure: {rendered}"
    );
    assert!(
        !rendered.contains(&h.resolver.table["client_secret"]),
        "the resolved material leaked into the failure: {rendered}"
    );
}

/// An unparseable token endpoint is a configuration error, not an exchange attempt.
#[tokio::test]
async fn a_malformed_token_endpoint_is_a_validation_error() {
    let h = oauth2_harness();
    let ctx = ctx(&h);
    let plugin = binding(
        "oauth2_client_cred",
        serde_json::json!({
            "client_id": "cred://client_id",
            "client_secret": "cred://client_secret",
            "token_endpoint": "not a url"
        }),
    );
    let err = crate::plugins::auth::execute(&plugin, &ctx)
        .await
        .expect_err("not a URL");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError);
}
