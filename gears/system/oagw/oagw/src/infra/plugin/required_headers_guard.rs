//! `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`
//! (ADR-0009).
//!
//! Stateless presence check over comma-separated header names. Both phases are
//! independently configurable and fail open when unconfigured.

use async_trait::async_trait;
use http::HeaderMap;

use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginResult, RequestContext, ResponseContext,
};

pub const ERROR_CODE: &str = "REQUIRED_HEADER_MISSING";

pub struct RequiredHeadersGuardPlugin;

/// Split on `,`, trim, lowercase, drop empties.
#[must_use]
pub fn parse_required(raw: Option<&str>) -> Vec<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// The first configured header absent from `headers`, if any.
#[must_use]
pub fn first_missing(headers: &HeaderMap, required: &[String]) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.keys().any(|k| k.as_str() == name.as_str()))
        .cloned()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision> {
        let required = parse_required(ctx.config_str("required_request_headers"));
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        Ok(first_missing(&ctx.headers, &required).map_or(GuardDecision::Allow, |name| {
            GuardDecision::reject(400, ERROR_CODE, format!("Required header missing: {name}"))
        }))
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision> {
        let required = parse_required(ctx.config_str("required_response_headers"));
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        Ok(first_missing(&ctx.headers, &required).map_or(GuardDecision::Allow, |name| {
            GuardDecision::reject(
                502,
                ERROR_CODE,
                format!("Required response header missing: {name}"),
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn blank_and_malformed_config_no_ops() {
        assert!(parse_required(None).is_empty());
        assert!(parse_required(Some("")).is_empty());
        assert!(parse_required(Some(", , ,")).is_empty());
    }

    #[test]
    fn names_are_normalised_and_matched_case_insensitively() {
        let required = parse_required(Some("X-Correlation-Id, Accept"));
        assert_eq!(required, vec!["x-correlation-id", "accept"]);

        let mut headers = HeaderMap::new();
        headers.insert("x-correlation-id", HeaderValue::from_static("abc"));
        assert_eq!(first_missing(&headers, &required).as_deref(), Some("accept"));

        headers.insert("accept", HeaderValue::from_static("*/*"));
        assert_eq!(first_missing(&headers, &required), None);
    }

    #[test]
    fn only_the_first_missing_header_is_reported() {
        let required = parse_required(Some("a,b,c"));
        let headers = HeaderMap::new();
        assert_eq!(first_missing(&headers, &required).as_deref(), Some("a"));
    }
}
