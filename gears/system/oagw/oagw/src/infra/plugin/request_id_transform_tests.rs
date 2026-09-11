//! The built-in `request_id` transform
//! (`cpt-cf-oagw-dod-plugin-system-request-id-transform`).
//!
//! The correlation identifier is minted once at proxy entry, so the plugin
//! *adopts* the propagated value and mints a fresh lowercase hyphenated UUID
//! only when there is no valid one; it is the only writer of `X-Request-ID`,
//! and it never emits the header to the caller.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-request-id-transform:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::domain::plugin::{ErrorContext, RequestContext, ResponseContext, TransformPlugin};
use serde_json::json;

/// The `X-Request-ID` value of a request context, first entry wins.
fn request_id_of(ctx: &RequestContext) -> Option<&str> {
    ctx.headers
        .iter()
        .find(|(name, _)| name == REQUEST_ID_HEADER)
        .map(|(_, value)| value.as_str())
}

fn context(headers: Vec<(&str, &str)>, extensions: Vec<(&str, &str)>) -> RequestContext {
    RequestContext {
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        extensions: extensions
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        ..RequestContext::default()
    }
}

/// `injects_minted_lowercase_hyphenated_uuid_when_absent`
/// (`inst-ps-rid-1`).
#[tokio::test]
async fn injects_minted_lowercase_hyphenated_uuid_when_absent() {
    let plugin = RequestIdTransformPlugin;
    let mut ctx = context(vec![("content-type", "text/plain")], vec![]);
    plugin.transform_request(&mut ctx).await.expect("the transform runs");
    let value = request_id_of(&ctx).expect("the header is written");
    let uuid = uuid::Uuid::parse_str(value).expect("the value is a UUID");
    assert_eq!(uuid.to_string(), value, "the UUID is lowercase hyphenated");
    // The minted value is held in the request context for the response phase.
    assert_eq!(
        ctx.extensions.iter().find(|(name, _)| name == REQUEST_ID_EXTENSION).map(|(_, v)| v.as_str()),
        Some(value)
    );
}

/// `adopts_the_proxy_entry_correlation_identifier` (`inst-ps-rid-2`).
#[tokio::test]
async fn adopts_the_proxy_entry_correlation_identifier() {
    let plugin = RequestIdTransformPlugin;
    let mut ctx = context(
        vec![("x-request-id", "inbound-value")],
        vec![(CORRELATION_EXTENSION, "proxy-entry-id")],
    );
    plugin.transform_request(&mut ctx).await.expect("the transform runs");
    assert_eq!(
        request_id_of(&ctx).unwrap_or_default(),
        "proxy-entry-id",
        "the propagated value wins over the inbound header"
    );
    // Without a propagated value the inbound header is adopted instead.
    let mut ctx = context(vec![("x-request-id", "inbound-value")], vec![]);
    plugin.transform_request(&mut ctx).await.expect("the transform runs");
    assert_eq!(request_id_of(&ctx).unwrap_or_default(), "inbound-value");
}

/// `validates_inbound_value_and_replaces_an_invalid_one`
/// (`inst-ps-rid-3`).
#[tokio::test]
async fn validates_inbound_value_and_replaces_an_invalid_one() {
    // The accepted window: 1..=128 characters over the RFC 3986 unreserved
    // set plus `-._~`.
    assert_eq!(RequestIdTransformPlugin::validate_inbound("abc-._~XYZ0189"), Some("abc-._~XYZ0189"));
    assert_eq!(RequestIdTransformPlugin::validate_inbound(""), None);
    assert_eq!(RequestIdTransformPlugin::validate_inbound("has space"), None);
    assert_eq!(RequestIdTransformPlugin::validate_inbound("slash/not-allowed"), None);
    assert_eq!(RequestIdTransformPlugin::validate_inbound(&"x".repeat(MAX_INBOUND_LENGTH + 1)), None);
    assert_eq!(
        RequestIdTransformPlugin::validate_inbound(&"x".repeat(MAX_INBOUND_LENGTH)),
        Some("x".repeat(MAX_INBOUND_LENGTH).as_str())
    );

    let plugin = RequestIdTransformPlugin;
    let mut ctx = context(vec![("X-REQUEST-ID", "not a valid id!")], vec![]);
    plugin.transform_request(&mut ctx).await.expect("the transform runs");
    let value = request_id_of(&ctx).expect("the header is written");
    assert_ne!(value, "not a valid id!");
    assert!(uuid::Uuid::parse_str(value).is_ok(), "{value} is the minted replacement");
    // The invalid inbound entry is replaced, never forwarded alongside it.
    assert_eq!(ctx.headers.iter().filter(|(name, _)| name == REQUEST_ID_HEADER).count(), 1);
}

/// `emits_no_response_header_to_the_caller` (`inst-ps-rid-5`).
#[tokio::test]
async fn emits_no_response_header_to_the_caller() {
    let plugin = RequestIdTransformPlugin;
    let mut response = ResponseContext {
        status: 200,
        headers: vec![("x-request-id".to_owned(), "echoed".to_owned())],
        ..ResponseContext::default()
    };
    plugin.transform_response(&mut response).await.expect("the transform runs");
    assert!(
        !response.headers.iter().any(|(name, _)| name == REQUEST_ID_HEADER),
        "an upstream echo is dropped, never returned to the caller"
    );
    // The error phase contributes nothing either.
    plugin
        .transform_error(&mut ErrorContext::default())
        .await
        .expect("the error transform runs");
}

/// The plugin is the only writer of the header, and a second pass in the same
/// chain does not duplicate it.
#[tokio::test]
async fn the_write_is_idempotent() {
    let plugin = RequestIdTransformPlugin;
    let mut ctx = context(vec![], vec![]);
    plugin.transform_request(&mut ctx).await.expect("the transform runs");
    let first = request_id_of(&ctx).expect("written").to_owned();
    plugin.transform_request(&mut ctx).await.expect("the transform runs");
    assert_eq!(request_id_of(&ctx), Some(first.as_str()));
    assert_eq!(ctx.headers.iter().filter(|(name, _)| name == REQUEST_ID_HEADER).count(), 1);
}

/// The configuration of the plugin binding is never read: the plugin carries
/// no configuration surface at all.
#[test]
fn the_plugin_has_no_configuration_surface() {
    let ctx = RequestContext { config: Some(json!({ "anything": true })), ..RequestContext::default() };
    assert_eq!(RequestIdTransformPlugin::resolve(&ctx).len(), 36);
}
