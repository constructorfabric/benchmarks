//! The `required_headers` guard plugin (ADR 0009).
//!
//! Presence-only, case-insensitive checks over the request and the response,
//! independently configurable and fail-open when unconfigured.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::BUILTIN_GUARD_REQUIRED_HEADERS;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
};

/// Parses a comma-separated header list, lower-cased, empty entries dropped.
pub fn parse_header_list(value: Option<&str>) -> Vec<String> {
    value
        .map(|raw| {
            raw.split(',')
                .map(|entry| entry.trim().to_ascii_lowercase())
                .filter(|entry| !entry.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn required_request(ctx: &RequestContext) -> Vec<String> {
    parse_header_list(
        ctx.plugin_config
            .as_ref()
            .and_then(|c| c.get("required_request_headers"))
            .and_then(|v| v.as_str()),
    )
}

fn required_response(ctx: &ResponseContext) -> Vec<String> {
    parse_header_list(
        ctx.plugin_config
            .as_ref()
            .and_then(|c| c.get("required_response_headers"))
            .and_then(|v| v.as_str()),
    )
}

/// The required-header guard.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuard;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuard {
    fn id(&self) -> &str {
        BUILTIN_GUARD_REQUIRED_HEADERS
    }

    fn plugin_type(&self) -> &str {
        crate::domain::gts_helpers::GUARD_PLUGIN_TYPE
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        for name in required_request(ctx) {
            if ctx.header(&name).is_none() {
                return Ok(GuardDecision::Reject(DomainError::ValidationError {
                    detail: format!("required header `{name}` is missing"),
                }));
            }
        }
        Ok(GuardDecision::Next)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        for name in required_response(ctx) {
            if ctx.header(&name).is_none() {
                // The upstream omitted the header, so the failure belongs to
                // the upstream side of the gateway (502), not to the caller.
                return Err(PluginError::Reject(DomainError::DownstreamError {
                    status: 502,
                    host: None,
                }));
            }
        }
        Ok(GuardDecision::Next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_context(config: serde_json::Value, headers: &[(&str, &str)]) -> RequestContext {
        let mut ctx = RequestContext::default();
        ctx.plugin_config = Some(config);
        for (name, value) in headers {
            ctx.set_header(*name, *value);
        }
        ctx
    }

    #[tokio::test]
    async fn an_unconfigured_plugin_is_a_no_op() {
        let mut ctx = RequestContext::default();
        assert!(matches!(
            RequiredHeadersGuard.guard_request(&ctx).await.unwrap(),
            GuardDecision::Next
        ));
        ctx.plugin_config = Some(serde_json::json!({
            "required_request_headers": "  ,  "
        }));
        assert!(matches!(
            RequiredHeadersGuard.guard_request(&ctx).await.unwrap(),
            GuardDecision::Next
        ));
    }

    #[tokio::test]
    async fn a_missing_request_header_is_rejected_with_400() {
        let ctx = request_context(
            serde_json::json!({ "required_request_headers": "x-correlation-id,accept" }),
            &[("x-correlation-id", "abc")],
        );
        let decision = RequiredHeadersGuard.guard_request(&ctx).await.unwrap();
        match decision {
            GuardDecision::Reject(DomainError::ValidationError { detail }) => {
                assert!(detail.contains("accept"), "{detail}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn headers_match_case_insensitively() {
        let ctx = request_context(
            serde_json::json!({ "required_request_headers": "X-CORRELATION-ID" }),
            &[("x-correlation-id", "abc")],
        );
        assert!(matches!(
            RequiredHeadersGuard.guard_request(&ctx).await.unwrap(),
            GuardDecision::Next
        ));
    }

    #[tokio::test]
    async fn a_missing_response_header_is_rejected_with_502() {
        let mut ctx = ResponseContext::default();
        ctx.plugin_config = Some(serde_json::json!({ "required_response_headers": "content-type" }));
        let err = RequiredHeadersGuard.guard_response(&ctx).await.unwrap_err();
        match err {
            PluginError::Reject(DomainError::DownstreamError { status, .. }) => {
                assert_eq!(status, 502);
            }
            other => panic!("expected a 502 rejection, got {other:?}"),
        }
        ctx.set_header("content-type", "application/json");
        assert!(RequiredHeadersGuard.guard_response(&ctx).await.is_ok());
    }

    #[test]
    fn the_list_parser_trims_lowercases_and_drops_empties() {
        assert_eq!(
            parse_header_list(Some(" A , b,C ")),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(parse_header_list(None).is_empty());
    }
}
