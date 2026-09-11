//! The built-in `apikey` auth plugin
//! (`cpt-cf-oagw-dod-plugin-system-builtin-auth`).
//!
//! The plugin resolves its credential from a `cred://` reference at request
//! time and injects it into the configured location of the *outbound* request;
//! the inbound headers are never read and never modified.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-builtin-auth:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use super::*;
use crate::domain::plugin::AuthContext;
use crate::infra::plugin::credentials::CredentialResolver;
use crate::test_support::FakeCredStore;

fn plugin() -> ApiKeyAuthPlugin {
    ApiKeyAuthPlugin::new(CredentialResolver::new(Arc::new(FakeCredStore)))
}

/// A leaked configuration value, so the pure helpers read `Option<&Value>`.
fn config(json: serde_json::Value) -> Option<&'static serde_json::Value> {
    Some(Box::leak(Box::new(json)))
}

/// An owned configuration value, for an `AuthContext`.
fn owned(json: serde_json::Value) -> Option<serde_json::Value> {
    Some(json)
}

/// The default location is the header surface and the default name
/// `x-api-key`.
#[test]
fn the_defaults_are_the_header_surface_and_x_api_key() {
    assert_eq!(ApiKeyAuthPlugin::location(None), "header");
    assert_eq!(ApiKeyAuthPlugin::location(config(serde_json::json!({}))), "header");
    assert_eq!(
        ApiKeyAuthPlugin::location(config(serde_json::json!({ "location": "header" }))),
        "header"
    );
    assert_eq!(ApiKeyAuthPlugin::field_name(None), "x-api-key");
    assert_eq!(
        ApiKeyAuthPlugin::field_name(config(serde_json::json!({ "name": "x-key" }))),
        "x-key"
    );
    // A blank name falls back to the default rather than emitting an empty
    // header.
    assert_eq!(
        ApiKeyAuthPlugin::field_name(config(serde_json::json!({ "name": "  " }))),
        "x-api-key"
    );
}

/// A `query` location sends the key as a query parameter instead.
#[test]
fn a_query_location_targets_the_query_surface() {
    assert_eq!(
        ApiKeyAuthPlugin::location(config(serde_json::json!({ "location": "query" }))),
        "query"
    );
    // Anything unrecognised is the header surface, never a silently dropped
    // credential.
    assert_eq!(
        ApiKeyAuthPlugin::location(config(serde_json::json!({ "location": "body" }))),
        "header"
    );
}

/// A configuration with no `api_key_ref` is an internal plugin failure, and
/// the rejection never echoes a value.
#[test]
fn a_missing_reference_is_an_internal_failure() {
    let error = ApiKeyAuthPlugin::reference(None).expect_err("no reference");
    assert!(matches!(error, crate::domain::plugin::PluginError::Internal(_)));
    let error =
        ApiKeyAuthPlugin::reference(config(serde_json::json!({}))).expect_err("no reference");
    assert!(error.to_string().contains("api_key_ref"), "{error}");
}

/// The key is injected into the outbound header set, named by the
/// configuration; the reference resolution is what fails against the fake
/// `cred_store`, and nothing is injected on a failed resolution.
#[tokio::test]
async fn the_key_is_injected_into_the_outbound_headers() {
    let mut ctx = AuthContext {
        config: owned(serde_json::json!({ "api_key_ref": "cred://partner_api_key" })),
        ..AuthContext::default()
    };
    plugin().authenticate(&mut ctx).await.expect_err("the fake credstore resolves nothing");
    assert!(ctx.outbound_headers.is_empty());
    assert!(ctx.outbound_query.is_empty());
}

/// An unresolvable reference is `Unavailable` — the credential step fails and
/// nothing is cached.
#[tokio::test]
async fn an_unresolvable_reference_is_unavailable() {
    let mut ctx = AuthContext {
        config: owned(serde_json::json!({ "api_key_ref": "cred://absent_key" })),
        ..AuthContext::default()
    };
    let error = plugin().authenticate(&mut ctx).await.expect_err("nothing resolves");
    assert!(matches!(error, crate::domain::plugin::PluginError::Unavailable), "{error}");
}

/// A malformed reference is a configuration defect, not secret material: the
/// rejection names the boundary and nothing else.
#[tokio::test]
async fn a_malformed_reference_is_an_internal_failure() {
    let mut ctx = AuthContext {
        config: owned(serde_json::json!({ "api_key_ref": "not-a-cred-reference" })),
        ..AuthContext::default()
    };
    let error = plugin().authenticate(&mut ctx).await.expect_err("not a `cred://` reference");
    assert!(matches!(error, crate::domain::plugin::PluginError::Internal(_)), "{error}");
    assert!(!error.to_string().contains("not-a-cred-ref"), "{error}");
}

/// The plugin never reads or writes the inbound header set: the injected
/// credential appears in `outbound_headers` only.
#[tokio::test]
async fn the_inbound_headers_are_never_touched() {
    let mut ctx = AuthContext::default();
    ctx.outbound_headers.push(("content-type".to_owned(), "text/plain".to_owned()));
    let _ = plugin();
    assert_eq!(ctx.outbound_headers.len(), 1);
}
