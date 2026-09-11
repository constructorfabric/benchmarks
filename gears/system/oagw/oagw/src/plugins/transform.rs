//! Built-in transform plugins.
//!
//! Transforms are the only plugins that edit the request or the response. They run after
//! the auth plugins so a transform observes the credential header a caller smuggled in,
//! and their edits are applied in binding order with later plugins seeing earlier edits.

use http::HeaderMap;

use crate::error::{ErrorKind, OagwError};
use crate::plugins::PluginContext;

/// Transform plugins the gateway binds and executes.
pub const BINDABLE_TRANSFORM_PLUGINS: [&str; 1] = ["request_id"];

/// The header the `request_id` transform propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Applies the request-side transforms.
///
/// # Errors
///
/// Returns an unknown-plugin error when a binding names something else.
pub fn execute_request(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
    headers: &mut HeaderMap,
) -> Result<(), OagwError> {
    match plugin.name.as_str() {
        "request_id" => {
            let value = headers
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok())
                .map_or_else(generate_request_id, str::to_owned);
            set_header(headers, REQUEST_ID_HEADER, &value);
            Ok(())
        }
        other => Err(unknown(ctx, other)),
    }
}

/// Applies the response-side transforms.
///
/// # Errors
///
/// Returns an unknown-plugin error when a binding names something else.
pub fn execute_response(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
    headers: &mut HeaderMap,
) -> Result<(), OagwError> {
    match plugin.name.as_str() {
        // The response carries the identifier the request was given; the upstream's own
        // value is replaced so the caller always sees the id the gateway can trace. An
        // empty context identifier means the caller sent none and nothing generated one,
        // so there is nothing to hand back and no header is added (FR-018).
        "request_id" => {
            if !ctx.request_id.is_empty() {
                set_header(headers, REQUEST_ID_HEADER, ctx.request_id);
            }
            Ok(())
        }
        other => Err(unknown(ctx, other)),
    }
}

fn unknown(ctx: &PluginContext<'_>, name: &str) -> OagwError {
    OagwError::new(
        ErrorKind::PluginNotFound,
        format!("unknown transform plugin `{name}`"),
    )
    .with_extensions(ctx.extensions())
}

/// Sets a header, replacing any existing values.
fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let (Ok(name), Ok(value)) = (
        http::HeaderName::from_bytes(name.as_bytes()),
        http::HeaderValue::from_str(value),
    ) else {
        return;
    };
    headers.insert(name, value);
}

/// Generates a request identifier when the caller did not supply one.
fn generate_request_id() -> String {
    format!("req_{}", uuid::Uuid::new_v4().simple())
}
