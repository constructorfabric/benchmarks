//! Built-in guard plugin: `required_headers` (`docs/ADR/0009`).
use async_trait::async_trait;

use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PROTOCOL_ERROR, PluginError, RequestContext, ResponseContext,
    VALIDATION, problem_type,
};

/// GTS type id of the built-in `required_headers` guard plugin.
pub const REQUIRED_HEADERS_PLUGIN_TYPE: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// Rejects the hop when a configured header is absent.
///
/// Effective configuration keys (both independent, both optional):
///
/// | Key | Phase |
/// |---|---|
/// | `required_request_headers` | `guard_request` |
/// | `required_response_headers` | `guard_response` |
///
/// Either key is a comma-separated list of header names, matched
/// case-insensitively on presence only. A blank key is a no-op (fail-open),
/// and only the first missing header is reported.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    /// Split and normalise a configured header list.
    fn required(value: Option<&str>) -> Vec<String> {
        value
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// First configured header the message does not carry.
    ///
    /// A configured name that is not a legal `HeaderName` can never travel on
    /// the wire, so it is treated as satisfied — the same fail-open posture as
    /// a blank key — rather than wedging the route forever.
    fn missing<'a>(required: &'a [String], headers: &'a http::HeaderMap) -> Option<&'a str> {
        required
            .iter()
            .find(|name| {
                http::HeaderName::try_from(name.as_str())
                    .map(|header_name| !headers.contains_key(header_name))
                    .unwrap_or(false)
            })
            .map(String::as_str)
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        REQUIRED_HEADERS_PLUGIN_TYPE
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = Self::required(ctx.config_str("required_request_headers"));
        let Some(missing) = Self::missing(&required, &ctx.headers) else {
            return Ok(GuardDecision::allow());
        };
        Ok(GuardDecision::reject(
            400,
            problem_type(VALIDATION),
            format!("required request header '{missing}' is absent"),
        ))
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = Self::required(ctx.config_str("required_response_headers"));
        let Some(missing) = Self::missing(&required, &ctx.headers) else {
            return Ok(GuardDecision::allow());
        };
        Ok(GuardDecision::reject(
            502,
            problem_type(PROTOCOL_ERROR),
            format!("upstream response is missing required header '{missing}'"),
        ))
    }
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
