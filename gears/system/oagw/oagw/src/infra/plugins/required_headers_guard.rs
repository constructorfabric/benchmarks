//! `required_headers` guard plugin (`ADR/0009`).
//!
//! Configuration keys (`ctx.config`), both optional and independent:
//!
//! | Key | Phase | Missing header |
//! |---|---|---|
//! | `required_request_headers` | `guard_request` | `400` |
//! | `required_response_headers` | `guard_response` | `502` |
//!
//! Values are comma-separated header names; entries are trimmed and matched
//! case-insensitively. Unconfigured → the phase is a no-op (fail-open). Only
//! the first missing header is reported.

use async_trait::async_trait;
use http::HeaderMap;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginResult, RequestContext, ResponseContext,
};

/// Machine-readable code carried in the rejection detail (`ADR/0009`).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Parses a comma-separated header-name list into lowercase names.
fn parse_names(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Reads one config key as a comma-separated list.
fn configured(config: &serde_json::Value, key: &str) -> Vec<String> {
    match config.get(key) {
        Some(serde_json::Value::String(raw)) => parse_names(Some(raw)),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .flat_map(|entry| parse_names(Some(entry)))
            .collect(),
        _ => Vec::new(),
    }
}

/// `true` when the header is present, compared case-insensitively.
fn present(headers: &HeaderMap, name: &str) -> bool {
    http::HeaderName::try_from(name)
        .map(|name| headers.contains_key(name))
        .unwrap_or_else(|_| {
            headers
                .keys()
                .any(|existing| existing.as_str().eq_ignore_ascii_case(name))
        })
}

/// Stateless required-header enforcement.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision> {
        let required = configured(&ctx.config, "required_request_headers");
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        for name in required {
            if !present(&ctx.headers, &name) {
                return Ok(GuardDecision::Reject(DomainError::Validation(format!(
                    "{REQUIRED_HEADER_MISSING}: request header '{name}' is absent"
                ))));
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision> {
        let required = configured(&ctx.config, "required_response_headers");
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        for name in required {
            if !present(&ctx.headers, &name) {
                return Ok(GuardDecision::Reject(DomainError::ProtocolError(format!(
                    "{REQUIRED_HEADER_MISSING}: response header '{name}' is missing"
                ))));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(config: serde_json::Value, headers: &[(&str, &str)]) -> RequestContext {
        RequestContext {
            method: http::Method::GET,
            path: "/".to_owned(),
            query: String::new(),
            headers: {
                let mut map = HeaderMap::new();
                for (name, value) in headers {
                    map.insert(
                        http::HeaderName::try_from(*name).expect("name"),
                        http::HeaderValue::try_from(*value).expect("value"),
                    );
                }
                map
            },
            body: bytes::Bytes::new(),
            request_id: None,
            config,
            runtime: crate::domain::plugin::test_runtime(),
        }
    }

    #[tokio::test]
    async fn an_unconfigured_guard_allows_the_request() {
        let plugin = RequiredHeadersGuardPlugin;
        let ctx = request(serde_json::json!({}), &[]);
        assert!(plugin.guard_request(&ctx).await.expect("allow").is_allow());
    }

    #[tokio::test]
    async fn a_missing_request_header_is_a_400() {
        let plugin = RequiredHeadersGuardPlugin;
        let ctx = request(
            serde_json::json!({"required_request_headers": "X-Request-Id, X-Tenant"}),
            &[("x-request-id", "abc")],
        );
        let decision = plugin.guard_request(&ctx).await.expect("decision");
        let reject = match decision {
            GuardDecision::Reject(err) => err,
            GuardDecision::Allow => panic!("expected a rejection"),
        };
        assert_eq!(reject.status(), 400);
        assert!(reject.to_string().contains(REQUIRED_HEADER_MISSING));
        assert!(reject.to_string().contains("x-tenant"));
    }

    #[tokio::test]
    async fn header_names_are_matched_case_insensitively() {
        let plugin = RequiredHeadersGuardPlugin;
        let ctx = request(
            serde_json::json!({"required_request_headers": "X-API-Version"}),
            &[("x-api-version", "2024-01-01")],
        );
        assert!(plugin.guard_request(&ctx).await.expect("allow").is_allow());
    }

    #[tokio::test]
    async fn a_missing_response_header_is_a_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let ctx = crate::domain::plugin::test_response(serde_json::json!({
            "required_response_headers": "Content-Type"
        }));
        let decision = plugin.guard_response(&ctx).await.expect("decision");
        let reject = match decision {
            GuardDecision::Reject(err) => err,
            GuardDecision::Allow => panic!("expected a rejection"),
        };
        assert_eq!(reject.status(), 502);
    }

    #[tokio::test]
    async fn a_present_response_header_allows() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = crate::domain::plugin::test_response(serde_json::json!({
            "required_response_headers": "Content-Type"
        }));
        ctx.headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        assert!(plugin.guard_response(&ctx).await.expect("allow").is_allow());
    }
}
