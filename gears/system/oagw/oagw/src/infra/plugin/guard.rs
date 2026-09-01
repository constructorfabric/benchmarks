// Created: 2026-08-29 by Constructor Tech
//! `cf.core.oagw.required_headers.v1` — presence-only header guard.
//!
//! Fail-open by design (DESIGN §9): an absent or blank configuration allows
//! every request; a present but unmet requirement is a hard rejection.

use async_trait::async_trait;

use crate::domain::plugin::{
    GUARD_REQUIRED_HEADERS, GuardDecision, GuardPlugin, PluginError, RequestContext,
    ResponseContext,
};
use crate::infra::plugin::auth::config_list;

/// Guard that requires named headers on the request and/or the response.
///
/// The guard is stateless: the requirement comes from the plugin binding
/// configuration handed to the request / response context, so one registered
/// instance serves every upstream.
#[derive(Debug, Clone, Default)]
pub struct RequiredHeadersGuard;

impl RequiredHeadersGuard {
    fn id_str() -> &'static str {
        GUARD_REQUIRED_HEADERS
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuard {
    fn id(&self) -> &str {
        Self::id_str()
    }

    fn plugin_type(&self) -> &str {
        "guard_plugin"
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = config_list(&ctx.config, "required_request_headers");
        Ok(match first_missing(&required, &ctx.headers) {
            Some(missing) => GuardDecision::Reject {
                status: axum::http::StatusCode::BAD_REQUEST,
                error_code: "REQUIRED_HEADER_MISSING".to_owned(),
                message: format!("required request header '{missing}' is absent"),
            },
            None => GuardDecision::Allow,
        })
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = config_list(&ctx.config, "required_response_headers");
        Ok(match first_missing(&required, &ctx.headers) {
            Some(missing) => GuardDecision::Reject {
                status: axum::http::StatusCode::BAD_GATEWAY,
                error_code: "REQUIRED_HEADER_MISSING".to_owned(),
                message: format!("required response header '{missing}' is absent"),
            },
            None => GuardDecision::Allow,
        })
    }
}

/// First configured header name absent from `headers`, case-insensitively.
///
/// Unparsable names can never be sent by a well-formed client, so they are
/// treated as missing rather than ignored.
fn first_missing(required: &[String], headers: &axum::http::HeaderMap) -> Option<String> {
    required
        .iter()
        .find(|name| {
            axum::http::HeaderName::from_bytes(name.as_bytes())
                .is_ok_and(|normalized| !headers.contains_key(normalized))
        })
        .cloned()
}

/// `PluginError` helper used when the guard cannot be evaluated at all.
#[must_use]
pub fn guard_error(message: &str) -> PluginError {
    PluginError::Internal(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(items: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in items {
            let parsed = axum::http::HeaderName::from_bytes(name.as_bytes()).expect("name");
            map.insert(
                parsed,
                axum::http::HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    fn context(headers: axum::http::HeaderMap, config: serde_json::Value) -> RequestContext {
        RequestContext {
            tenant_id: uuid::Uuid::nil(),
            upstream_id: uuid::Uuid::nil(),
            alias: "a.example.com".to_owned(),
            method: "GET".to_owned(),
            path: "/".to_owned(),
            query: None,
            headers,
            body: bytes::Bytes::new(),
            uri: axum::http::Uri::from_static("/"),
            config: config.as_object().cloned().unwrap_or_default(),
            security: toolkit_security::SecurityContext::anonymous(),
        }
    }

    #[tokio::test]
    async fn request_phase_rejects_missing_header() {
        let guard = RequiredHeadersGuard;
        let ctx = context(
            headers(&[("content-type", "application/json")]),
            serde_json::json!({ "required_request_headers": "x-trace-id" }),
        );
        assert!(matches!(
            guard.guard_request(&ctx).await,
            Ok(GuardDecision::Reject {
                status: axum::http::StatusCode::BAD_REQUEST,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn request_phase_allows_present_header() {
        let guard = RequiredHeadersGuard;
        let ctx = context(
            headers(&[("X-Trace-Id", "abc")]),
            serde_json::json!({ "required_request_headers": "x-trace-id" }),
        );
        assert_eq!(guard.guard_request(&ctx).await, Ok(GuardDecision::Allow));
    }

    #[tokio::test]
    async fn fail_open_when_no_headers_required() {
        let guard = RequiredHeadersGuard;
        let ctx = context(axum::http::HeaderMap::new(), serde_json::json!({}));
        assert_eq!(guard.guard_request(&ctx).await, Ok(GuardDecision::Allow));
    }

    #[test]
    fn guard_error_is_internal() {
        assert!(matches!(guard_error("x"), PluginError::Internal(_)));
    }
}
