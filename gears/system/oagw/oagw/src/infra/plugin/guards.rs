//! Built-in guard and transform plugins (ADR 0009, ADR 0002).

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::builtins::{GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID};
use crate::domain::plugin::{
    ErrorContext, GuardDecision, GuardPlugin, RequestContext, ResponseContext, TransformPlugin,
};

/// Registry key of the required-headers guard.
pub const REQUIRED_HEADERS_PLUGIN_ID: &str = "required_headers";
/// Registry key of the request-id transform.
pub const REQUEST_ID_PLUGIN_ID: &str = "request_id";
/// Header the request-id transform propagates or generates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Reads a comma-separated configuration entry as a list of header names.
///
/// Mirrors ADR 0009: entries are split on `,`, trimmed, lowercased and empty
/// entries dropped. An absent or blank key yields an empty list, which makes
/// the phase a no-op (fail-open).
fn required_names(config: &serde_json::Value, key: &str) -> Vec<String> {
    let Some(raw) = config.get(key).and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Reports the first header of `names` missing from `headers`.
fn first_missing(headers: &[(String, String)], names: &[String]) -> Option<String> {
    names.iter().find(|name| {
        !headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    }).cloned()
}

/// Enforces the presence of configured request/response headers (ADR 0009).
///
/// Review evidence (privilege boundary — policy enforcement):
/// * Guardrail: ADR 0009 "Decision Flow" — an absent or blank configuration is
///   a no-op, only the *first* missing header is reported and header *values*
///   are never inspected or echoed.
/// * Rationale: failing closed on an unconfigured guard would reject every
///   request of every upstream that merely lists the built-in in its plugin
///   chain; failing open on a *configured* missing header would silently
///   disable the contract the upstream relies on.
/// * Validation performed: `required_headers_*` tests in
///   `infra/plugin/tests.rs` cover the request 400, the response 502, the
///   fail-open default and the case-insensitive lookup.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        REQUIRED_HEADERS_PLUGIN_ID
    }

    fn plugin_type(&self) -> &'static str {
        GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, DomainError> {
        let names = required_names(&ctx.config, "required_request_headers");
        if names.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        match first_missing(&ctx.headers, &names) {
            Some(missing) => Ok(GuardDecision::Reject(DomainError::ValidationError {
                detail: format!("required request header '{missing}' is missing"),
                invalid_value: Some(missing),
                alias: None,
            })),
            None => Ok(GuardDecision::Allow),
        }
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, DomainError> {
        let names = required_names(&ctx.config, "required_response_headers");
        if names.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        match first_missing(&ctx.headers, &names) {
            Some(missing) => Ok(GuardDecision::Reject(DomainError::ProtocolError {
                detail: format!("upstream response is missing the required header '{missing}'"),
                upstream_id: None,
                host: None,
            })),
            None => Ok(GuardDecision::Allow),
        }
    }
}

/// Propagates `X-Request-ID` (ADR 0002 "Built-in Plugins").
///
/// An inbound request id is forwarded verbatim; a request without one gets a
/// freshly generated identifier, so the upstream always sees a correlation id.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        REQUEST_ID_PLUGIN_ID
    }

    fn plugin_type(&self) -> &'static str {
        TRANSFORM_REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let existing = ctx
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(REQUEST_ID_HEADER))
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), |(_, value)| value.clone());
        ctx.set_outbound_header(REQUEST_ID_HEADER, existing);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), DomainError> {
        let _ = ctx;
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), DomainError> {
        let _ = ctx;
        Ok(())
    }
}
