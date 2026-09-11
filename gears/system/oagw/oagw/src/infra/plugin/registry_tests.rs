//! Which plugin identifiers the in-process registries resolve.

use super::*;
use crate::domain::model::ConfigMap;
use crate::domain::plugin::{AuthContext, PluginScope};
use credstore_sdk::test_util::MockCredStoreClient;
use http::HeaderMap;
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn credstore(secrets: Vec<(&str, &str)>) -> Arc<dyn CredStoreClientV1> {
    Arc::new(MockCredStoreClient::with_secrets(
        secrets
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    ))
}

fn registries(secrets: Vec<(&str, &str)>) -> PluginRegistries {
    PluginRegistries::with_builtins(
        credstore(secrets),
        super::super::oauth2_client_cred_auth::TokenCacheConfig::default(),
    )
}

#[test]
fn every_implemented_auth_plugin_resolves() {
    let registries = registries(Vec::new());
    for id in [
        gts::NOOP_AUTH_PLUGIN_ID,
        gts::APIKEY_AUTH_PLUGIN_ID,
        gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    ] {
        let plugin = registries.auth.get(id).unwrap_or_else(|| panic!("{id}"));
        assert_eq!(plugin.plugin_type(), id);
    }
}

#[test]
fn the_catalog_only_auth_identifiers_have_no_implementation() {
    let registries = registries(Vec::new());
    assert!(registries.auth.get(gts::BASIC_AUTH_PLUGIN_ID).is_none());
    assert!(registries.auth.get(gts::BEARER_AUTH_PLUGIN_ID).is_none());
}

#[test]
fn required_headers_is_the_only_builtin_guard() {
    let registries = registries(Vec::new());
    assert!(registries.guard.get(gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID).is_some());
    // Timeout and CORS are core Data Plane logic, not guard implementations.
    assert!(registries.guard.get(gts::TIMEOUT_GUARD_PLUGIN_ID).is_none());
    assert!(registries.guard.get(gts::CORS_GUARD_PLUGIN_ID).is_none());
}

#[test]
fn request_id_is_the_only_builtin_transform() {
    let registries = registries(Vec::new());
    assert!(registries.transform.get(gts::REQUEST_ID_TRANSFORM_PLUGIN_ID).is_some());
    // Logging and metrics are core instrumentation.
    assert!(registries.transform.get(gts::LOGGING_TRANSFORM_PLUGIN_ID).is_none());
    assert!(registries.transform.get(gts::METRICS_TRANSFORM_PLUGIN_ID).is_none());
}

async fn inject(config: ConfigMap, secrets: Vec<(&str, &str)>) -> (HeaderMap, Vec<(String, String)>) {
    let registries = registries(secrets);
    let plugin = registries.auth.get(gts::APIKEY_AUTH_PLUGIN_ID).unwrap();
    let security = SecurityContext::anonymous();
    let mut headers = HeaderMap::new();
    let mut query = Vec::new();
    let mut ctx = AuthContext {
        scope: PluginScope {
            security_context: &security,
            alias: "api.example.com",
            upstream_id: Uuid::nil(),
            route_id: None,
        },
        config: &config,
        headers: &mut headers,
        query: &mut query,
    };
    plugin.authenticate(&mut ctx).await.expect("injection should succeed");
    (headers, query)
}

fn config(pairs: &[(&str, &str)]) -> ConfigMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), serde_json::Value::from(*v)))
        .collect()
}

#[tokio::test]
async fn the_api_key_plugin_injects_a_header_by_default() {
    let (headers, query) = inject(
        config(&[("secret_ref", "cred://openai-key")]),
        vec![("openai-key", "sk-test")],
    )
    .await;
    assert_eq!(headers.get("x-api-key").unwrap(), "sk-test");
    assert!(query.is_empty());
}

#[tokio::test]
async fn the_api_key_plugin_honours_the_configured_header_and_prefix() {
    let (headers, _) = inject(
        config(&[
            ("secret_ref", "cred://openai-key"),
            ("name", "Authorization"),
            ("prefix", "Bearer"),
        ]),
        vec![("openai-key", "sk-test")],
    )
    .await;
    assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-test");
    // Header values holding secret material are marked sensitive so tracing
    // layers redact them.
    assert!(headers.get("authorization").unwrap().is_sensitive());
}

#[tokio::test]
async fn the_api_key_plugin_can_inject_a_query_parameter() {
    let (headers, query) = inject(
        config(&[
            ("secret_ref", "cred://openai-key"),
            ("in", "query"),
            ("name", "api_key"),
        ]),
        vec![("openai-key", "sk-test")],
    )
    .await;
    assert!(headers.is_empty());
    assert_eq!(query, vec![("api_key".to_owned(), "sk-test".to_owned())]);
}

#[tokio::test]
async fn the_api_key_plugin_needs_a_secret_reference() {
    let registries = registries(Vec::new());
    let plugin = registries.auth.get(gts::APIKEY_AUTH_PLUGIN_ID).unwrap();
    let security = SecurityContext::anonymous();
    let empty = ConfigMap::new();
    let mut headers = HeaderMap::new();
    let mut query = Vec::new();
    let mut ctx = AuthContext {
        scope: PluginScope {
            security_context: &security,
            alias: "api.example.com",
            upstream_id: Uuid::nil(),
            route_id: None,
        },
        config: &empty,
        headers: &mut headers,
        query: &mut query,
    };
    let err = plugin.authenticate(&mut ctx).await.unwrap_err();
    assert!(err.to_string().contains("secret_ref"), "{err}");
}

#[tokio::test]
async fn the_noop_plugin_injects_nothing() {
    let registries = registries(Vec::new());
    let plugin = registries.auth.get(gts::NOOP_AUTH_PLUGIN_ID).unwrap();
    let security = SecurityContext::anonymous();
    let empty = ConfigMap::new();
    let mut headers = HeaderMap::new();
    let mut query = Vec::new();
    let mut ctx = AuthContext {
        scope: PluginScope {
            security_context: &security,
            alias: "api.example.com",
            upstream_id: Uuid::nil(),
            route_id: None,
        },
        config: &empty,
        headers: &mut headers,
        query: &mut query,
    };
    plugin.authenticate(&mut ctx).await.unwrap();
    assert!(headers.is_empty());
    assert!(query.is_empty());
}

#[test]
fn a_gear_supplied_plugin_can_be_registered() {
    let mut registry = GuardPluginRegistry::with_builtins();
    registry.register(Arc::new(super::super::required_headers_guard::RequiredHeadersGuardPlugin));
    assert!(registry.get(gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID).is_some());
}
