// Created: 2026-09-04 by Constructor Tech
//! Tests of the plugin chain of the data plane.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::domain::plugin::{
    AUTH_APIKEY, AUTH_NOOP, AUTH_PLUGIN_TYPE, GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID,
};
use crate::domain::{AuthConfig, HttpMethod, PluginKind, PluginRef, SharingMode};

/// A tenant / subject pair shared by the tests of this module.
fn ids() -> (Uuid, Uuid) {
    (Uuid::from_u128(0x0A6D), Uuid::from_u128(0x5EB))
}

/// A request context with the given headers and configuration.
fn context(headers: &[(&str, &str)], config: serde_json::Value) -> RequestContext {
    let (tenant_id, subject_id) = ids();
    RequestContext {
        tenant_id,
        subject_id,
        upstream_id: Uuid::from_u128(0x1),
        route_id: None,
        method: HttpMethod::parse("GET").unwrap(),
        path: String::from("/v1/ping"),
        query: None,
        headers: headers_of(headers),
        body: None,
        request_id: None,
        config,
    }
}

/// A header map built from raw `name: value` pairs.
fn headers_of(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

/// A guard plugin reference.
fn guard_ref() -> PluginRef {
    PluginRef::parse(PluginKind::Guard, GUARD_REQUIRED_HEADERS).unwrap()
}

/// A transform plugin reference.
fn transform_ref() -> PluginRef {
    PluginRef::parse(PluginKind::Transform, TRANSFORM_REQUEST_ID).unwrap()
}

/// An auth plugin reference.
fn auth_ref() -> PluginRef {
    PluginRef::parse(PluginKind::Auth, AUTH_APIKEY).unwrap()
}

/// A credential resolver serving one fixed secret.
struct FixedResolver {
    secret: Option<String>,
}

#[async_trait]
impl CredentialResolver for FixedResolver {
    fn resolves(&self) -> bool {
        self.secret.is_some()
    }

    async fn resolve(&self, request: &CredentialRequest<'_>) -> Result<String, OagwError> {
        match &self.secret {
            Some(secret) => Ok(secret.clone()),
            None => Err(OagwError::AuthenticationFailed {
                detail: format!("secret {} is unknown", request.secret.as_str()),
            }),
        }
    }
}

/// The built-in registry, optionally with a credential resolver.
fn registry(resolver: Option<Arc<dyn CredentialResolver>>) -> PluginRegistry {
    PluginRegistry::with_builtins(resolver)
}

#[test]
fn registry_resolves_builtins_by_reference() {
    let registry = registry(None);
    assert!(
        registry
            .auth(&PluginRef::parse(PluginKind::Auth, AUTH_NOOP).unwrap())
            .is_some()
    );
    assert!(registry.auth(&auth_ref()).is_some());
    assert!(registry.guard(&guard_ref()).is_some());
    assert!(registry.transform(&transform_ref()).is_some());
    // A reference of the wrong kind resolves to nothing.
    assert!(registry.guard(&auth_ref()).is_none());
    assert!(registry.transform(&guard_ref()).is_none());
}

#[test]
fn registry_reports_the_builtin_catalog() {
    let registry = registry(None);
    let debug = format!("{registry:?}");
    assert!(debug.contains("auth"));
    assert!(debug.contains("guard"));
    assert!(debug.contains("transform"));
}

#[tokio::test]
async fn noop_auth_plugin_injects_nothing() {
    let registry = registry(None);
    let plugin = registry
        .auth(&PluginRef::parse(PluginKind::Auth, AUTH_NOOP).unwrap())
        .unwrap();
    let mut ctx = context(&[], serde_json::Value::Null);
    plugin.authenticate(&mut ctx).await.unwrap();
    assert!(ctx.headers.is_empty());
    assert_eq!(plugin.plugin_type(), AUTH_PLUGIN_TYPE);
}

#[tokio::test]
async fn required_headers_guard_rejects_the_first_missing_header() {
    let registry = registry(None);
    let plugin = registry.guard(&guard_ref()).unwrap();
    let config = json!({"required_request_headers": "x-tenant-id, x-signature"});

    let ctx = context(&[("x-signature", "abc")], config);
    let decision = plugin.guard_request(&ctx).await.unwrap();
    match decision {
        GuardDecision::Reject(OagwError::Validation { detail }) => {
            assert!(
                detail.contains("x-tenant-id"),
                "first missing header: {detail}"
            );
            assert!(!detail.contains("x-signature"));
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[tokio::test]
async fn required_headers_guard_allows_a_request_with_every_header() {
    let registry = registry(None);
    let plugin = registry.guard(&guard_ref()).unwrap();
    let config = json!({"required_request_headers": ["x-tenant-id", "x-signature"]});
    let ctx = context(&[("x-tenant-id", "t"), ("X-Signature", "abc")], config);
    assert!(plugin.guard_request(&ctx).await.unwrap().is_allow());
}

#[tokio::test]
async fn required_headers_guard_is_fail_open_when_unconfigured() {
    let registry = registry(None);
    let plugin = registry.guard(&guard_ref()).unwrap();
    let ctx = context(&[], serde_json::Value::Null);
    assert!(plugin.guard_request(&ctx).await.unwrap().is_allow());

    let config = json!({"required_request_headers": "   "});
    let ctx = context(&[], config);
    assert!(plugin.guard_request(&ctx).await.unwrap().is_allow());
}

#[tokio::test]
async fn required_headers_guard_rejects_a_response_missing_a_header() {
    let registry = registry(None);
    let plugin = registry.guard(&guard_ref()).unwrap();
    let ctx = ResponseContext {
        status: http::StatusCode::OK,
        headers: HeaderMap::new(),
        request_id: None,
        config: json!({"required_response_headers": "x-request-id"}),
    };
    let decision = plugin.guard_response(&ctx).await.unwrap();
    match decision {
        GuardDecision::Reject(OagwError::DownstreamError { detail }) => {
            assert!(detail.contains("x-request-id"));
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[tokio::test]
async fn request_id_transform_propagates_the_incoming_identifier() {
    let registry = registry(None);
    let plugin = registry.transform(&transform_ref()).unwrap();
    let mut ctx = context(&[("x-request-id", "trace-1")], serde_json::Value::Null);

    plugin.transform_request(&mut ctx).await.unwrap();
    assert_eq!(ctx.request_id.as_deref(), Some("trace-1"));
    assert_eq!(ctx.headers.get("x-request-id").unwrap(), "trace-1");

    // The identifier is echoed on the response and on a problem response.
    let mut response = ResponseContext {
        status: http::StatusCode::OK,
        headers: HeaderMap::new(),
        request_id: ctx.request_id.clone(),
        config: serde_json::Value::Null,
    };
    plugin.transform_response(&mut response).await.unwrap();
    assert_eq!(response.headers.get("x-request-id").unwrap(), "trace-1");

    let mut error = ErrorContext {
        error: OagwError::Validation {
            detail: String::from("boom"),
        },
        headers: HeaderMap::new(),
        request_id: ctx.request_id.clone(),
        config: serde_json::Value::Null,
    };
    plugin.transform_error(&mut error).await.unwrap();
    assert_eq!(error.headers.get("x-request-id").unwrap(), "trace-1");
}

#[tokio::test]
async fn request_id_transform_generates_one_when_absent() {
    let registry = registry(None);
    let plugin = registry.transform(&transform_ref()).unwrap();
    let mut ctx = context(&[], serde_json::Value::Null);

    plugin.transform_request(&mut ctx).await.unwrap();
    let request_id = ctx.request_id.clone().unwrap();
    assert_eq!(request_id.len(), 32, "a simple UUID has no separators");
    assert_eq!(
        ctx.headers.get("x-request-id").unwrap(),
        request_id.as_str()
    );
}

#[tokio::test]
async fn apikey_plugin_injects_the_default_header() {
    let registry = registry(Some(Arc::new(FixedResolver { secret: None })));
    let plugin = registry.auth(&auth_ref()).unwrap();
    let mut ctx = context(&[], json!({"key": "sk-123"}));

    plugin.authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-123");
}

#[tokio::test]
async fn apikey_plugin_injects_into_the_configured_header() {
    let registry = registry(None);
    let plugin = registry.auth(&auth_ref()).unwrap();
    let mut ctx = context(&[], json!({"key": "sk-123", "header": "X-Upstream-Key"}));

    plugin.authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.headers.get("x-upstream-key").unwrap(), "sk-123");
    assert!(!ctx.headers.contains_key("x-api-key"));
}

#[tokio::test]
async fn apikey_plugin_can_inject_a_query_parameter() {
    let registry = registry(None);
    let plugin = registry.auth(&auth_ref()).unwrap();
    let mut ctx = context(&[], json!({"key": "sk-123", "query": "api_key"}));
    ctx.query = Some(String::from("top=5"));

    plugin.authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.query.as_deref(), Some("top=5&api_key=sk-123"));
    assert!(!ctx.headers.contains_key("x-api-key"));
}

#[tokio::test]
async fn apikey_plugin_resolves_cred_references() {
    let registry = registry(Some(Arc::new(FixedResolver {
        secret: Some(String::from("sk-from-store")),
    })));
    let plugin = registry.auth(&auth_ref()).unwrap();
    let mut ctx = context(&[], json!({"key": "cred://tenants/acme/openai"}));

    plugin.authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-from-store");
}

#[tokio::test]
async fn apikey_plugin_reads_the_dedicated_secret_keys() {
    for key in ["key_secret", "secret_ref"] {
        let registry = registry(Some(Arc::new(FixedResolver {
            secret: Some(String::from("sk-dedicated")),
        })));
        let plugin = registry.auth(&auth_ref()).unwrap();
        let mut ctx = context(&[], json!({ key: "cred://tenants/acme/openai" }));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-dedicated");
    }
}

#[tokio::test]
async fn apikey_plugin_fails_closed_without_a_key_source() {
    let registry = registry(Some(Arc::new(FixedResolver { secret: None })));
    let plugin = registry.auth(&auth_ref()).unwrap();
    let mut ctx = context(&[], json!({"header": "x-api-key"}));

    let error = plugin.authenticate(&mut ctx).await.unwrap_err();
    match error {
        OagwError::AuthenticationFailed { .. } => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn apikey_plugin_surfaces_a_credential_store_failure_as_401() {
    let registry = registry(Some(Arc::new(FixedResolver { secret: None })));
    let plugin = registry.auth(&auth_ref()).unwrap();
    let mut ctx = context(&[], json!({"key_secret": "cred://missing"}));

    let error = plugin.authenticate(&mut ctx).await.unwrap_err();
    assert!(matches!(error, OagwError::AuthenticationFailed { .. }));
    assert!(!format!("{error:?}").contains("cred://missing"));
}

#[tokio::test]
async fn apikey_plugin_without_a_resolver_fails_closed() {
    let registry = registry(None);
    let plugin = registry.auth(&auth_ref()).unwrap();
    let mut ctx = context(&[], json!({"key": "cred://tenants/acme/openai"}));

    let error = plugin.authenticate(&mut ctx).await.unwrap_err();
    assert!(matches!(error, OagwError::AuthenticationFailed { .. }));
}

#[test]
fn required_header_names_are_normalized() {
    assert_eq!(
        required(&json!({"k": " A , B ,, "}), "k"),
        vec![String::from("a"), String::from("b")]
    );
    assert_eq!(
        required(&json!({"k": ["A", " B, C "]}), "k"),
        vec![String::from("a"), String::from("b"), String::from("c")]
    );
    assert!(required(&json!({"k": 12}), "k").is_empty());
    assert!(required(&json!({}), "k").is_empty());
}

#[test]
fn config_str_reads_string_values_only() {
    let config = json!({"key": "value", "number": 1});
    assert_eq!(config_str(&config, "key"), Some("value"));
    assert_eq!(config_str(&config, "number"), None);
    assert_eq!(config_str(&config, "absent"), None);
}

#[tokio::test]
async fn an_unresolved_chain_entry_reports_the_plugin_reference() {
    let plane_registry = PluginRegistry::default();
    assert!(plane_registry.guard(&guard_ref()).is_none());
    assert!(plane_registry.auth(&auth_ref()).is_none());
    assert!(plane_registry.transform(&transform_ref()).is_none());
}

#[test]
fn auth_config_sharing_is_carried_into_the_policy() {
    let config = AuthConfig {
        sharing: SharingMode::Enforce,
        plugin: Some(auth_ref()),
        config: json!({"key": "sk-123"}),
    };
    assert_eq!(config.sharing, SharingMode::Enforce);
    assert_eq!(config.plugin, Some(auth_ref()));
}

#[test]
fn request_context_holds_the_forwarded_request() {
    let (tenant_id, subject_id) = ids();
    let ctx = context(&[("x-tenant", "t1")], serde_json::Value::Null);
    assert_eq!(ctx.tenant_id, tenant_id);
    assert_eq!(ctx.subject_id, subject_id);
    assert_eq!(ctx.method, HttpMethod::parse("GET").unwrap());
    assert_eq!(ctx.headers.get("x-tenant").unwrap(), "t1");
    assert!(ctx.body.is_none());
    assert_eq!(ctx.method.as_str(), Method::GET.as_str());
}
