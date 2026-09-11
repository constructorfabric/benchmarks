//! Required-headers guard plugin (ADR 0009).
//!
//! Checks configured header names for presence in the request or response
//! phase, rejecting with a phase-specific status on the first missing one and
//! failing open when unconfigured.

use super::{GuardDecision, GuardPlugin, PluginRequestContext, PluginResponseContext, config_list};
use crate::domain::error::OagwError;
use crate::gts_helpers;
use async_trait::async_trait;

/// Configuration key for the request-phase header list.
pub const REQUIRED_REQUEST_HEADERS: &str = "required_request_headers";
/// Configuration key for the response-phase header list.
pub const REQUIRED_RESPONSE_HEADERS: &str = "required_response_headers";
/// Rejection reason carried in the guard decision.
pub const MISSING_HEADER_CODE: &str = "REQUIRED_HEADER_MISSING";

/// Built-in guard plugin enforcing header presence.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        gts_helpers::GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(
        &self,
        context: &PluginRequestContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, OagwError> {
        Ok(check(
            &context.headers,
            &config_list(config, REQUIRED_REQUEST_HEADERS),
            400,
        ))
    }

    async fn guard_response(
        &self,
        context: &PluginResponseContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, OagwError> {
        Ok(check(
            &context.headers,
            &config_list(config, REQUIRED_RESPONSE_HEADERS),
            502,
        ))
    }
}

fn check(headers: &http::HeaderMap, required: &[String], status: u16) -> GuardDecision {
    for name in required {
        let present = headers.get_all(name.as_str()).iter().next().is_some();
        if !present {
            return GuardDecision::Reject {
                status,
                code: MISSING_HEADER_CODE,
                detail: format!("required header '{name}' is missing"),
            };
        }
    }
    GuardDecision::Allow
}

#[cfg(test)]
#[path = "required_headers_guard_tests.rs"]
mod tests;
