//! `required_headers` guard plugin (ADR-0009).

use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
};

/// Guard plugin enforcing header presence on request and response (ADR-0009).
pub struct RequiredHeadersGuardPlugin;

/// Config key holding the comma-separated request header names.
pub const REQUEST_KEY: &str = "required_request_headers";
/// Config key holding the comma-separated response header names.
pub const RESPONSE_KEY: &str = "required_response_headers";

#[async_trait::async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        crate::ids::GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = configured(&ctx.config, REQUEST_KEY);
        if required.is_empty() {
            return Ok(GuardDecision::allow());
        }
        for name in required {
            if ctx.headers.get(&name).is_none() {
                return Ok(GuardDecision::reject(
                    400,
                    "REQUIRED_HEADER_MISSING",
                    format!("required header `{name}` is missing"),
                ));
            }
        }
        Ok(GuardDecision::allow())
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = configured(&ctx.config, RESPONSE_KEY);
        if required.is_empty() {
            return Ok(GuardDecision::allow());
        }
        for name in required {
            if ctx.headers.get(&name).is_none() {
                return Ok(GuardDecision::reject(
                    502,
                    "REQUIRED_HEADER_MISSING",
                    format!("required response header `{name}` is missing"),
                ));
            }
        }
        Ok(GuardDecision::allow())
    }
}

/// Parse a comma-separated header list, dropping blanks (ADR-0009).
///
/// An absent or all-blank value yields an empty list, which makes the phase a
/// no-op (fail-open).
#[must_use]
pub fn configured(config: &serde_json::Value, key: &str) -> Vec<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "required_headers_guard_tests.rs"]
mod required_headers_guard_tests;
