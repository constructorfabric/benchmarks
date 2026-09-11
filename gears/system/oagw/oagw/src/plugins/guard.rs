//! Built-in guard plugins.
//!
//! Guards are assertions the gateway makes about a request or an upstream's answer. They
//! change nothing: a guard either accepts what it is given or rejects it with a canonical
//! error. Guards run after the auth plugins, so a header a credential plugin is about to
//! inject is not one a guard can demand of the caller.

use http::HeaderMap;

use crate::error::{ErrorKind, OagwError};
use crate::plugins::PluginContext;

/// Guard plugins the gateway binds and executes.
pub const BINDABLE_GUARD_PLUGINS: [&str; 1] = ["required_headers"];

/// The binding key naming the headers a request must carry (ADR-0009).
pub const REQUEST_HEADER_KEY: &str = "required_request_headers";

/// The binding key naming the headers an upstream's response must carry (ADR-0009).
pub const RESPONSE_HEADER_KEY: &str = "required_response_headers";

/// The request-phase alias the guard accepted before the ADR's keys were implemented.
const REQUEST_HEADER_ALIAS: &str = "headers";

/// Runs the request-side guards.
///
/// # Errors
///
/// Returns a validation error when a guard rejects the outbound request.
pub fn execute_request(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
    headers: &HeaderMap,
) -> Result<(), OagwError> {
    match plugin.name.as_str() {
        "required_headers" => require_headers(plugin, ctx, headers, REQUEST_HEADER_KEY),
        other => Err(unknown_guard(ctx, other)),
    }
}

/// Runs the response-side guards.
///
/// FR-017 makes the response half of `required_headers` as much a guard as the request
/// half: an upstream that omits a header it was configured to always send is misbehaving,
/// and its answer is refused with a `502` naming the gateway rather than relayed.
///
/// # Errors
///
/// Returns a downstream error when a guard rejects the upstream's answer, and a
/// plugin error when a guard names an unknown identifier.
pub fn execute_response(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
    headers: &HeaderMap,
) -> Result<(), OagwError> {
    match plugin.name.as_str() {
        "required_headers" => require_headers(plugin, ctx, headers, RESPONSE_HEADER_KEY),
        other => Err(unknown_guard(ctx, other)),
    }
}

fn unknown_guard(ctx: &PluginContext<'_>, name: &str) -> OagwError {
    OagwError::new(
        ErrorKind::PluginNotFound,
        format!("unknown guard plugin `{name}`"),
    )
    .with_extensions(ctx.extensions())
}

/// `required_headers`: every named header must be present and non-empty on the subject.
///
/// The names come from the ADR-0009 key `phase_key`; the request phase also accepts the
/// `headers` alias. A missing name is reported as the phase's own error kind — a
/// validation error for a request the gateway refuses to send, a downstream error for an
/// answer the gateway refuses to hand back — and only the first missing name is named.
fn require_headers(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
    headers: &HeaderMap,
    key: &str,
) -> Result<(), OagwError> {
    let kind = if key == RESPONSE_HEADER_KEY {
        ErrorKind::DownstreamError
    } else {
        ErrorKind::ValidationError
    };
    for name in required_names(&plugin.config, key) {
        let missing = headers.get(name.as_str()).is_none_or(http::HeaderValue::is_empty);
        if missing {
            return Err(OagwError::new(
                kind,
                format!("required header `{name}` is missing"),
            )
            .with_extensions(ctx.extensions()));
        }
    }
    Ok(())
}

/// Reads the header names a guard binding demands, per ADR-0009.
///
/// The key's value is a comma-separated list: each entry is trimmed, lowercased and
/// dropped when empty, so `", ,X-Correlation-Id"` names one header and a blank value names
/// none. The request-phase `headers` alias is read too, as either a list or a single bare
/// string, so a definition written in that shape still enforces something rather than
/// silently failing open.
fn required_names(config: &serde_json::Map<String, serde_json::Value>, key: &str) -> Vec<String> {
    let mut names = match config.get(key) {
        Some(serde_json::Value::String(list)) => split_names(list),
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .flat_map(split_names)
            .collect(),
        _ => Vec::new(),
    };
    if key == REQUEST_HEADER_KEY {
        match config.get(REQUEST_HEADER_ALIAS) {
            Some(serde_json::Value::Array(values)) => {
                names.extend(values.iter().filter_map(serde_json::Value::as_str).map(str::to_owned));
            }
            Some(serde_json::Value::String(name)) => names.push(name.clone()),
            _ => {}
        }
    }
    names
}

/// Splits a comma-separated header list into normalised, non-empty names.
fn split_names(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}
