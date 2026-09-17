//! `RequiredHeadersGuardPlugin` — request/response header presence enforcement
//! (ADR-0009).
//!
//! Configuration (`ctx.config` keys):
//!
//! | Key | Required | Description |
//! |---|---|---|
//! | `required_request_headers` | no | Comma-separated header names checked in `guard_request` |
//! | `required_response_headers` | no | Comma-separated header names checked in `guard_response` |
//!
//! Absent or blank configuration makes the phase a no-op (fail-open). Only the
//! first missing header is reported per rejection. Request phase rejects with
//! 400; response phase rejects with 502.

use async_trait::async_trait;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{GuardDecision, GuardPlugin, RequestContext, ResponseContext};

/// Stateless presence check for configured header names.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

/// Parses a comma-separated header list: split, trim, lowercase, drop empties.
#[must_use]
pub fn parse_header_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> OagwResult<GuardDecision> {
        let list = parse_header_list(
            ctx.config
                .get("required_request_headers")
                .and_then(serde_json::Value::as_str),
        );
        if list.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        for name in &list {
            if !ctx.headers.contains_key(name.as_str()) {
                return Ok(GuardDecision::Reject(OagwError::Validation(format!(
                    "required request header '{name}' is missing"
                ))));
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> OagwResult<GuardDecision> {
        let list = parse_header_list(
            ctx.config
                .get("required_response_headers")
                .and_then(serde_json::Value::as_str),
        );
        if list.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        for name in &list {
            if !ctx.headers.contains_key(name.as_str()) {
                return Ok(GuardDecision::Reject(OagwError::DownstreamError(format!(
                    "required response header '{name}' is missing from the upstream response"
                ))));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::request_context;
    use bytes::Bytes;

    fn response_context(headers: http::HeaderMap, config: serde_json::Value) -> ResponseContext {
        ResponseContext {
            status: http::StatusCode::OK,
            headers,
            body: Bytes::new(),
            is_error: false,
            config,
        }
    }

    #[tokio::test]
    async fn unconfigured_is_fail_open() {
        let mut ctx = request_context();
        let decision = RequiredHeadersGuardPlugin.guard_request(&ctx).await.unwrap();
        assert!(decision.is_allowed());

        ctx.config = serde_json::json!({"required_request_headers": "  ,  , "});
        let decision = RequiredHeadersGuardPlugin.guard_request(&ctx).await.unwrap();
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn missing_request_header_rejects_with_400() {
        let mut ctx = request_context();
        ctx.config = serde_json::json!({"required_request_headers": "x-correlation-id,accept"});
        ctx.headers.insert("x-correlation-id", http::HeaderValue::from_static("abc"));
        let decision = RequiredHeadersGuardPlugin.guard_request(&ctx).await.unwrap();
        match decision {
            GuardDecision::Reject(err) => {
                assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
                assert!(err.detail().contains("accept"));
            }
            GuardDecision::Allow => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn all_present_is_allowed() {
        let mut ctx = request_context();
        ctx.config = serde_json::json!({"required_request_headers": "X-Correlation-ID"});
        ctx.headers.insert("x-correlation-id", http::HeaderValue::from_static("abc"));
        let decision = RequiredHeadersGuardPlugin.guard_request(&ctx).await.unwrap();
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn missing_response_header_rejects_with_502() {
        let ctx = response_context(http::HeaderMap::new(), serde_json::json!({"required_response_headers": "content-type"}));
        let decision = RequiredHeadersGuardPlugin.guard_response(&ctx).await.unwrap();
        match decision {
            GuardDecision::Reject(err) => {
                assert_eq!(err.status(), http::StatusCode::BAD_GATEWAY);
            }
            GuardDecision::Allow => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn present_response_header_is_allowed() {
        let mut headers = http::HeaderMap::new();
        headers.insert("Content-Type", http::HeaderValue::from_static("application/json"));
        let ctx = response_context(
            headers,
            serde_json::json!({"required_response_headers": "content-type"}),
        );
        let decision = RequiredHeadersGuardPlugin.guard_response(&ctx).await.unwrap();
        assert!(decision.is_allowed());
    }

    #[test]
    fn parses_comma_separated_lists() {
        assert_eq!(
            parse_header_list(Some(" X-A , ,x-b,")),
            vec!["x-a".to_owned(), "x-b".to_owned()]
        );
        assert!(parse_header_list(None).is_empty());
    }
}
