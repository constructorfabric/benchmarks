//! `cf.core.oagw.required_headers.v1` — presence enforcement on request and
//! response headers (`ADR/0009-required-headers-guard-plugin.md`).
//!
//! Both phases are independent and fail open when unconfigured, so adding the
//! plugin to the registry changes nothing for upstreams that do not opt in.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, HeaderBag, PluginResult, RequestContext, ResponseContext,
};

/// Error code reported on rejection, in both phases.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Stateless presence check for configured header names.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

/// Split a comma-separated config value into lowercased, non-empty names.
fn parse_names(raw: Option<&str>) -> Vec<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// First configured name absent from `headers`, in configuration order.
fn first_missing(headers: &HeaderBag, required: &[String]) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.contains(name))
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
        let required = parse_names(ctx.config_str("required_request_headers"));
        match first_missing(&ctx.headers, &required) {
            None => Ok(GuardDecision::Allow),
            Some(name) => Ok(GuardDecision::Reject(
                DomainError::validation(format!("Required request header '{name}' is missing"))
                    .with_extension("error_code", REQUIRED_HEADER_MISSING),
            )),
        }
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision> {
        let required = parse_names(ctx.config_str("required_response_headers"));
        match first_missing(&ctx.headers, &required) {
            None => Ok(GuardDecision::Allow),
            Some(name) => Ok(GuardDecision::Reject(
                DomainError::downstream_error(format!(
                    "Required response header '{name}' is missing"
                ))
                .with_extension("error_code", REQUIRED_HEADER_MISSING),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::{request_context, response_context};
    use serde_json::json;

    fn config(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[tokio::test]
    async fn unconfigured_phases_are_no_ops() {
        let ctx = request_context(serde_json::Map::new());
        assert!(matches!(
            RequiredHeadersGuardPlugin
                .guard_request(&ctx)
                .await
                .expect("guard"),
            GuardDecision::Allow
        ));

        let blank = request_context(config(json!({ "required_request_headers": ", , ," })));
        assert!(matches!(
            RequiredHeadersGuardPlugin
                .guard_request(&blank)
                .await
                .expect("guard"),
            GuardDecision::Allow
        ));
    }

    #[tokio::test]
    async fn request_phase_rejects_with_400() {
        let mut ctx = request_context(config(json!({
            "required_request_headers": "x-correlation-id,accept"
        })));
        ctx.headers.set("Accept", "application/json");
        let decision = RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect("guard");
        match decision {
            GuardDecision::Reject(err) => {
                assert_eq!(err.status(), 400);
                assert!(err.detail().contains("x-correlation-id"));
            }
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn response_phase_rejects_with_502() {
        let ctx = response_context(config(json!({ "required_response_headers": "content-type" })));
        let decision = RequiredHeadersGuardPlugin
            .guard_response(&ctx)
            .await
            .expect("guard");
        match decision {
            GuardDecision::Reject(err) => assert_eq!(err.status(), 502),
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn matching_is_case_insensitive_and_reports_only_the_first_gap() {
        let mut ctx = request_context(config(json!({
            "required_request_headers": "X-Correlation-ID, X-Trace"
        })));
        ctx.headers.set("x-correlation-id", "abc");
        let decision = RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect("guard");
        match decision {
            GuardDecision::Reject(err) => {
                assert!(err.detail().contains("x-trace"));
                assert!(!err.detail().contains("x-correlation-id"));
            }
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn the_two_phases_are_independent() {
        let ctx = request_context(config(json!({ "required_response_headers": "content-type" })));
        assert!(matches!(
            RequiredHeadersGuardPlugin
                .guard_request(&ctx)
                .await
                .expect("guard"),
            GuardDecision::Allow
        ));
    }
}
