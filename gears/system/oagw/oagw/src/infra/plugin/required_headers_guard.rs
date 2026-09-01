//! `RequiredHeadersGuardPlugin` (ADR-0009).
//!
//! Config keys: `required_request_headers`, `required_response_headers` —
//! comma-separated header name lists, matched case-insensitively, presence
//! only. An absent or blank key makes that phase a no-op. The first missing
//! header is reported; a missing request header is a 400, a missing response
//! header is a 502.

use crate::domain::error::DomainError;
use crate::domain::gts::GUARD_PLUGIN_REQUIRED_HEADERS_INSTANCE;
use crate::domain::plugin::{GuardPlugin, PluginContext};
use async_trait::async_trait;

/// Statelessness is the point: the plugin carries no data.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

/// Splits and normalizes a comma-separated header list.
#[must_use]
pub fn parse_header_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn first_missing(required: &[String], headers: &http::HeaderMap) -> Option<String> {
    required.iter().find_map(|name| {
        http::HeaderName::from_bytes(name.as_bytes())
            .map(|candidate| !headers.contains_key(&candidate))
            .unwrap_or(true)
            .then(|| name.clone())
    })
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        GUARD_PLUGIN_REQUIRED_HEADERS_INSTANCE
    }

    async fn guard_request(
        &self,
        _ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &http::request::Parts,
    ) -> Result<(), DomainError> {
        let required =
            parse_header_list(super::config_str(config, "required_request_headers").as_deref());
        if required.is_empty() {
            return Ok(());
        }
        match first_missing(&required, &parts.headers) {
            Some(name) => Err(DomainError::Validation(format!(
                "required header '{name}' is missing"
            ))),
            None => Ok(()),
        }
    }

    async fn guard_response(
        &self,
        _ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &http::response::Parts,
    ) -> Result<(), DomainError> {
        let required =
            parse_header_list(super::config_str(config, "required_response_headers").as_deref());
        if required.is_empty() {
            return Ok(());
        }
        match first_missing(&required, &parts.headers) {
            Some(name) => Err(DomainError::DownstreamError(format!(
                "the upstream response is missing the required header '{name}'"
            ))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use toolkit_security::SecurityContext;

    fn request_context() -> PluginContext {
        PluginContext {
            security_context: SecurityContext::anonymous(),
            upstream_id: uuid::Uuid::nil(),
            host: "vendor.com".to_owned(),
            route_id: None,
            endpoint_host: "api.vendor.com:443".to_owned(),
            request_id: PluginContext::default_request_id(),
        }
    }

    #[test]
    fn parses_and_normalizes_lists() {
        assert_eq!(
            parse_header_list(Some(" X-Corr-Id ,  accept ,, ")),
            vec!["x-corr-id".to_owned(), "accept".to_owned()]
        );
        assert!(parse_header_list(None).is_empty());
        assert!(parse_header_list(Some(" , ")).is_empty());
    }

    #[tokio::test]
    async fn unconfigured_phase_is_a_noop() {
        let request = http::Request::builder().body(()).unwrap();
        let (parts, _) = request.into_parts();
        RequiredHeadersGuardPlugin
            .guard_request(&request_context(), &serde_json::json!({}), &parts)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_request_header_is_400() {
        let request = http::Request::builder().body(()).unwrap();
        let (parts, _) = request.into_parts();
        let error = RequiredHeadersGuardPlugin
            .guard_request(
                &request_context(),
                &serde_json::json!({"required_request_headers": "x-correlation-id"}),
                &parts,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, DomainError::Validation(_)));
        assert_eq!(error.status(), 400);
    }

    #[tokio::test]
    async fn header_presence_is_case_insensitive() {
        let request = http::Request::builder()
            .header("X-CORRELATION-ID", "abc")
            .body(())
            .unwrap();
        let (parts, _) = request.into_parts();
        RequiredHeadersGuardPlugin
            .guard_request(
                &request_context(),
                &serde_json::json!({"required_request_headers": "x-correlation-id"}),
                &parts,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_response_header_is_502() {
        let response = http::Response::builder().body(()).unwrap();
        let (parts, _) = response.into_parts();
        let error = RequiredHeadersGuardPlugin
            .guard_response(
                &request_context(),
                &serde_json::json!({"required_response_headers": "content-type"}),
                &parts,
            )
            .await
            .unwrap_err();
        assert_eq!(error.status(), 502);
    }
}
