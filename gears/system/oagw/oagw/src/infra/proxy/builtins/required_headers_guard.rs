//! The `required_headers` guard plugin (ADR-0009).
//!
//! Configuration:
//!
//! ```json
//! { "required_request_headers": "X-Tenant-Id,X-Api-Version",
//!   "required_response_headers": "X-Request-Id" }
//! ```
//!
//! Checks are presence-only and case-insensitive. A missing request header is
//! rejected with `400 REQUIRED_HEADER_MISSING`; a missing response header is
//! rejected with `502 REQUIRED_HEADER_MISSING`. Absent or blank configuration
//! is a no-op (fail-open).

use async_trait::async_trait;
use axum::http::HeaderMap;

use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
};

const INSTANCE: &str = "cf.core.oagw.required_headers.v1";
const PLUGIN_TYPE: &str = "cf.core.oagw.guard_plugin.v1";
const ERROR_CODE: &str = "REQUIRED_HEADER_MISSING";

/// Guard plugin that enforces required header presence.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

fn configured(config: &serde_json::Value, key: &str) -> Vec<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn missing(headers: &HeaderMap, required: &[String]) -> Option<String> {
    required
        .iter()
        .find(|name| {
            headers
                .get(name.trim().to_ascii_lowercase().as_str())
                .is_none_or(|value| value.to_str().is_ok_and(|text| text.trim().is_empty()))
        })
        .map(|name| name.trim().to_owned())
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        INSTANCE
    }

    fn plugin_type(&self) -> &str {
        PLUGIN_TYPE
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = configured(&ctx.config, "required_request_headers");
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        if let Some(name) = missing(&ctx.headers, &required) {
            return Ok(GuardDecision::Reject {
                status: 400,
                error_code: ERROR_CODE.to_owned(),
                detail: format!("required request header `{name}` is missing or empty"),
            });
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = configured(&ctx.config, "required_response_headers");
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        if let Some(name) = missing(&ctx.headers, &required) {
            return Ok(GuardDecision::Reject {
                status: 502,
                error_code: ERROR_CODE.to_owned(),
                detail: format!("upstream response is missing required header `{name}`"),
            });
        }
        Ok(GuardDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use bytes::Bytes;
    use uuid::Uuid;

    fn request_ctx(config: serde_json::Value, headers: &[(&str, &str)]) -> RequestContext {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                axum::http::header::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        RequestContext {
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            query: String::new(),
            headers: map,
            body: Bytes::new(),
            config,
            attributes: Default::default(),
        }
    }

    fn response_ctx(config: serde_json::Value, headers: &[(&str, &str)]) -> ResponseContext {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                axum::http::header::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        ResponseContext {
            status: 200,
            headers: map,
            body: Bytes::new(),
            config,
        }
    }

    #[tokio::test]
    async fn a_missing_request_header_is_rejected_with_400() {
        let plugin = RequiredHeadersGuardPlugin;
        let decision = plugin
            .guard_request(&request_ctx(
                serde_json::json!({ "required_request_headers": "X-Tenant-Id, x-api-version" }),
                &[("x-tenant-id", "acme")],
            ))
            .await
            .expect("guard ran");
        match decision {
            GuardDecision::Reject {
                status,
                error_code,
                detail,
            } => {
                assert_eq!(status, 400);
                assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
                assert!(detail.contains("x-api-version"), "{detail}");
            }
            GuardDecision::Allow => panic!("must reject"),
        }
    }

    #[tokio::test]
    async fn present_and_case_insensitive_headers_pass() {
        let plugin = RequiredHeadersGuardPlugin;
        let decision = plugin
            .guard_request(&request_ctx(
                serde_json::json!({ "required_request_headers": "X-Tenant-Id" }),
                &[("x-tenant-id", "acme")],
            ))
            .await
            .expect("guard ran");
        assert!(decision.is_allow());
    }

    #[tokio::test]
    async fn a_blank_header_counts_as_missing() {
        let plugin = RequiredHeadersGuardPlugin;
        let decision = plugin
            .guard_request(&request_ctx(
                serde_json::json!({ "required_request_headers": "X-Tenant-Id" }),
                &[("x-tenant-id", "   ")],
            ))
            .await
            .expect("guard ran");
        assert!(matches!(
            decision,
            GuardDecision::Reject { status: 400, .. }
        ));
    }

    #[tokio::test]
    async fn a_missing_response_header_is_rejected_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let decision = plugin
            .guard_response(&response_ctx(
                serde_json::json!({ "required_response_headers": "X-Request-Id" }),
                &[("server", "mock")],
            ))
            .await
            .expect("guard ran");
        assert!(matches!(
            decision,
            GuardDecision::Reject { status: 502, .. }
        ));
    }

    #[tokio::test]
    async fn blank_configuration_is_a_no_op() {
        let plugin = RequiredHeadersGuardPlugin;
        for config in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({ "required_request_headers": "  " }),
        ] {
            let decision = plugin
                .guard_request(&request_ctx(config, &[]))
                .await
                .expect("guard ran");
            assert!(decision.is_allow());
        }
    }
}
