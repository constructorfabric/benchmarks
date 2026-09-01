//! `required_headers` guard plugin — request/response header presence
//! enforcement (ADR-0009).
//!
//! Configuration (`ctx.config`):
//! - `required_request_headers`: comma-separated header names checked in
//!   `guard_request` (case-insensitive).
//! - `required_response_headers`: comma-separated header names checked in
//!   `guard_response`.
//!
//! Each phase is a no-op when its key is absent or blank (fail-open when
//! unconfigured). Missing headers reject the exchange with
//! `REQUIRED_HEADER_MISSING` (ADR-0009): request phase status 400, response
//! phase status 502. Only the first missing header is reported.

use serde::Deserialize;

use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, PluginResult, RequestContext, ResponseContext,
    async_trait,
};

/// Parsed configuration for the required-headers guard.
#[derive(Debug, Deserialize, Default)]
struct RequiredHeadersConfig {
    required_request_headers: Option<String>,
    required_response_headers: Option<String>,
}

fn parse_list(value: Option<&String>) -> Vec<String> {
    value
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn missing(headers: &http::HeaderMap, required: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|name| {
            !headers
                .keys()
                .any(|k| k.as_str().eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect()
}

/// The `required_headers` guard plugin.
#[derive(Debug, Clone, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait declares `&str`; returns a literal
    fn plugin_type(&self) -> &str {
        "required_headers"
    }

    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision> {
        let cfg: RequiredHeadersConfig =
            serde_json::from_value(serde_json::Value::Object(ctx.config.clone())).map_err(|e| {
                PluginError::config(format!("invalid required_headers config: {e}"))
            })?;
        let required = parse_list(cfg.required_request_headers.as_ref());
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        let missing = missing(&ctx.headers, &required);
        if missing.is_empty() {
            Ok(GuardDecision::Allow)
        } else {
            // ADR-0009: only the first missing header is reported.
            Ok(GuardDecision::Reject {
                status: 400,
                code: "REQUIRED_HEADER_MISSING",
                detail: format!("missing required request header '{}'", missing[0]),
            })
        }
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision> {
        let cfg: RequiredHeadersConfig =
            serde_json::from_value(serde_json::Value::Object(ctx.config.clone())).map_err(|e| {
                PluginError::config(format!("invalid required_headers config: {e}"))
            })?;
        let required = parse_list(cfg.required_response_headers.as_ref());
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        let missing = missing(&ctx.headers, &required);
        if missing.is_empty() {
            Ok(GuardDecision::Allow)
        } else {
            // ADR-0009: response-phase rejection is a downstream (502) error
            // with the same `REQUIRED_HEADER_MISSING` code; only the first
            // missing header is reported.
            Ok(GuardDecision::Reject {
                status: 502,
                code: "REQUIRED_HEADER_MISSING",
                detail: format!("missing required response header '{}'", missing[0]),
            })
        }
    }
}
