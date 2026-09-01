//! `NoopAuthPlugin` and `ApiKeyAuthPlugin` tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;

use crate::domain::error::DomainError;
use crate::domain::plugin::AuthPlugin;
use crate::infra::plugin::apikey_auth::{ApiKeyAuthPlugin, ApiKeyLocation};
use crate::infra::plugin::noop_auth::NoopAuthPlugin;
use crate::infra::plugin::secrets::StaticSecretResolver;

fn context() -> crate::domain::dto::ProxyContext {
    crate::domain::dto::ProxyContext {
        alias: "partner-openai".to_owned(),
        method: "GET".to_owned(),
        path: "/v1/models".to_owned(),
        query: Vec::new(),
        headers: BTreeMap::from([("accept".to_owned(), "application/json".to_owned())]),
        trace_id: Some("trace-1".to_owned()),
        tenant: uuid::Uuid::nil(),
        subject: uuid::Uuid::nil(),
    }
}

#[tokio::test]
async fn noop_auth_forwards_the_caller_credentials_untouched() {
    let mut request = context();
    request
        .headers
        .insert("authorization".to_owned(), "Basic zzz".to_owned());
    let outcome = NoopAuthPlugin::new()
        .authenticate(&mut request)
        .await
        .unwrap();
    assert_eq!(outcome, crate::domain::plugin::AuthOutcome::default());
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Basic zzz")
    );
}

#[test]
fn apikey_location_defaults_to_the_x_api_key_header() {
    let location = ApiKeyLocation::parse(&json!({})).unwrap();
    assert_eq!(location, ApiKeyLocation::Header("x-api-key".to_owned()));
}

#[test]
fn apikey_location_honours_header_and_query_spellings() {
    let header =
        ApiKeyLocation::parse(&json!({"location": "header", "name": "X-Stripe-Key"})).unwrap();
    assert_eq!(header, ApiKeyLocation::Header("x-stripe-key".to_owned()));
    let query = ApiKeyLocation::parse(&json!({"location": "query", "name": "key"})).unwrap();
    assert_eq!(query, ApiKeyLocation::Query("key".to_owned()));
}

#[test]
fn apikey_location_refuses_an_unknown_location() {
    let error = ApiKeyLocation::parse(&json!({"location": "cookie"})).unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[test]
fn apikey_plugin_requires_a_value_reference() {
    let error = ApiKeyAuthPlugin::new(Arc::new(StaticSecretResolver::default()), Some(&json!({})))
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[tokio::test]
async fn apikey_plugin_injects_the_resolved_header() {
    let plugin = ApiKeyAuthPlugin::new(
        Arc::new(StaticSecretResolver::single("stripe-key", "sk_live_1")),
        Some(&json!({"value_ref": "stripe-key", "location": "header", "name": "X-Api-Key"})),
    )
    .unwrap();
    let mut request = context();
    let outcome = plugin.authenticate(&mut request).await.unwrap();
    assert_eq!(
        request.headers.get("x-api-key").map(String::as_str),
        Some("sk_live_1")
    );
    assert_eq!(
        outcome
            .forwarded_headers
            .get("x-api-key")
            .map(String::as_str),
        Some("sk_live_1")
    );
    assert!(!request.headers.contains_key("x-api-key-x"));
}

#[tokio::test]
async fn apikey_plugin_injects_the_resolved_query_parameter() {
    let plugin = ApiKeyAuthPlugin::new(
        Arc::new(StaticSecretResolver::single("maps-key", "AIza_1")),
        Some(&json!({"value_ref": "maps-key", "location": "query", "name": "api-key"})),
    )
    .unwrap();
    let mut request = context();
    let outcome = plugin.authenticate(&mut request).await.unwrap();
    assert_eq!(
        request.query,
        vec![("api-key".to_owned(), "AIza_1".to_owned())]
    );
    assert_eq!(
        outcome
            .forwarded_headers
            .get("query:api-key")
            .map(String::as_str),
        Some("AIza_1")
    );
}

#[tokio::test]
async fn apikey_plugin_propagates_a_secret_failure() {
    let plugin = ApiKeyAuthPlugin::new(
        Arc::new(StaticSecretResolver::default()),
        Some(&json!({"value_ref": "absent"})),
    )
    .unwrap();
    let mut request = context();
    let error = plugin.authenticate(&mut request).await.unwrap_err();
    assert!(matches!(error, DomainError::SecretNotFound { .. }));
}

#[test]
fn apikey_plugin_reports_its_gts_id() {
    let plugin = ApiKeyAuthPlugin::new(
        Arc::new(StaticSecretResolver::default()),
        Some(&json!({"value_ref": "stripe-key"})),
    )
    .unwrap();
    assert_eq!(
        plugin.gts_id(),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(
        NoopAuthPlugin::new().gts_id(),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
    );
}
